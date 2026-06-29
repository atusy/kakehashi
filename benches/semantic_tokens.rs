//! Semantic-tokens A/B performance benchmark.
//!
//! Compares the semantic-tokens hot path of two `kakehashi` server binaries —
//! typically the current `HEAD` against `origin/main` — across several
//! scenarios, each chosen to exercise a specific performance improvement made
//! on this branch.
//!
//! ## Why drive the binary over LSP
//!
//! The semantic-tokens code is `pub(crate)`, so a `benches/` crate cannot call
//! it directly. Driving the compiled binary over the LSP protocol instead means
//! ONE harness (compiled from HEAD) can benchmark ANY binary, selected at
//! runtime via an env var — so we never need this file to exist on `origin/main`.
//! Both binaries read the same parser/query data dir, so only their code differs.
//!
//! ## Usage
//!
//! The harness must be built with `--features e2e` so it can reach
//! `install::test_support` for parser/query setup. The benchmarked server
//! binaries are built separately and do NOT need that feature.
//!
//! Single binary (absolute timings):
//! ```sh
//! cargo build --release --bin kakehashi
//! KAKEHASHI_BENCH_BIN=target/release/kakehashi \
//!   cargo bench --bench semantic_tokens --features e2e
//! ```
//!
//! A/B comparison (the common case — see benches/compare_head_vs_main.sh):
//! ```sh
//! KAKEHASHI_BENCH_BIN_A=/tmp/kakehashi-main KAKEHASHI_BENCH_LABEL_A=main \
//! KAKEHASHI_BENCH_BIN_B=/tmp/kakehashi-head KAKEHASHI_BENCH_LABEL_B=head \
//!   cargo bench --bench semantic_tokens --features e2e
//! ```
//!
//! Tunables (env): `KAKEHASHI_BENCH_ITERS` (default 80),
//! `KAKEHASHI_BENCH_WARMUP` (default 10).

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

// ───────────────────────────── Minimal LSP client ─────────────────────────────

/// A spawned `kakehashi` server process plus the JSON-RPC plumbing needed to
/// initialize it and request semantic tokens. Intentionally minimal — just the
/// subset of `tests/helpers/lsp_client.rs` the benchmark needs (benches cannot
/// import test helpers).
struct Server {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
}

impl Server {
    /// Spawn `bin`, run the LSP handshake, and return a ready server.
    fn start(bin: &str, data_dir: &str) -> Server {
        let mut child = Command::new(bin)
            .env("KAKEHASHI_DATA_DIR", data_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap_or_else(|e| panic!("failed to spawn server binary {bin:?}: {e}"));

        let stdin = child.stdin.take().expect("stdin");
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));
        let mut server = Server {
            child,
            stdin,
            stdout,
            next_id: 0,
        };

        server.request(
            "initialize",
            json!({
                "processId": std::process::id(),
                "rootUri": null,
                "capabilities": {
                    "textDocument": {
                        "semanticTokens": {
                            "dynamicRegistration": false,
                            "requests": { "full": { "delta": true } },
                            "tokenTypes": [],
                            "tokenModifiers": [],
                            "formats": ["relative"]
                        }
                    }
                }
            }),
        );
        server.notify("initialized", json!({}));
        server
    }

    fn did_open(&mut self, uri: &str, language_id: &str, text: &str) {
        self.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": language_id,
                    "version": 1,
                    "text": text,
                }
            }),
        );
    }

    /// `semanticTokens/full`; returns the response's `result` object.
    fn semantic_full(&mut self, uri: &str) -> Value {
        self.request(
            "textDocument/semanticTokens/full",
            json!({ "textDocument": { "uri": uri } }),
        )
    }

    /// `semanticTokens/full/delta` against `previous_result_id`.
    fn semantic_delta(&mut self, uri: &str, previous_result_id: &str) -> Value {
        self.request(
            "textDocument/semanticTokens/full/delta",
            json!({
                "textDocument": { "uri": uri },
                "previousResultId": previous_result_id,
            }),
        )
    }

    /// Incremental `didChange` that toggles a single space at the start of
    /// `line`: `insert` adds it, `!insert` removes it. Because the char at
    /// column 0 after an insert is always that space, the delete restores the
    /// document to its exact original bytes — so a type/untype pair round-trips
    /// and keeps token count stable across iterations.
    fn did_change_toggle(&mut self, uri: &str, version: i64, line: u32, insert: bool) {
        let change = if insert {
            json!({
                "range": {
                    "start": { "line": line, "character": 0 },
                    "end": { "line": line, "character": 0 },
                },
                "text": " ",
            })
        } else {
            json!({
                "range": {
                    "start": { "line": line, "character": 0 },
                    "end": { "line": line, "character": 1 },
                },
                "text": "",
            })
        };
        self.notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": uri, "version": version },
                "contentChanges": [change],
            }),
        );
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }));
        self.recv_response(id)
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.send(&json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }));
    }

    fn send(&mut self, msg: &Value) {
        let body = serde_json::to_string(msg).unwrap();
        write!(self.stdin, "Content-Length: {}\r\n\r\n{}", body.len(), body).unwrap();
        self.stdin.flush().unwrap();
    }

    /// Read messages until the response for `id` arrives, skipping
    /// notifications and server-to-client requests.
    fn recv_response(&mut self, id: i64) -> Value {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if Instant::now() > deadline {
                panic!("timed out waiting for response id={id}");
            }
            let msg = self.recv_message();
            // Server-to-client requests carry a "method"; skip them.
            if msg.get("method").is_some() {
                continue;
            }
            if msg.get("id").and_then(Value::as_i64) == Some(id) {
                // Fail fast on a JSON-RPC error: a misconfigured server (missing
                // parser/query, bad setup) must not silently produce Null and
                // bogus timings.
                if let Some(error) = msg.get("error") {
                    panic!("server returned a JSON-RPC error for request id={id}: {error}");
                }
                return msg.get("result").cloned().unwrap_or(Value::Null);
            }
        }
    }

    fn recv_message(&mut self) -> Value {
        // Parse Content-Length framing.
        let mut content_length = None;
        loop {
            let mut line = String::new();
            if self.stdout.read_line(&mut line).unwrap() == 0 {
                panic!("server closed stdout unexpectedly");
            }
            let trimmed = line.trim_end();
            if trimmed.is_empty() {
                break; // end of headers
            }
            if let Some(v) = trimmed.strip_prefix("Content-Length:") {
                content_length = Some(v.trim().parse::<usize>().expect("content-length"));
            }
        }
        let len = content_length.expect("missing Content-Length");
        let mut buf = vec![0u8; len];
        self.stdout.read_exact(&mut buf).unwrap();
        serde_json::from_slice(&buf).expect("valid json body")
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // Best-effort shutdown; kill if it doesn't exit promptly.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ───────────────────────────── Document generators ────────────────────────────

/// Dense, all-ASCII Rust source: keywords, types, strings, numbers, comments,
/// functions, and locals. Exercises the host-only token path.
fn gen_rust(funcs: usize) -> String {
    let mut s = String::with_capacity(funcs * 400);
    s.push_str("use std::collections::HashMap;\nuse std::fmt;\n\n");
    for i in 0..funcs {
        s.push_str(&format!(
            "/// Documentation comment for function number {i}.\n\
             pub fn function_{i}(arg_a: i32, arg_b: &str, items: &[u64]) -> Result<String, String> {{\n\
            \x20   let local_total: i64 = arg_a as i64 * 2 + {i};\n\
            \x20   let mut lookup: HashMap<String, u64> = HashMap::new();\n\
            \x20   for (index, value) in items.iter().enumerate() {{\n\
            \x20       lookup.insert(format!(\"key_{{}}\", index), *value);\n\
            \x20   }}\n\
            \x20   let message = format!(\"total is {{}} for {{}}\", local_total, arg_b);\n\
            \x20   if local_total > 100 && !items.is_empty() {{\n\
            \x20       return Err(String::from(\"value too large\"));\n\
            \x20   }}\n\
            \x20   match arg_b {{\n\
            \x20       \"alpha\" => Ok(message),\n\
            \x20       \"beta\" => Ok(String::from(\"the beta branch\")),\n\
            \x20       _ => Ok(message.clone()),\n\
            \x20   }}\n\
             }}\n\n"
        ));
    }
    s
}

/// Markdown with many fenced code blocks in rust/lua/python — each block is a
/// separate injection region. Exercises the injection pipeline: included-range
/// computation, active-region detection, and host/injection coordinate mapping.
fn gen_markdown_injections(blocks: usize) -> String {
    let mut s = String::with_capacity(blocks * 300);
    s.push_str("# Benchmark Document\n\nAn introductory paragraph with some **bold** and `inline code`.\n\n");
    for i in 0..blocks {
        s.push_str(&format!(
            "## Section {i}\n\nProse describing the code in section {i} before the fence.\n\n"
        ));
        match i % 3 {
            0 => s.push_str(&format!(
                "```rust\n\
                 fn rust_block_{i}(x: i32) -> i32 {{\n\
                \x20   let doubled = x * 2; // a comment\n\
                \x20   let label = \"section {i}\";\n\
                \x20   println!(\"{{}} {{}}\", label, doubled);\n\
                \x20   doubled + {i}\n\
                 }}\n```\n\n"
            )),
            1 => s.push_str(&format!(
                "```lua\n\
                 local function lua_block_{i}(x)\n\
                \x20   local doubled = x * 2 -- a comment\n\
                \x20   local label = \"section {i}\"\n\
                \x20   print(label, doubled)\n\
                \x20   return doubled + {i}\n\
                 end\n```\n\n"
            )),
            _ => s.push_str(&format!(
                "```python\n\
                 def python_block_{i}(x):\n\
                \x20   doubled = x * 2  # a comment\n\
                \x20   label = \"section {i}\"\n\
                \x20   print(label, doubled)\n\
                \x20   return doubled + {i}\n```\n\n"
            )),
        }
    }
    s
}

/// Rust source whose comments and string literals are full of multi-byte UTF-8,
/// forcing the non-ASCII branch of byte→UTF-16 column conversion on most lines.
fn gen_unicode_rust(funcs: usize) -> String {
    let mut s = String::with_capacity(funcs * 300);
    for i in 0..funcs {
        s.push_str(&format!(
            "/// 関数番号 {i} のドキュメントコメント — 日本語の説明文です。\n\
             pub fn 関数_{i}(引数: i32) -> String {{\n\
            \x20   let メッセージ = \"こんにちは世界、ベンチマーク {i} 番\";\n\
            \x20   let emoji = \"🚀✨🎯 milestone {i}\";\n\
            \x20   format!(\"{{}} {{}} {{}}\", メッセージ, emoji, 引数)\n\
             }}\n\n"
        ));
    }
    s
}

/// Rust source dense with constant-like (ALL_CAPS) and type-like identifiers.
/// The rust highlight query runs `#lua-match?` predicates per identifier (e.g.
/// `^[A-Z][A-Z0-9_]+$` for constants, `^[A-Z]` for constructors), so a high
/// identifier density maximizes predicate evaluations — the path that the shared
/// lua-match regex cache pool optimizes.
fn gen_rust_predicate_heavy(groups: usize) -> String {
    let mut s = String::with_capacity(groups * 320);
    for i in 0..groups {
        s.push_str(&format!(
            "pub const MAX_VALUE_{i}: u64 = {i};\n\
             pub const MIN_LIMIT_{i}: u64 = {i};\n\
             pub const DEFAULT_NAME_{i}: &str = \"item_{i}\";\n\
             pub static GLOBAL_COUNTER_{i}: u64 = {i};\n\
             pub fn compute_{i}(InputValue: u64, OtherArg: u64) -> u64 {{\n\
            \x20   let LocalResult = MAX_VALUE_{i} + MIN_LIMIT_{i} + InputValue;\n\
            \x20   let AnotherName = GLOBAL_COUNTER_{i} * OtherArg + LocalResult;\n\
            \x20   AnotherName.max(MAX_VALUE_{i})\n\
             }}\n\n"
        ));
    }
    s
}

// ─────────────────────────────── Statistics ───────────────────────────────────

struct Stats {
    median: Duration,
    p25: Duration,
    p75: Duration,
}

fn summarize(mut samples: Vec<Duration>) -> Stats {
    samples.sort_unstable();
    let pick = |q: f64| samples[((samples.len() as f64 * q) as usize).min(samples.len() - 1)];
    Stats {
        median: pick(0.50),
        p25: pick(0.25),
        p75: pick(0.75),
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

// ───────────────────────────── Scenario harness ───────────────────────────────

#[derive(Clone, Copy)]
enum Kind {
    /// `semanticTokens/full` on an unchanging document.
    Full,
    /// `semanticTokens/full/delta` with no edit between requests.
    DeltaNoop,
    /// Realistic editing: a toggle `didChange` then `semanticTokens/full/delta`,
    /// so each measured request pays incremental reparse + cache invalidation +
    /// delta diff on top of the (full) token recompute.
    EditDelta,
    /// Cold-open latency: each iteration opens a FRESH document and times
    /// `didOpen` → first `semanticTokens/full` response — the editor-visible
    /// "open the file, see highlights" latency. The one scenario that captures
    /// the open parse itself (no settle sleep), so it is what validates the
    /// off-ingress open flip (#6) against the reader's watermark settle budget.
    OpenFirstToken,
}

/// Mutable per-run state for [`Kind::EditDelta`]: the next `didChange` version,
/// whether the next edit inserts (vs deletes) — flips each iteration to toggle —
/// and the line edited.
struct EditState {
    version: i64,
    insert_next: bool,
    line: u32,
}

struct Scenario {
    name: &'static str,
    language_id: &'static str,
    uri: &'static str,
    content: String,
    kind: Kind,
    /// Which branch commits this scenario is designed to exercise.
    targets: &'static str,
}

struct Binary {
    label: String,
    path: String,
}

/// Run one scenario against one binary and return per-request timing samples.
/// `iters` requests are measured after `warmup` unmeasured ones.
fn measure(
    bin: &Binary,
    scn: &Scenario,
    data_dir: &str,
    iters: usize,
    warmup: usize,
) -> Vec<Duration> {
    if let Kind::OpenFirstToken = scn.kind {
        return measure_open(bin, scn, data_dir, iters, warmup);
    }
    let mut server = Server::start(&bin.path, data_dir);
    server.did_open(scn.uri, scn.language_id, &scn.content);
    // Let the initial parse settle (didOpen parse may be async).
    std::thread::sleep(Duration::from_millis(300));

    // For delta/edit scenarios we need a valid previous result_id; seed it from a
    // full request, then keep it current (a no-op delta may or may not rotate).
    let mut prev_result_id = seed_result_id(&mut server, scn);

    // did_open used version 1; edits continue from there. Edit a line in the
    // middle of the document (representative of typical cursor position).
    let mut edit = EditState {
        version: 1,
        // First edit must insert (the document opens with no extra space).
        insert_next: true,
        line: (scn.content.lines().count() / 2) as u32,
    };

    for _ in 0..warmup {
        run_once(&mut server, scn, &mut prev_result_id, &mut edit);
    }

    let mut samples = Vec::with_capacity(iters);
    for _ in 0..iters {
        let start = Instant::now();
        run_once(&mut server, scn, &mut prev_result_id, &mut edit);
        samples.push(start.elapsed());
    }
    samples
}

/// Cold-open latency loop for [`Kind::OpenFirstToken`]. Each iteration opens a
/// FRESH document (unique URI, so it is a genuine cold open rather than a reopen)
/// and times `didOpen` → first `semanticTokens/full` response. No settle sleep —
/// capturing the open parse is the whole point: this is the path the off-ingress
/// flip (#6) changes from "gated by the ingress barrier (always the full parse)"
/// to "gated by the reader's watermark settle budget, then an on-demand fallback
/// parse if it times out". The language is driven by `language_id` (detected
/// before the path's now-mangled extension), so the per-iteration URI suffix is
/// harmless.
fn measure_open(
    bin: &Binary,
    scn: &Scenario,
    data_dir: &str,
    iters: usize,
    warmup: usize,
) -> Vec<Duration> {
    let mut server = Server::start(&bin.path, data_dir);
    let mut samples = Vec::with_capacity(iters);
    for i in 0..(warmup + iters) {
        let uri = format!("{}_{i}", scn.uri);
        let start = Instant::now();
        server.did_open(&uri, scn.language_id, &scn.content);
        // Blocks until the server answers — i.e. until the (off-ingress) open parse
        // has produced a tree the reader can tokenize, or the reader fell back to an
        // on-demand parse.
        let _ = server.semantic_full(&uri);
        let elapsed = start.elapsed();
        if i >= warmup {
            samples.push(elapsed);
        }
    }
    samples
}

fn seed_result_id(server: &mut Server, scn: &Scenario) -> String {
    match scn.kind {
        Kind::Full | Kind::OpenFirstToken => String::new(),
        // Both delta-based scenarios need an initial result_id to diff against.
        Kind::DeltaNoop | Kind::EditDelta => {
            result_id_of(&server.semantic_full(scn.uri)).unwrap_or_default()
        }
    }
}

fn run_once(
    server: &mut Server,
    scn: &Scenario,
    prev_result_id: &mut String,
    edit: &mut EditState,
) {
    match scn.kind {
        // Handled by `measure_open`, which never calls `run_once`.
        Kind::OpenFirstToken => unreachable!("OpenFirstToken uses measure_open"),
        Kind::Full => {
            let _ = server.semantic_full(scn.uri);
        }
        Kind::DeltaNoop => {
            let result = server.semantic_delta(scn.uri, prev_result_id);
            // Keep the id valid for the next request whether or not it rotated.
            if let Some(id) = result_id_of(&result) {
                *prev_result_id = id;
            }
        }
        Kind::EditDelta => {
            // Apply one toggle edit, then request a delta against the prior id.
            let insert = edit.insert_next;
            edit.insert_next = !edit.insert_next;
            edit.version += 1;
            server.did_change_toggle(scn.uri, edit.version, edit.line, insert);
            // Fall back to a full request if we don't yet have a valid id
            // (e.g. a prior delta was cancelled and returned no result).
            let result = if prev_result_id.is_empty() {
                server.semantic_full(scn.uri)
            } else {
                server.semantic_delta(scn.uri, prev_result_id)
            };
            if let Some(id) = result_id_of(&result) {
                *prev_result_id = id;
            }
        }
    }
}

fn result_id_of(result: &Value) -> Option<String> {
    result
        .get("resultId")
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn main() {
    // Ensure parsers/queries exist, and reuse the test data dir both binaries read.
    let data_dir: PathBuf = kakehashi::install::test_support::test_data_dir_path();
    std::fs::create_dir_all(&data_dir).expect("create data dir");
    kakehashi::install::test_support::ensure_test_languages_installed(&data_dir)
        .expect("install test languages");
    let data_dir = data_dir.to_string_lossy().to_string();

    let iters = env_usize("KAKEHASHI_BENCH_ITERS", 80);
    let warmup = env_usize("KAKEHASHI_BENCH_WARMUP", 10);

    let binaries = resolve_binaries();

    let scenarios = vec![
        Scenario {
            name: "rust_small/full",
            language_id: "rust",
            uri: "file:///bench/small.rs",
            content: gen_rust(15),
            kind: Kind::Full,
            targets: "token-index resolution, Arc mappings, lazy filter_captures",
        },
        Scenario {
            name: "rust_large/full",
            language_id: "rust",
            uri: "file:///bench/large.rs",
            content: gen_rust(150),
            kind: Kind::Full,
            targets: "per-token String removal, ASCII fast-path, Arc mappings (amplified)",
        },
        Scenario {
            name: "rust_predicate_heavy/full",
            language_id: "rust",
            uri: "file:///bench/predicates.rs",
            content: gen_rust_predicate_heavy(120),
            kind: Kind::Full,
            targets: "#lua-match? predicate evaluation — shared regex lazy-DFA cache pool",
        },
        Scenario {
            name: "markdown_injections/full",
            language_id: "markdown",
            uri: "file:///bench/injections.md",
            content: gen_markdown_injections(60),
            kind: Kind::Full,
            targets: "active-region binary search, line/col index, host_lines sharing",
        },
        Scenario {
            name: "markdown_injections_large/full",
            language_id: "markdown",
            uri: "file:///bench/injections_large.md",
            content: gen_markdown_injections(150),
            kind: Kind::Full,
            targets: "injection pipeline at scale (amplifies region/coord work)",
        },
        Scenario {
            name: "unicode_rust/full",
            language_id: "rust",
            uri: "file:///bench/unicode.rs",
            content: gen_unicode_rust(150),
            kind: Kind::Full,
            targets: "byte→UTF-16 conversion (non-ASCII fallback) + token path",
        },
        Scenario {
            name: "rust_large/delta_noop",
            language_id: "rust",
            uri: "file:///bench/large_delta.rs",
            content: gen_rust(150),
            kind: Kind::DeltaNoop,
            targets: "no-op delta result_id reuse",
        },
        Scenario {
            name: "rust_large/edit_delta",
            language_id: "rust",
            uri: "file:///bench/large_edit.rs",
            content: gen_rust(150),
            kind: Kind::EditDelta,
            targets: "edit→reparse→retokenize→delta diff (host path under typing)",
        },
        Scenario {
            name: "markdown_injections/edit_delta",
            language_id: "markdown",
            uri: "file:///bench/injections_edit.md",
            content: gen_markdown_injections(60),
            kind: Kind::EditDelta,
            targets: "edit→reparse→injection re-detect→cache invalidation→delta (typing)",
        },
        // Cold-open latency scenarios for the #6 off-ingress flip. The reader gates
        // on the **host parse** via the watermark (≤200ms budget), then falls back to
        // an on-demand parse; the expensive injection work runs off the reader's
        // critical path. So the regime that risks a regression is a doc whose *host
        // parse* alone exceeds 200ms — that is dense host-language source, NOT an
        // injection-heavy doc (whose host parse is small; its cost is injections,
        // off-path). Empirically: markdown stays well under budget at any realistic
        // size; rust crosses ~200ms host-parse only around ~4000 dense functions
        // (~150KB), where the on-demand fallback fires every open — and even there
        // HEAD ≈ main, because token computation dominates and the redundant parse
        // overlaps the off-ingress one.
        Scenario {
            name: "markdown_injections/open_first_token",
            language_id: "markdown",
            uri: "file:///bench/open_md.md",
            content: gen_markdown_injections(150),
            kind: Kind::OpenFirstToken,
            targets: "cold open; injection work moved off the reader path (HEAD faster)",
        },
        Scenario {
            name: "rust_large/open_first_token (host parse under budget)",
            language_id: "rust",
            uri: "file:///bench/open_rust_under.rs",
            content: gen_rust(1500),
            kind: Kind::OpenFirstToken,
            targets: "cold open; host parse stays under the 200ms budget — no fallback",
        },
        Scenario {
            name: "rust_xlarge/open_first_token (host parse OVER budget — fallback fires)",
            language_id: "rust",
            uri: "file:///bench/open_rust_over.rs",
            content: gen_rust(4000),
            kind: Kind::OpenFirstToken,
            targets: "cold open whose host parse exceeds 200ms — the on-demand fallback \
                      regime; validates no latency regression there (slow; use low iters)",
        },
    ];

    println!();
    println!("semantic-tokens benchmark  (iters={iters}, warmup={warmup}, lower is better)");
    for b in &binaries {
        println!("  {} = {}", b.label, b.path);
    }
    println!();

    if binaries.len() == 2 {
        print_ab_header(&binaries[0].label, &binaries[1].label);
        for scn in &scenarios {
            // Interleave A and B at the scenario level (separate processes); the
            // two runs are back-to-back to minimize machine drift between them.
            let a = summarize(measure(&binaries[0], scn, &data_dir, iters, warmup));
            let b = summarize(measure(&binaries[1], scn, &data_dir, iters, warmup));
            print_ab_row(scn, &a, &b);
        }
    } else {
        print_single_header();
        for scn in &scenarios {
            let s = summarize(measure(&binaries[0], scn, &data_dir, iters, warmup));
            print_single_row(scn, &s);
        }
    }
    println!();
}

// ───────────────────────────── Output formatting ──────────────────────────────

fn print_single_header() {
    println!(
        "{:<34} {:>10} {:>10} {:>10}",
        "scenario", "median", "p25", "p75"
    );
    println!("{}", "-".repeat(68));
}

fn print_single_row(scn: &Scenario, s: &Stats) {
    println!(
        "{:<34} {:>9.3}ms {:>9.3}ms {:>9.3}ms",
        scn.name,
        ms(s.median),
        ms(s.p25),
        ms(s.p75)
    );
    println!("    └ targets: {}", scn.targets);
}

fn print_ab_header(label_a: &str, label_b: &str) {
    println!(
        "{:<34} {:>12} {:>12} {:>10}",
        "scenario",
        format!("{label_a} (med)"),
        format!("{label_b} (med)"),
        "Δ (B vs A)"
    );
    println!("{}", "-".repeat(72));
}

fn print_ab_row(scn: &Scenario, a: &Stats, b: &Stats) {
    let am = ms(a.median);
    let bm = ms(b.median);
    let delta_pct = if am > 0.0 {
        (bm - am) / am * 100.0
    } else {
        0.0
    };
    let sign = if delta_pct <= 0.0 { "" } else { "+" };
    println!(
        "{:<34} {:>10.3}ms {:>10.3}ms {:>8}{:.1}%",
        scn.name, am, bm, sign, delta_pct
    );
    println!("    └ targets: {}", scn.targets);
}

// ──────────────────────────────── Env helpers ─────────────────────────────────

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Resolve which binaries to benchmark from the environment.
/// A/B mode if both `_A` and `_B` are set; otherwise single-binary mode.
fn resolve_binaries() -> Vec<Binary> {
    let a = std::env::var("KAKEHASHI_BENCH_BIN_A").ok();
    let b = std::env::var("KAKEHASHI_BENCH_BIN_B").ok();
    if let (Some(a), Some(b)) = (a, b) {
        return vec![
            Binary {
                label: std::env::var("KAKEHASHI_BENCH_LABEL_A").unwrap_or_else(|_| "A".into()),
                path: a,
            },
            Binary {
                label: std::env::var("KAKEHASHI_BENCH_LABEL_B").unwrap_or_else(|_| "B".into()),
                path: b,
            },
        ];
    }
    let single = std::env::var("KAKEHASHI_BENCH_BIN")
        .unwrap_or_else(|_| "target/release/kakehashi".to_string());
    vec![Binary {
        label: "binary".into(),
        path: single,
    }]
}

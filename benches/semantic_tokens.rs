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
//! `KAKEHASHI_BENCH_WARMUP` (default 10), `KAKEHASHI_BENCH_SCENARIOS`
//! (optional comma-separated scenario-name substrings), and
//! `KAKEHASHI_BENCH_SAMPLES_FILE` (optional raw-sample JSON output path).
//! `KAKEHASHI_BENCH_DATA_DIR` can point both servers at an attested fixture
//! snapshot instead of the checkout-local persistent test directory.

use serde_json::{Value, json};
use std::collections::HashMap;
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
    buffered_responses: HashMap<i64, Value>,
}

impl Server {
    /// Spawn `bin`, run the LSP handshake, and return a ready server.
    fn start(bin: &str, data_dir: &str) -> Server {
        let stderr = if std::env::var_os("KAKEHASHI_BENCH_SERVER_STDERR").is_some() {
            Stdio::inherit()
        } else {
            Stdio::null()
        };
        let mut child = Command::new(bin)
            .env("KAKEHASHI_DATA_DIR", data_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(stderr)
            .spawn()
            .unwrap_or_else(|e| panic!("failed to spawn server binary {bin:?}: {e}"));

        let stdin = child.stdin.take().expect("stdin");
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));
        let mut server = Server {
            child,
            stdin,
            stdout,
            next_id: 0,
            buffered_responses: HashMap::new(),
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

    fn send_semantic_full(&mut self, uri: &str) -> i64 {
        self.send_request(
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

    /// `semanticTokens/range` for a whole-line range.
    fn semantic_range(&mut self, uri: &str, start_line: u32, end_line: u32) -> Value {
        self.request(
            "textDocument/semanticTokens/range",
            json!({
                "textDocument": { "uri": uri },
                "range": {
                    "start": { "line": start_line, "character": 0 },
                    "end": { "line": end_line, "character": 0 },
                }
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

    /// A unique typing edit: insert one more space at the cursor without
    /// toggling back to an earlier whole-document cache key. Real typing grows
    /// through distinct document states; an A/B toggle would turn every other
    /// request into an unrealistic whole-document cache hit.
    fn did_change_insert_space(&mut self, uri: &str, version: i64, line: u32) {
        self.notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": uri, "version": version },
                "contentChanges": [{
                    "range": {
                        "start": { "line": line, "character": 0 },
                        "end": { "line": line, "character": 0 },
                    },
                    "text": " ",
                }],
            }),
        );
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.send_request(method, params);
        self.recv_response(id)
    }

    fn send_request(&mut self, method: &str, params: Value) -> i64 {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }));
        id
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
        let msg = self.recv_raw_response(id);
        // Fail fast on a JSON-RPC error: a misconfigured server (missing
        // parser/query, bad setup) must not silently produce Null and bogus
        // timings.
        if let Some(error) = msg.get("error") {
            panic!("server returned a JSON-RPC error for request id={id}: {error}");
        }
        msg.get("result").cloned().unwrap_or(Value::Null)
    }

    fn recv_raw_response(&mut self, id: i64) -> Value {
        if let Some(response) = self.buffered_responses.remove(&id) {
            return response;
        }
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
            let Some(response_id) = msg.get("id").and_then(Value::as_i64) else {
                continue;
            };
            if response_id == id {
                return msg;
            }
            self.buffered_responses.insert(response_id, msg);
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

const QUERY_METADATA_GATE_BYTES: usize = 32 * 1024;

/// Syntactically valid Rust with only a handful of captures, sized exactly.
///
/// This isolates query-metadata table setup from dense capture walking around
/// the server's 32 KiB admission boundary. The marker stays on its own line so
/// typing scenarios can insert harmless leading spaces at a tracked token.
fn gen_sparse_rust(bytes: usize) -> String {
    const PREFIX: &str = "/*";
    const SUFFIX: &str = "*/\nfn sparse_marker() {}\n";

    assert!(bytes >= PREFIX.len() + SUFFIX.len());
    let mut source = String::with_capacity(bytes);
    source.push_str(PREFIX);
    source.extend(std::iter::repeat_n(
        'x',
        bytes - PREFIX.len() - SUFFIX.len(),
    ));
    source.push_str(SUFFIX);
    assert_eq!(source.len(), bytes);
    source
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
    p95: Duration,
}

fn summarize(samples: &[Duration]) -> Stats {
    let mut samples = samples.to_vec();
    samples.sort_unstable();
    let pick = |q: f64| samples[((samples.len() as f64 * q) as usize).min(samples.len() - 1)];
    let middle = samples.len() / 2;
    let median = if samples.len().is_multiple_of(2) {
        (samples[middle - 1] + samples[middle]) / 2
    } else {
        samples[middle]
    };
    Stats {
        median,
        p25: pick(0.25),
        p75: pick(0.75),
        p95: pick(0.95),
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
    /// `semanticTokens/range` over representative viewport-sized slices.
    Range {
        start_line: u32,
        end_line: u32,
        step: u32,
        variants: u32,
    },
    /// `semanticTokens/full/delta` with no edit between requests.
    DeltaNoop,
    /// Realistic editing: a toggle `didChange` then `semanticTokens/full/delta`,
    /// so each measured request pays incremental reparse + cache invalidation +
    /// delta diff on top of the (full) token recompute.
    EditDelta,
    /// Sustained typing through unique document states. Unlike `EditDelta`, this
    /// never toggles back to a previously cached whole-document snapshot.
    TypingDelta,
    /// Several unique edits arrive without waiting for intermediate token
    /// requests, followed by one delta for the only version still usable.
    TypingBurst { edits: usize },
    /// Rapid edits explicitly cancel several already-issued full requests.
    /// Measures latency from the final edit/request until the only result the
    /// editor can still use, while obsolete computation is reclaimed.
    CancelBurst { obsolete: usize },
    /// Cold-open latency: each iteration opens a FRESH document and times
    /// `didOpen` → first `semanticTokens/full` response — the editor-visible
    /// "open the file, see highlights" latency. The one scenario that captures
    /// the open parse itself (no settle sleep), so it is what validates the
    /// off-ingress open flip (#6) against the reader's watermark settle budget.
    OpenFirstToken,
}

/// Mutable per-run state for [`Kind::EditDelta`]: the next `didChange` version,
/// whether the next edit inserts (vs deletes) — flips each iteration to toggle —
/// the line edited, and the next range viewport variant.
struct EditState {
    version: i64,
    insert_next: bool,
    line: u32,
    range_next: u32,
}

/// Client-side semantic-token state. A fresh `resultId` alone does not prove
/// that a response represents the latest document version, so typing scenarios
/// also reconstruct full token data and verify the edited line moved exactly as
/// far as the inserted/removed prefix.
struct SemanticBaseline {
    result_id: String,
    data: Vec<u32>,
    tracked_line: u32,
    initial_start: u32,
    expected_shift: u32,
}

impl SemanticBaseline {
    fn seed(result: &Value, preferred_line: u32, scenario: &str) -> Self {
        let result_id = result_id_of(result).unwrap_or_else(|| {
            panic!("initial semantic response has no resultId for {scenario}: {result}")
        });
        let data = token_data_of(result).unwrap_or_else(|| {
            panic!("initial semantic response has no token data for {scenario}: {result}")
        });
        let tracked_line = nearest_token_line(&data, preferred_line)
            .unwrap_or_else(|| panic!("initial semantic response has no tokens for {scenario}"));
        let initial_start = first_token_start_on_line(&data, tracked_line)
            .expect("nearest semantic-token line must contain a token");
        Self {
            result_id,
            data,
            tracked_line,
            initial_start,
            expected_shift: 0,
        }
    }

    fn apply_and_verify(&mut self, result: &Value, scenario: &str) {
        if let Some(data) = token_data_of(result) {
            self.data = data;
        } else if let Some(edits) = result.get("edits").and_then(Value::as_array) {
            for edit in edits {
                let start = edit
                    .get("start")
                    .and_then(Value::as_u64)
                    .unwrap_or_else(|| {
                        panic!("semantic delta edit has no start for {scenario}: {edit}")
                    }) as usize;
                let delete_count = edit
                    .get("deleteCount")
                    .and_then(Value::as_u64)
                    .unwrap_or_else(|| {
                        panic!("semantic delta edit has no deleteCount for {scenario}: {edit}")
                    }) as usize;
                let end = start.saturating_add(delete_count);
                assert!(
                    end <= self.data.len(),
                    "semantic delta edit is out of bounds for {scenario}: {edit}"
                );
                let replacement = edit.get("data").map(parse_token_array).unwrap_or_default();
                self.data.splice(start..end, replacement);
            }
        } else {
            panic!("semantic response has neither data nor edits for {scenario}: {result}");
        }

        self.result_id = result_id_of(result).unwrap_or_else(|| {
            panic!("semantic response has no resultId for {scenario}: {result}")
        });
        let actual =
            first_token_start_on_line(&self.data, self.tracked_line).unwrap_or_else(|| {
                panic!(
                    "latest semantic response lost every token on tracked line {} for {scenario}",
                    self.tracked_line
                )
            });
        assert_eq!(
            actual,
            self.initial_start + self.expected_shift,
            "semantic tokens did not follow the latest edit for {scenario}"
        );
    }
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
    if let Kind::CancelBurst { obsolete } = scn.kind {
        return measure_cancel_burst(bin, scn, data_dir, iters, warmup, obsolete);
    }
    let mut server = Server::start(&bin.path, data_dir);
    server.did_open(scn.uri, scn.language_id, &scn.content);
    // Let the initial parse settle (didOpen parse may be async).
    std::thread::sleep(Duration::from_millis(300));

    // did_open used version 1; edits continue from there. Edit a line in the
    // middle of the document (representative of typical cursor position).
    let mut edit = EditState {
        version: 1,
        // First edit must insert (the document opens with no extra space).
        insert_next: true,
        line: (scn.content.lines().count() / 2) as u32,
        range_next: 0,
    };

    // Delta/edit scenarios keep a complete client-side baseline. This validates
    // both the delta protocol and that tokens represent the latest typed state.
    let mut baseline = seed_baseline(&mut server, scn, edit.line);
    if let Some(baseline) = &baseline {
        edit.line = baseline.tracked_line;
    }

    for _ in 0..warmup {
        run_once(&mut server, scn, &mut baseline, &mut edit);
    }

    let mut samples = Vec::with_capacity(iters);
    for _ in 0..iters {
        let start = Instant::now();
        run_once(&mut server, scn, &mut baseline, &mut edit);
        samples.push(start.elapsed());
    }
    samples
}

fn measure_cancel_burst(
    bin: &Binary,
    scn: &Scenario,
    data_dir: &str,
    iters: usize,
    warmup: usize,
    obsolete: usize,
) -> Vec<Duration> {
    let mut server = Server::start(&bin.path, data_dir);
    server.did_open(scn.uri, scn.language_id, &scn.content);
    std::thread::sleep(Duration::from_millis(300));
    let mut version = 1_i64;
    let preferred_line = (scn.content.lines().count() / 2) as u32;
    let mut baseline =
        SemanticBaseline::seed(&server.semantic_full(scn.uri), preferred_line, scn.name);
    let line = baseline.tracked_line;
    let mut samples = Vec::with_capacity(iters);
    for iteration in 0..(warmup + iters) {
        let mut obsolete_ids = Vec::with_capacity(obsolete);
        for _ in 0..obsolete {
            version += 1;
            server.did_change_insert_space(scn.uri, version, line);
            baseline.expected_shift += 1;
            let obsolete_id = server.send_semantic_full(scn.uri);
            // Let the request enter worker compute before explicitly
            // cancelling it; immediate cancellation would only benchmark
            // ingress coalescing.
            std::thread::sleep(Duration::from_millis(20));
            server.notify("$/cancelRequest", json!({ "id": obsolete_id }));
            obsolete_ids.push(obsolete_id);
        }
        version += 1;
        let started = Instant::now();
        server.did_change_insert_space(scn.uri, version, line);
        baseline.expected_shift += 1;
        let final_id = server.send_semantic_full(scn.uri);
        let result = server.recv_response(final_id);
        baseline.apply_and_verify(&result, scn.name);
        let elapsed = started.elapsed();
        for obsolete_id in obsolete_ids {
            let response = server.recv_raw_response(obsolete_id);
            assert_eq!(
                response.pointer("/error/code").and_then(Value::as_i64),
                Some(-32800),
                "cancel_burst obsolete request {obsolete_id} did not return RequestCancelled for {}: {response}",
                scn.name
            );
        }
        if iteration >= warmup {
            samples.push(elapsed);
        }
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

fn seed_baseline(
    server: &mut Server,
    scn: &Scenario,
    tracked_line: u32,
) -> Option<SemanticBaseline> {
    match scn.kind {
        Kind::Full | Kind::Range { .. } | Kind::OpenFirstToken | Kind::CancelBurst { .. } => None,
        // Delta-based scenarios need an initial result_id to diff against.
        Kind::DeltaNoop | Kind::EditDelta | Kind::TypingDelta | Kind::TypingBurst { .. } => Some(
            SemanticBaseline::seed(&server.semantic_full(scn.uri), tracked_line, scn.name),
        ),
    }
}

fn run_once(
    server: &mut Server,
    scn: &Scenario,
    baseline: &mut Option<SemanticBaseline>,
    edit: &mut EditState,
) {
    match scn.kind {
        // Handled by `measure_open`, which never calls `run_once`.
        Kind::OpenFirstToken => unreachable!("OpenFirstToken uses measure_open"),
        Kind::CancelBurst { .. } => {
            unreachable!("CancelBurst uses measure_cancel_burst")
        }
        Kind::Full => {
            let _ = server.semantic_full(scn.uri);
        }
        Kind::Range {
            start_line,
            end_line,
            step,
            variants,
        } => {
            let offset = (edit.range_next % variants.max(1)) * step;
            edit.range_next = edit.range_next.wrapping_add(1);
            let _ = server.semantic_range(scn.uri, start_line + offset, end_line + offset);
        }
        Kind::DeltaNoop => {
            let baseline = baseline.as_mut().expect("delta baseline");
            let result = server.semantic_delta(scn.uri, &baseline.result_id);
            baseline.apply_and_verify(&result, scn.name);
        }
        Kind::EditDelta => {
            // Apply one toggle edit, then request a delta against the prior id.
            let insert = edit.insert_next;
            edit.insert_next = !edit.insert_next;
            edit.version += 1;
            server.did_change_toggle(scn.uri, edit.version, edit.line, insert);
            let baseline = baseline.as_mut().expect("edit baseline");
            if insert {
                baseline.expected_shift += 1;
            } else {
                baseline.expected_shift -= 1;
            }
            let result = server.semantic_delta(scn.uri, &baseline.result_id);
            baseline.apply_and_verify(&result, scn.name);
        }
        Kind::TypingDelta => {
            edit.version += 1;
            server.did_change_insert_space(scn.uri, edit.version, edit.line);
            let baseline = baseline.as_mut().expect("typing baseline");
            baseline.expected_shift += 1;
            let result = server.semantic_delta(scn.uri, &baseline.result_id);
            baseline.apply_and_verify(&result, scn.name);
        }
        Kind::TypingBurst { edits } => {
            for _ in 0..edits {
                edit.version += 1;
                server.did_change_insert_space(scn.uri, edit.version, edit.line);
            }
            let baseline = baseline.as_mut().expect("typing baseline");
            baseline.expected_shift += edits as u32;
            let result = server.semantic_delta(scn.uri, &baseline.result_id);
            baseline.apply_and_verify(&result, scn.name);
        }
    }
}

fn token_data_of(result: &Value) -> Option<Vec<u32>> {
    result.get("data").map(parse_token_array)
}

fn parse_token_array(value: &Value) -> Vec<u32> {
    value
        .as_array()
        .unwrap_or_else(|| panic!("semantic token data is not an array: {value}"))
        .iter()
        .map(|value| {
            value
                .as_u64()
                .and_then(|value| u32::try_from(value).ok())
                .unwrap_or_else(|| panic!("semantic token value is not u32: {value}"))
        })
        .collect()
}

fn first_token_start_on_line(data: &[u32], tracked_line: u32) -> Option<u32> {
    assert_eq!(data.len() % 5, 0, "semantic token data is not 5-tuples");
    let mut line = 0_u32;
    let mut start = 0_u32;
    for token in data.chunks_exact(5) {
        line += token[0];
        start = if token[0] == 0 {
            start + token[1]
        } else {
            token[1]
        };
        if line == tracked_line {
            return Some(start);
        }
        if line > tracked_line {
            return None;
        }
    }
    None
}

fn nearest_token_line(data: &[u32], preferred_line: u32) -> Option<u32> {
    assert_eq!(data.len() % 5, 0, "semantic token data is not 5-tuples");
    let mut line = 0_u32;
    let mut start = 0_u32;
    let mut nearest = None;
    let mut fallback = None;
    let mut previous_line = None;
    for token in data.chunks_exact(5) {
        line += token[0];
        start = if token[0] == 0 {
            start + token[1]
        } else {
            token[1]
        };
        if previous_line == Some(line) {
            continue;
        }
        previous_line = Some(line);
        if fallback.is_none_or(|current: u32| {
            line.abs_diff(preferred_line) < current.abs_diff(preferred_line)
        }) {
            fallback = Some(line);
        }
        // Leading-space edits preserve the grammar most reliably on an already
        // indented token line (not headings/fences at column zero in Markdown).
        if start > 0
            && nearest.is_none_or(|current: u32| {
                line.abs_diff(preferred_line) < current.abs_diff(preferred_line)
            })
        {
            nearest = Some(line);
        }
        if line > preferred_line && nearest.is_some() {
            break;
        }
    }
    nearest.or(fallback)
}

fn result_id_of(result: &Value) -> Option<String> {
    result
        .get("resultId")
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn main() {
    // Ensure parsers/queries exist, and reuse the test data dir both binaries read.
    let data_dir: PathBuf = std::env::var_os("KAKEHASHI_BENCH_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(kakehashi::install::test_support::test_data_dir_path);
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
            name: "rust_large/range",
            language_id: "rust",
            uri: "file:///bench/large_range.rs",
            content: gen_rust(150),
            kind: Kind::Range {
                start_line: 500,
                end_line: 560,
                step: 20,
                variants: 8,
            },
            targets: "range request variation with scrolling viewports; first miss seeds full-cache filtering",
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
            name: "markdown_injections_large/range",
            language_id: "markdown",
            uri: "file:///bench/injections_large_range.md",
            content: gen_markdown_injections(150),
            kind: Kind::Range {
                start_line: 600,
                end_line: 680,
                step: 24,
                variants: 8,
            },
            targets: "range request variation across markdown injections; first miss seeds full-cache filtering",
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
            name: "rust_large/typing_delta",
            language_id: "rust",
            uri: "file:///bench/large_typing.rs",
            content: gen_rust(150),
            kind: Kind::TypingDelta,
            targets: "unique edit states→reparse→retokenize→delta; excludes A/B cache returns",
        },
        Scenario {
            name: "rust_sparse_below_gate/typing_delta",
            language_id: "rust",
            uri: "file:///bench/sparse_below_gate.rs",
            // Leave enough margin that all warmup/timed leading-space edits
            // remain below the gate.
            content: gen_sparse_rust(QUERY_METADATA_GATE_BYTES - 128),
            kind: Kind::TypingDelta,
            targets: "sparse query walk below the 32 KiB metadata-table gate",
        },
        Scenario {
            name: "rust_sparse_at_gate/typing_delta",
            language_id: "rust",
            uri: "file:///bench/sparse_at_gate.rs",
            content: gen_sparse_rust(QUERY_METADATA_GATE_BYTES),
            kind: Kind::TypingDelta,
            targets: "sparse query walk at the 32 KiB metadata-table gate",
        },
        Scenario {
            name: "rust_sparse_above_gate/typing_delta",
            language_id: "rust",
            uri: "file:///bench/sparse_above_gate.rs",
            content: gen_sparse_rust(QUERY_METADATA_GATE_BYTES + 128),
            kind: Kind::TypingDelta,
            targets: "sparse query walk just above the 32 KiB metadata-table gate",
        },
        Scenario {
            name: "rust_sparse_medium/typing_delta",
            language_id: "rust",
            uri: "file:///bench/sparse_medium.rs",
            content: gen_sparse_rust(64 * 1024),
            kind: Kind::TypingDelta,
            targets: "gate-enabled 64 KiB source with few query matches",
        },
        Scenario {
            name: "rust_large/typing_burst",
            language_id: "rust",
            uri: "file:///bench/large_typing_burst.rs",
            content: gen_rust(150),
            kind: Kind::TypingBurst { edits: 8 },
            targets: "eight queued unique edits→current delta; latest-wins follow latency",
        },
        Scenario {
            name: "rust_xlarge/cancel_burst",
            language_id: "rust",
            uri: "file:///bench/large_supersede.rs",
            content: gen_rust(600),
            kind: Kind::CancelBurst { obsolete: 4 },
            targets: "current-token latency after four superseded unique edit states",
        },
        Scenario {
            name: "markdown_injections/edit_delta",
            language_id: "markdown",
            uri: "file:///bench/injections_edit.md",
            content: gen_markdown_injections(60),
            kind: Kind::EditDelta,
            targets: "edit→reparse→injection re-detect→cache invalidation→delta (typing)",
        },
        Scenario {
            name: "markdown_injections/typing_delta",
            language_id: "markdown",
            uri: "file:///bench/injections_typing.md",
            content: gen_markdown_injections(60),
            kind: Kind::TypingDelta,
            targets: "unique edit states→injection reuse→delta; excludes A/B cache returns",
        },
        Scenario {
            name: "markdown_injections/typing_burst",
            language_id: "markdown",
            uri: "file:///bench/injections_typing_burst.md",
            content: gen_markdown_injections(60),
            kind: Kind::TypingBurst { edits: 8 },
            targets: "eight queued unique edits→current injection delta; latest-wins follow latency",
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
    let scenarios = filter_scenarios(scenarios);

    println!();
    println!("semantic-tokens benchmark  (iters={iters}, warmup={warmup}, lower is better)");
    for b in &binaries {
        println!("  {} = {}", b.label, b.path);
    }
    println!();

    let mut raw_runs = Vec::new();

    if binaries.len() == 2 {
        print_ab_header(&binaries[0].label, &binaries[1].label);
        for scn in &scenarios {
            // Interleave A and B at the scenario level (separate processes); the
            // two runs are back-to-back to minimize machine drift between them.
            let a_samples = measure(&binaries[0], scn, &data_dir, iters, warmup);
            let b_samples = measure(&binaries[1], scn, &data_dir, iters, warmup);
            record_raw_run(&mut raw_runs, &binaries[0], scn, &a_samples);
            record_raw_run(&mut raw_runs, &binaries[1], scn, &b_samples);
            let a = summarize(&a_samples);
            let b = summarize(&b_samples);
            print_ab_row(scn, &a, &b);
        }
    } else {
        print_single_header();
        for scn in &scenarios {
            let samples = measure(&binaries[0], scn, &data_dir, iters, warmup);
            record_raw_run(&mut raw_runs, &binaries[0], scn, &samples);
            let s = summarize(&samples);
            print_single_row(scn, &s);
        }
    }
    write_raw_samples(iters, warmup, &binaries, raw_runs);
    println!();
}

fn record_raw_run(raw_runs: &mut Vec<Value>, bin: &Binary, scn: &Scenario, samples: &[Duration]) {
    raw_runs.push(json!({
        "binary_label": bin.label,
        "binary_path": bin.path,
        "scenario": scn.name,
        "samples_ns": samples.iter().map(Duration::as_nanos).collect::<Vec<_>>(),
    }));
}

fn write_raw_samples(iters: usize, warmup: usize, binaries: &[Binary], raw_runs: Vec<Value>) {
    let Some(path) = std::env::var_os("KAKEHASHI_BENCH_SAMPLES_FILE") else {
        return;
    };
    let output = json!({
        "schema_version": 1,
        "iterations": iters,
        "warmup_iterations": warmup,
        "binaries": binaries.iter().map(|bin| json!({
            "label": bin.label,
            "path": bin.path,
        })).collect::<Vec<_>>(),
        "runs": raw_runs,
    });
    let bytes = serde_json::to_vec_pretty(&output).expect("serialize raw benchmark samples");
    std::fs::write(&path, bytes)
        .unwrap_or_else(|error| panic!("write raw samples to {:?}: {error}", path));
}

fn filter_scenarios(scenarios: Vec<Scenario>) -> Vec<Scenario> {
    let Some(filter) = std::env::var("KAKEHASHI_BENCH_SCENARIOS")
        .ok()
        .filter(|s| !s.trim().is_empty())
    else {
        return scenarios;
    };
    let terms: Vec<String> = filter
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    let filtered: Vec<_> = scenarios
        .into_iter()
        .filter(|scenario| terms.iter().any(|term| scenario.name.contains(term)))
        .collect();
    if filtered.is_empty() {
        panic!("KAKEHASHI_BENCH_SCENARIOS={filter:?} matched no scenarios");
    }
    filtered
}

// ───────────────────────────── Output formatting ──────────────────────────────

fn print_single_header() {
    println!(
        "{:<34} {:>10} {:>10} {:>10} {:>10}",
        "scenario", "median", "p25", "p75", "p95"
    );
    println!("{}", "-".repeat(79));
}

fn print_single_row(scn: &Scenario, s: &Stats) {
    println!(
        "{:<34} {:>9.3}ms {:>9.3}ms {:>9.3}ms {:>9.3}ms",
        scn.name,
        ms(s.median),
        ms(s.p25),
        ms(s.p75),
        ms(s.p95)
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
    println!("    p95: {:>10.3}ms {:>10.3}ms", ms(a.p95), ms(b.p95));
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

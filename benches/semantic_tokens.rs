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
//! A/B comparison (the common case — see benches/README.md):
//! ```sh
//! KAKEHASHI_BENCH_BIN_A=/tmp/kakehashi-main KAKEHASHI_BENCH_LABEL_A=main \
//! KAKEHASHI_BENCH_BIN_B=/tmp/kakehashi-head KAKEHASHI_BENCH_LABEL_B=head \
//!   cargo bench --bench semantic_tokens --features e2e
//! ```
//!
//! Tunables (env): `KAKEHASHI_BENCH_ITERS` (default 80),
//! `KAKEHASHI_BENCH_WARMUP` (default 10), `KAKEHASHI_BENCH_SCENARIOS`
//! (optional comma-separated scenario-name substrings), and
//! `KAKEHASHI_BENCH_SAMPLES_FILE` (optional raw JSON output). Reproducible
//! comparisons should use `benches/collect_semantic_pairs.py`, which supplies
//! the fixture and binary attestations required by raw output.

#[path = "support/semantic_baseline.rs"]
mod semantic_baseline;

use semantic_baseline::{
    SemanticBaseline, TRACKED_MARKER, tracked_marker_line, validate_token_payload,
};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

// ───────────────────────────── Minimal LSP client ─────────────────────────────

/// A spawned `kakehashi` server process plus the JSON-RPC plumbing needed to
/// initialize it and request semantic tokens. Intentionally minimal — just the
/// subset of `tests/helpers/lsp_client.rs` the benchmark needs (benches cannot
/// import test helpers).
struct Server {
    child: Child,
    stdin: ChildStdin,
    responses: Receiver<Result<Value, String>>,
    reader: Option<JoinHandle<()>>,
    next_id: i64,
    buffered_responses: HashMap<i64, Value>,
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
        let stdout = child.stdout.take().expect("stdout");
        let (sender, responses) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            let mut stdout = BufReader::new(stdout);
            loop {
                let message = recv_message(&mut stdout);
                let should_stop = message.is_err();
                if sender.send(message).is_err() || should_stop {
                    break;
                }
            }
        });
        let mut server = Server {
            child,
            stdin,
            responses,
            reader: Some(reader),
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

    /// Insert a new leading space without returning to an earlier document
    /// state. This models sustained typing without whole-document cache hits.
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

    fn did_change_replace_ascii(&mut self, uri: &str, version: i64, line: u32, text: char) {
        self.notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": uri, "version": version },
                "contentChanges": [{
                    "range": {
                        "start": { "line": line, "character": 0 },
                        "end": { "line": line, "character": 1 },
                    },
                    "text": text.to_string(),
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
        let response = self.recv_raw_response(id);
        if let Some(error) = response.get("error") {
            panic!("server returned a JSON-RPC error for request id={id}: {error}");
        }
        response.get("result").cloned().unwrap_or(Value::Null)
    }

    fn recv_raw_response(&mut self, id: i64) -> Value {
        if let Some(response) = self.buffered_responses.remove(&id) {
            return response;
        }

        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .unwrap_or(Duration::ZERO);
            let msg = self
                .responses
                .recv_timeout(remaining)
                .unwrap_or_else(|error| panic!("failed waiting for response id={id}: {error}"))
                .unwrap_or_else(|error| panic!("failed reading server response: {error}"));
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
}

fn recv_message(stdout: &mut impl BufRead) -> Result<Value, String> {
    let mut content_length = None;
    loop {
        let mut line = String::new();
        let read = stdout
            .read_line(&mut line)
            .map_err(|error| format!("failed reading LSP header: {error}"))?;
        if read == 0 {
            return Err("server closed stdout unexpectedly".to_owned());
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break;
        }
        if let Some(value) = trimmed.strip_prefix("Content-Length:") {
            content_length = Some(
                value
                    .trim()
                    .parse::<usize>()
                    .map_err(|error| format!("invalid Content-Length: {error}"))?,
            );
        }
    }
    let len = content_length.ok_or_else(|| "missing Content-Length".to_owned())?;
    let mut body = vec![0_u8; len];
    stdout
        .read_exact(&mut body)
        .map_err(|error| format!("failed reading LSP body: {error}"))?;
    serde_json::from_slice(&body).map_err(|error| format!("invalid JSON body: {error}"))
}

impl Drop for Server {
    fn drop(&mut self) {
        // Best-effort shutdown; kill if it doesn't exit promptly.
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

// ───────────────────────────── Document generators ────────────────────────────

/// Dense, all-ASCII Rust source: keywords, types, strings, numbers, comments,
/// functions, and locals. Exercises the host-only token path.
fn gen_rust(funcs: usize) -> String {
    let mut s = String::with_capacity(funcs * 400);
    s.push_str("use std::collections::HashMap;\nuse std::fmt;\n\n");
    s.push_str(TRACKED_MARKER);
    s.push_str("\n\n");
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

const SPARSE_CONTROL_BOUNDARY_BYTES: usize = 32 * 1024;

/// Syntactically valid sparse Rust with exact byte size and a stable edit line.
fn gen_sparse_rust(bytes: usize) -> String {
    const PREFIX: &str = "/*";
    let suffix = format!("*/\n{TRACKED_MARKER}\n");

    assert!(bytes >= PREFIX.len() + suffix.len());
    let mut source = String::with_capacity(bytes);
    source.push_str(PREFIX);
    source.extend(std::iter::repeat_n(
        'x',
        bytes - PREFIX.len() - suffix.len(),
    ));
    source.push_str(&suffix);
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
        let tracked_marker = if i == 0 {
            format!("{TRACKED_MARKER}\n")
        } else {
            String::new()
        };
        match i % 3 {
            0 => s.push_str(&format!(
                "```rust\n\
                 {tracked_marker}\
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
    s.push_str(TRACKED_MARKER);
    s.push_str("\n\n");
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
    s.push_str(TRACKED_MARKER);
    s.push_str("\n\n");
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

fn summarize(mut samples: Vec<Duration>) -> Stats {
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
    /// Sustained typing through unique document states. Unlike `EditDelta`,
    /// this never toggles back to a previously cached snapshot.
    TypingDelta,
    /// Unique same-width edit states, preserving the scenario's exact byte size.
    FixedWidthTypingDelta,
    /// Several edits arrive before the one semantic-token request whose result
    /// is still useful to an editor.
    TypingBurst { edits: usize },
    /// Several full requests become obsolete and are explicitly cancelled
    /// before the one response an editor can still use.
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
    fixed_width_next: usize,
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
    sha256: String,
}

struct Measurement {
    samples: Vec<Duration>,
    discarded_attempts: usize,
}

/// Run one scenario against one binary and return per-request timing samples.
/// `iters` requests are measured after `warmup` unmeasured ones.
fn measure(
    bin: &Binary,
    scn: &Scenario,
    data_dir: &str,
    iters: usize,
    warmup: usize,
) -> Measurement {
    if let Kind::CancelBurst { obsolete } = scn.kind {
        return measure_cancel_burst(bin, scn, data_dir, iters, warmup, obsolete);
    }
    if let Kind::OpenFirstToken = scn.kind {
        return measure_open(bin, scn, data_dir, iters, warmup);
    }
    let mut server = Server::start(&bin.path, data_dir);
    server.did_open(scn.uri, scn.language_id, &scn.content);
    // Let the initial parse settle (didOpen parse may be async).
    std::thread::sleep(Duration::from_millis(300));

    // Delta scenarios retain and validate the complete client-side baseline,
    // rather than treating a fresh resultId as proof of a current response.
    let mut baseline = seed_baseline(&mut server, scn);
    let mut edit = EditState {
        version: 1,
        // First edit must insert (the document opens with no extra space).
        insert_next: true,
        line: baseline.as_ref().map_or(0, SemanticBaseline::tracked_line),
        range_next: 0,
        fixed_width_next: 0,
    };

    for _ in 0..warmup {
        run_once(&mut server, scn, &mut baseline, &mut edit);
    }

    let mut samples = Vec::with_capacity(iters);
    for _ in 0..iters {
        let start = Instant::now();
        run_once(&mut server, scn, &mut baseline, &mut edit);
        samples.push(start.elapsed());
    }
    Measurement {
        samples,
        discarded_attempts: 0,
    }
}

fn measure_cancel_burst(
    bin: &Binary,
    scn: &Scenario,
    data_dir: &str,
    iters: usize,
    warmup: usize,
    obsolete: usize,
) -> Measurement {
    let mut server = Server::start(&bin.path, data_dir);
    server.did_open(scn.uri, scn.language_id, &scn.content);
    std::thread::sleep(Duration::from_millis(300));

    let tracked_line = tracked_marker_line(&scn.content)
        .unwrap_or_else(|error| panic!("invalid tracked marker for {}: {error:?}", scn.name));
    let mut baseline = SemanticBaseline::from_full(&server.semantic_full(scn.uri), tracked_line)
        .unwrap_or_else(|error| panic!("invalid semantic baseline for {}: {error:?}", scn.name));
    let mut version = 1_i64;
    let required = warmup + iters;
    let max_attempts = required.saturating_mul(20).max(20);
    let mut retained = 0_usize;
    let mut discarded = 0_usize;
    let mut samples = Vec::with_capacity(iters);

    for _ in 0..max_attempts {
        if retained == required {
            break;
        }

        let mut obsolete_ids = Vec::with_capacity(obsolete);
        for _ in 0..obsolete {
            version += 1;
            server.did_change_insert_space(scn.uri, version, tracked_line);
            baseline
                .record_prefix_insert(1)
                .expect("tracked position remains valid");
            let id = server.send_request(
                "textDocument/semanticTokens/full",
                json!({ "textDocument": { "uri": scn.uri } }),
            );
            // Give dispatch a chance to start computation. If it completes in
            // this interval, the attempt is classified as unusable below.
            std::thread::sleep(Duration::from_millis(1));
            server.notify("$/cancelRequest", json!({ "id": id }));
            obsolete_ids.push(id);
        }

        version += 1;
        let started = Instant::now();
        server.did_change_insert_space(scn.uri, version, tracked_line);
        baseline
            .record_prefix_insert(1)
            .expect("tracked position remains valid");
        let result = server.semantic_full(scn.uri);
        let elapsed = started.elapsed();
        baseline.apply_response(&result).unwrap_or_else(|error| {
            panic!("invalid semantic response for {}: {error:?}", scn.name)
        });

        let mut all_cancelled = true;
        for id in obsolete_ids {
            let response = server.recv_raw_response(id);
            match response.pointer("/error/code").and_then(Value::as_i64) {
                Some(-32800) => {}
                None if response.get("result").is_some() => all_cancelled = false,
                code => panic!(
                    "obsolete request {id} returned unexpected status {code:?} for {}: {response}",
                    scn.name
                ),
            }
        }

        if !all_cancelled {
            discarded += 1;
            continue;
        }
        if retained >= warmup {
            samples.push(elapsed);
        }
        retained += 1;
    }

    assert_eq!(
        retained, required,
        "could not retain {required} cancellation samples for {} after {max_attempts} attempts; discarded {discarded}",
        scn.name
    );
    if discarded > 0 {
        eprintln!(
            "{}: discarded {discarded} attempts whose obsolete request completed before cancellation",
            scn.name
        );
    }
    Measurement {
        samples,
        discarded_attempts: discarded,
    }
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
) -> Measurement {
    let mut server = Server::start(&bin.path, data_dir);
    let mut samples = Vec::with_capacity(iters);
    for i in 0..(warmup + iters) {
        let uri = format!("{}_{i}", scn.uri);
        let start = Instant::now();
        server.did_open(&uri, scn.language_id, &scn.content);
        // Blocks until the server answers — i.e. until the (off-ingress) open parse
        // has produced a tree the reader can tokenize, or the reader fell back to an
        // on-demand parse.
        let result = server.semantic_full(&uri);
        validate_full_response(scn, &result);
        let elapsed = start.elapsed();
        if i >= warmup {
            samples.push(elapsed);
        }
    }
    Measurement {
        samples,
        discarded_attempts: 0,
    }
}

fn validate_full_response(scn: &Scenario, result: &Value) {
    let tracked_line = tracked_marker_line(&scn.content)
        .unwrap_or_else(|error| panic!("invalid tracked marker for {}: {error:?}", scn.name));
    SemanticBaseline::from_full(result, tracked_line)
        .unwrap_or_else(|error| panic!("invalid full response for {}: {error:?}", scn.name));
}

fn seed_baseline(server: &mut Server, scn: &Scenario) -> Option<SemanticBaseline> {
    match scn.kind {
        Kind::Full | Kind::Range { .. } | Kind::OpenFirstToken | Kind::CancelBurst { .. } => None,
        Kind::DeltaNoop
        | Kind::EditDelta
        | Kind::TypingDelta
        | Kind::FixedWidthTypingDelta
        | Kind::TypingBurst { .. } => {
            let tracked_line = tracked_marker_line(&scn.content).unwrap_or_else(|error| {
                panic!("invalid tracked marker for {}: {error:?}", scn.name)
            });
            Some(
                SemanticBaseline::from_full(&server.semantic_full(scn.uri), tracked_line)
                    .unwrap_or_else(|error| {
                        panic!("invalid semantic baseline for {}: {error:?}", scn.name)
                    }),
            )
        }
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
        Kind::CancelBurst { .. } => unreachable!("CancelBurst uses measure_cancel_burst"),
        Kind::Full => {
            let result = server.semantic_full(scn.uri);
            validate_full_response(scn, &result);
        }
        Kind::Range {
            start_line,
            end_line,
            step,
            variants,
        } => {
            let offset = (edit.range_next % variants.max(1)) * step;
            edit.range_next = edit.range_next.wrapping_add(1);
            let result = server.semantic_range(scn.uri, start_line + offset, end_line + offset);
            validate_token_payload(&result).unwrap_or_else(|error| {
                panic!("invalid range response for {}: {error:?}", scn.name)
            });
        }
        Kind::DeltaNoop => {
            let baseline = baseline.as_mut().expect("delta baseline");
            let result = server.semantic_delta(scn.uri, baseline.result_id());
            baseline.apply_response(&result).unwrap_or_else(|error| {
                panic!("invalid semantic response for {}: {error:?}", scn.name)
            });
        }
        Kind::EditDelta => {
            // Apply one toggle edit, then request a delta against the prior id.
            let insert = edit.insert_next;
            edit.insert_next = !edit.insert_next;
            edit.version += 1;
            server.did_change_toggle(scn.uri, edit.version, edit.line, insert);
            let baseline = baseline.as_mut().expect("edit baseline");
            let updated = if insert {
                baseline.record_prefix_insert(1)
            } else {
                baseline.record_prefix_delete(1)
            };
            updated.expect("tracked position remains valid");
            let result = server.semantic_delta(scn.uri, baseline.result_id());
            baseline.apply_response(&result).unwrap_or_else(|error| {
                panic!("invalid semantic response for {}: {error:?}", scn.name)
            });
        }
        Kind::TypingDelta => {
            edit.version += 1;
            server.did_change_insert_space(scn.uri, edit.version, edit.line);
            let baseline = baseline.as_mut().expect("typing baseline");
            baseline
                .record_prefix_insert(1)
                .expect("tracked position remains valid");
            let result = server.semantic_delta(scn.uri, baseline.result_id());
            baseline.apply_response(&result).unwrap_or_else(|error| {
                panic!("invalid semantic response for {}: {error:?}", scn.name)
            });
        }
        Kind::FixedWidthTypingDelta => {
            const STATES: &[u8] = b"abcdeghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
            edit.version += 1;
            let replacement = char::from(STATES[edit.fixed_width_next % STATES.len()]);
            edit.fixed_width_next += 1;
            server.did_change_replace_ascii(scn.uri, edit.version, edit.line, replacement);
            let baseline = baseline.as_mut().expect("typing baseline");
            let result = server.semantic_delta(scn.uri, baseline.result_id());
            baseline.apply_response(&result).unwrap_or_else(|error| {
                panic!("invalid semantic response for {}: {error:?}", scn.name)
            });
        }
        Kind::TypingBurst { edits } => {
            for _ in 0..edits {
                edit.version += 1;
                server.did_change_insert_space(scn.uri, edit.version, edit.line);
            }
            let baseline = baseline.as_mut().expect("typing baseline");
            baseline
                .record_prefix_insert(u32::try_from(edits).expect("edit count fits u32"))
                .expect("tracked position remains valid");
            let result = server.semantic_delta(scn.uri, baseline.result_id());
            baseline.apply_response(&result).unwrap_or_else(|error| {
                panic!("invalid semantic response for {}: {error:?}", scn.name)
            });
        }
    }
}

fn validate_full_response_generators() {
    for content in [
        gen_rust(1),
        gen_rust_predicate_heavy(1),
        gen_markdown_injections(1),
        gen_unicode_rust(1),
    ] {
        tracked_marker_line(&content).expect("full-response generator has one fixed marker");
    }
}

fn main() {
    validate_full_response_generators();
    if std::env::var_os("KAKEHASHI_BENCH_VALIDATE_SCENARIOS").is_some() {
        return;
    }
    if let Some(path) = std::env::var_os("KAKEHASHI_BENCH_PREPARE_DATA_DIR") {
        let path = PathBuf::from(path);
        std::fs::create_dir_all(&path).expect("create benchmark fixture");
        kakehashi::install::test_support::ensure_test_languages_installed(&path)
            .expect("install benchmark fixture languages");
        validate_fixture(&path);
        println!("prepared benchmark fixture at {}", path.display());
        return;
    }

    // An explicit fixture is attested input: verify it without installing or
    // otherwise mutating it. The implicit checkout-local fixture retains the
    // convenient install-on-first-run behavior for interactive use.
    let data_dir = if let Some(path) = std::env::var_os("KAKEHASHI_BENCH_DATA_DIR") {
        let path = PathBuf::from(path);
        validate_fixture(&path);
        path
    } else {
        let path = kakehashi::install::test_support::test_data_dir_path();
        std::fs::create_dir_all(&path).expect("create data dir");
        kakehashi::install::test_support::ensure_test_languages_installed(&path)
            .expect("install test languages");
        path
    };
    let data_dir = data_dir.to_string_lossy().to_string();

    let iters = env_usize("KAKEHASHI_BENCH_ITERS", 80);
    let warmup = env_usize("KAKEHASHI_BENCH_WARMUP", 10);

    let binaries = resolve_binaries();

    let scenarios = vec![
        Scenario {
            name: "rust_small/full_cache_hit",
            language_id: "rust",
            uri: "file:///bench/small.rs",
            content: gen_rust(15),
            kind: Kind::Full,
            targets: "small exact-snapshot semantic cache-hit control",
        },
        Scenario {
            name: "rust_large/full_cache_hit",
            language_id: "rust",
            uri: "file:///bench/large.rs",
            content: gen_rust(150),
            kind: Kind::Full,
            targets: "large exact-snapshot semantic cache-hit control",
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
            name: "rust_predicate_heavy/full_cache_hit",
            language_id: "rust",
            uri: "file:///bench/predicates.rs",
            content: gen_rust_predicate_heavy(120),
            kind: Kind::Full,
            targets: "predicate-heavy exact-snapshot semantic cache-hit control",
        },
        Scenario {
            name: "markdown_injections/full_cache_hit",
            language_id: "markdown",
            uri: "file:///bench/injections.md",
            content: gen_markdown_injections(60),
            kind: Kind::Full,
            targets: "injection document exact-snapshot semantic cache-hit control",
        },
        Scenario {
            name: "markdown_injections_large/full_cache_hit",
            language_id: "markdown",
            uri: "file:///bench/injections_large.md",
            content: gen_markdown_injections(150),
            kind: Kind::Full,
            targets: "large injection document exact-snapshot semantic cache-hit control",
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
            name: "unicode_rust/full_cache_hit",
            language_id: "rust",
            uri: "file:///bench/unicode.rs",
            content: gen_unicode_rust(150),
            kind: Kind::Full,
            targets: "Unicode document exact-snapshot semantic cache-hit control",
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
            kind: Kind::FixedWidthTypingDelta,
            targets: "unique edit states→reparse→retokenize→delta; excludes A/B cache returns",
        },
        Scenario {
            name: "rust_sparse_32k_minus/typing_delta",
            language_id: "rust",
            uri: "file:///bench/sparse_32k_minus.rs",
            content: gen_sparse_rust(SPARSE_CONTROL_BOUNDARY_BYTES - 128),
            kind: Kind::FixedWidthTypingDelta,
            targets: "sparse low-match control just below 32 KiB",
        },
        Scenario {
            name: "rust_sparse_32k_exact/typing_delta",
            language_id: "rust",
            uri: "file:///bench/sparse_32k_exact.rs",
            content: gen_sparse_rust(SPARSE_CONTROL_BOUNDARY_BYTES),
            kind: Kind::FixedWidthTypingDelta,
            targets: "sparse low-match control at 32 KiB",
        },
        Scenario {
            name: "rust_sparse_32k_plus/typing_delta",
            language_id: "rust",
            uri: "file:///bench/sparse_32k_plus.rs",
            content: gen_sparse_rust(SPARSE_CONTROL_BOUNDARY_BYTES + 128),
            kind: Kind::FixedWidthTypingDelta,
            targets: "sparse low-match control just above 32 KiB",
        },
        Scenario {
            name: "rust_sparse_64k/typing_delta",
            language_id: "rust",
            uri: "file:///bench/sparse_64k.rs",
            content: gen_sparse_rust(64 * 1024),
            kind: Kind::TypingDelta,
            targets: "sparse low-match query walk at 64 KiB",
        },
        Scenario {
            name: "rust_large/typing_burst",
            language_id: "rust",
            uri: "file:///bench/large_typing_burst.rs",
            content: gen_rust(150),
            kind: Kind::TypingBurst { edits: 8 },
            targets: "eight queued unique edits→current delta; latest-state follow latency",
        },
        Scenario {
            name: "rust_xlarge/cancel_burst",
            language_id: "rust",
            uri: "file:///bench/large_supersede.rs",
            content: gen_rust(600),
            kind: Kind::CancelBurst { obsolete: 4 },
            targets: "current-token latency after four explicitly cancelled edit states",
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
            targets: "eight queued unique edits→current injection delta; latest-state follow latency",
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
            let a_measurement = measure(&binaries[0], scn, &data_dir, iters, warmup);
            let b_measurement = measure(&binaries[1], scn, &data_dir, iters, warmup);
            record_raw_run(&mut raw_runs, &binaries[0], scn, &a_measurement);
            record_raw_run(&mut raw_runs, &binaries[1], scn, &b_measurement);
            let a = summarize(a_measurement.samples);
            let b = summarize(b_measurement.samples);
            print_ab_row(scn, &a, &b);
        }
    } else {
        print_single_header();
        for scn in &scenarios {
            let measurement = measure(&binaries[0], scn, &data_dir, iters, warmup);
            record_raw_run(&mut raw_runs, &binaries[0], scn, &measurement);
            let s = summarize(measurement.samples);
            print_single_row(scn, &s);
        }
    }
    write_raw_samples(iters, warmup, &binaries, raw_runs);
    println!();
}

fn record_raw_run(
    raw_runs: &mut Vec<Value>,
    bin: &Binary,
    scn: &Scenario,
    measurement: &Measurement,
) {
    raw_runs.push(json!({
        "binary_label": bin.label,
        "binary_path": bin.path,
        "binary_sha256": bin.sha256,
        "scenario": scn.name,
        "document_bytes": scn.content.len(),
        "samples_ns": measurement.samples.iter().map(Duration::as_nanos).collect::<Vec<_>>(),
        "validation": {
            "retained_samples": measurement.samples.len(),
            "discarded_attempts": measurement.discarded_attempts,
        },
    }));
}

fn write_raw_samples(iters: usize, warmup: usize, binaries: &[Binary], raw_runs: Vec<Value>) {
    let Some(path) = std::env::var_os("KAKEHASHI_BENCH_SAMPLES_FILE") else {
        return;
    };
    assert!(
        binaries.iter().all(|binary| !binary.sha256.is_empty()),
        "raw samples require attested binary SHA-256 values"
    );
    let output = json!({
        "schema_version": 3,
        "pair_index": env_optional("KAKEHASHI_BENCH_PAIR_INDEX"),
        "order": env_optional("KAKEHASHI_BENCH_ORDER"),
        "iterations": iters,
        "warmup_iterations": warmup,
        "harness_commit": env_optional("KAKEHASHI_BENCH_HARNESS_COMMIT"),
        "harness_sha256": env_optional("KAKEHASHI_BENCH_HARNESS_SHA256"),
        "fixture_sha256": env_optional("KAKEHASHI_BENCH_FIXTURE_SHA256"),
        "binaries": binaries.iter().map(|bin| json!({
            "label": bin.label,
            "path": bin.path,
            "sha256": bin.sha256,
        })).collect::<Vec<_>>(),
        "runs": raw_runs,
    });
    let bytes = serde_json::to_vec_pretty(&output).expect("serialize raw benchmark samples");
    std::fs::write(&path, bytes)
        .unwrap_or_else(|error| panic!("write raw samples to {:?}: {error}", path));
}

fn validate_fixture(data_dir: &std::path::Path) {
    assert!(
        data_dir.join(".installed").is_file(),
        "attested fixture has no .installed marker: {}",
        data_dir.display()
    );
    let parser_entries = std::fs::read_dir(data_dir.join("parser"))
        .unwrap_or_else(|error| panic!("read fixture parsers in {}: {error}", data_dir.display()))
        .collect::<Result<Vec<_>, _>>()
        .expect("read fixture parser entry");
    for language in kakehashi::install::test_support::TEST_LANGUAGES {
        assert!(
            parser_entries.iter().any(|entry| {
                entry.path().file_stem().and_then(|stem| stem.to_str()) == Some(language)
            }),
            "attested fixture has no {language} parser: {}",
            data_dir.display()
        );
        assert!(
            data_dir
                .join("queries")
                .join(language)
                .join(".kakehashi-install-complete")
                .is_file(),
            "attested fixture has incomplete {language} queries: {}",
            data_dir.display()
        );
    }
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
        format!("Δ ({label_b} vs {label_a})")
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

fn env_optional(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|value| !value.is_empty())
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
                sha256: std::env::var("KAKEHASHI_BENCH_SHA256_A").unwrap_or_default(),
            },
            Binary {
                label: std::env::var("KAKEHASHI_BENCH_LABEL_B").unwrap_or_else(|_| "B".into()),
                path: b,
                sha256: std::env::var("KAKEHASHI_BENCH_SHA256_B").unwrap_or_default(),
            },
        ];
    }
    let single = std::env::var("KAKEHASHI_BENCH_BIN")
        .unwrap_or_else(|_| "target/release/kakehashi".to_string());
    vec![Binary {
        label: "binary".into(),
        path: single,
        sha256: std::env::var("KAKEHASHI_BENCH_SHA256").unwrap_or_default(),
    }]
}

#!/usr/bin/env python3
"""Emit a framed LSP session that drives heavy semanticTokens/full work.

Piped into the kakehashi server under a sampling profiler (flamegraph/samply),
this makes the server spend almost all its time in the semantic-tokens hot path,
so the resulting flamegraph is dominated by token computation rather than IPC.

Usage:
    gen_session.py [--lang rust|markdown] [--size N] [--requests N] > session.lsp

The document generators mirror benches/semantic_tokens.rs so the profile and the
A/B benchmark exercise the same code shape.
"""
import argparse
import json
import sys


def gen_rust(funcs: int) -> str:
    out = ["use std::collections::HashMap;\nuse std::fmt;\n\n"]
    for i in range(funcs):
        out.append(
            f"/// Documentation comment for function number {i}.\n"
            f"pub fn function_{i}(arg_a: i32, arg_b: &str, items: &[u64]) -> Result<String, String> {{\n"
            f"    let local_total: i64 = arg_a as i64 * 2 + {i};\n"
            f"    let mut lookup: HashMap<String, u64> = HashMap::new();\n"
            f"    for (index, value) in items.iter().enumerate() {{\n"
            f'        lookup.insert(format!("key_{{}}", index), *value);\n'
            f"    }}\n"
            f'    let message = format!("total is {{}} for {{}}", local_total, arg_b);\n'
            f"    if local_total > 100 && !items.is_empty() {{\n"
            f'        return Err(String::from("value too large"));\n'
            f"    }}\n"
            f"    match arg_b {{\n"
            f'        "alpha" => Ok(message),\n'
            f'        "beta" => Ok(String::from("the beta branch")),\n'
            f"        _ => Ok(message.clone()),\n"
            f"    }}\n"
            f"}}\n\n"
        )
    return "".join(out)


def gen_markdown_injections(blocks: int) -> str:
    out = ["# Benchmark Document\n\nAn introductory paragraph.\n\n"]
    for i in range(blocks):
        out.append(f"## Section {i}\n\nProse before the code in section {i}.\n\n")
        kind = i % 3
        if kind == 0:
            out.append(
                f"```rust\nfn rust_block_{i}(x: i32) -> i32 {{\n"
                f"    let doubled = x * 2; // a comment\n"
                f'    let label = "section {i}";\n'
                f'    println!("{{}} {{}}", label, doubled);\n'
                f"    doubled + {i}\n}}\n```\n\n"
            )
        elif kind == 1:
            out.append(
                f"```lua\nlocal function lua_block_{i}(x)\n"
                f"    local doubled = x * 2 -- a comment\n"
                f'    local label = "section {i}"\n'
                f"    print(label, doubled)\n"
                f"    return doubled + {i}\nend\n```\n\n"
            )
        else:
            out.append(
                f"```python\ndef python_block_{i}(x):\n"
                f"    doubled = x * 2  # a comment\n"
                f'    label = "section {i}"\n'
                f"    print(label, doubled)\n"
                f"    return doubled + {i}\n```\n\n"
            )
    return "".join(out)


def frame(msg: dict) -> bytes:
    body = json.dumps(msg).encode("utf-8")
    return f"Content-Length: {len(body)}\r\n\r\n".encode("ascii") + body


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--lang", choices=["rust", "markdown"], default="rust")
    ap.add_argument("--size", type=int, default=150,
                    help="rust: function count; markdown: code-block count")
    ap.add_argument("--requests", type=int, default=300,
                    help="number of semanticTokens/full requests")
    args = ap.parse_args()

    if args.lang == "rust":
        uri, language_id, text = "file:///profile/large.rs", "rust", gen_rust(args.size)
    else:
        uri, language_id, text = ("file:///profile/injections.md", "markdown",
                                  gen_markdown_injections(args.size))

    w = sys.stdout.buffer
    rid = 0

    def req(method, params):
        nonlocal rid
        rid += 1
        w.write(frame({"jsonrpc": "2.0", "id": rid, "method": method, "params": params}))

    def notify(method, params):
        w.write(frame({"jsonrpc": "2.0", "method": method, "params": params}))

    req("initialize", {
        "processId": None, "rootUri": None,
        "capabilities": {"textDocument": {"semanticTokens": {
            "requests": {"full": {"delta": True}},
            "tokenTypes": [], "tokenModifiers": [], "formats": ["relative"]}}},
    })
    notify("initialized", {})
    notify("textDocument/didOpen", {"textDocument": {
        "uri": uri, "languageId": language_id, "version": 1, "text": text}})

    for _ in range(args.requests):
        req("textDocument/semanticTokens/full", {"textDocument": {"uri": uri}})

    req("shutdown", None)
    notify("exit", {})
    w.flush()

    sys.stderr.write(
        f"[gen_session] lang={args.lang} size={args.size} requests={args.requests} "
        f"doc_bytes={len(text)}\n")


if __name__ == "__main__":
    main()

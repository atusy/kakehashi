#!/usr/bin/env python3
"""Prepare fixed fixtures/configs and hash an existing parser/query runtime."""
import argparse
import json
from pathlib import Path
import sys

from drive import file_sha256
from gen_session import gen_rust


def runtime_asset_paths(runtime):
    return [path for directory in (runtime / "parser", runtime / "queries")
            for path in sorted(directory.rglob("*")) if path.is_file()]


def rust_fixture(target_bytes):
    prefix = "// An ordinary comment edited by the measurement driver.\n"
    count = max(1, target_bytes // len(gen_rust(1).encode()))
    while True:
        text = prefix + gen_rust(count)
        if len(text.encode()) >= target_bytes:
            return text
        count += 1


def markdown_fixture(target_bytes):
    chunks = ["An ordinary prose line edited by the measurement driver.\n\n"]
    size = len(chunks[0].encode())
    index = 0
    while size < target_bytes:
        chunk = (
            f"## Section {index}\n\n"
            "This paragraph describes a small Lua example. Ordinary prose, "
            "emphasis on *readability*, and a `value` reference surround each "
            "code block. The examples keep their local state in functions.\n\n"
            f"```lua\n-- An ordinary comment for example {index}.\n"
            f"local function example_{index}(value)\n"
            "  local doubled = value * 2\n"
            "  print(doubled)\n"
            "  return doubled\nend\n```\n\n"
        )
        chunks.append(chunk)
        size += len(chunk.encode())
        index += 1
    return "".join(chunks)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--runtime", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    runtime = args.runtime.resolve()
    output = args.output.resolve()
    if not (runtime / "parser").is_dir() or not (runtime / "queries").is_dir():
        parser.error("--runtime must contain parser/ and queries/")
    # Refuse existing output so a rerun cannot mix fixtures or peer summaries.
    output.mkdir(parents=True, exist_ok=False)
    fixtures = []
    for name, target in (("small", 2048), ("common", 32768), ("large", 1048576)):
        for language, ext, generate in (("rust", "rs", rust_fixture),
                                        ("markdown", "md", markdown_fixture)):
            path = output / f"{language}-{name}.{ext}"
            path.write_text(generate(target), encoding="utf-8")
            fixtures.append({"path": path.name, "language": language,
                             "target_bytes": target, "bytes": path.stat().st_size,
                             "sha256": file_sha256(path)})
    command = [sys.executable, str(Path(__file__).with_name("bridge_fixture.py").resolve()),
               "--summary-dir", str(output / "peer-summaries")]
    common = f"searchPaths = [{json.dumps(str(runtime), ensure_ascii=False)}]\n[languages._]\nautoInstall = false\n"
    configs = []
    for mode in ("native", "narrow", "wildcard"):
        config = common
        if mode != "native":
            config += "[languages._.bridge._self]\nenabled = true\n"
            config += f"[languages._.bridge._]\nenabled = {str(mode == 'wildcard').lower()}\n"
            if mode == "narrow":
                config += "[languages._.bridge.lua]\nenabled = true\n"
            config += f"[languageServers.profile]\ncmd = {json.dumps(command, ensure_ascii=False)}\n"
            config += f"languages = {json.dumps(['lua'] if mode == 'narrow' else ['*'])}\n"
        for language in ("rust", "markdown", "markdown_inline", "lua", "comment", "regex"):
            config += f"[languages.{language}]\n"
        path = output / f"{mode}.toml"
        path.write_text(config, encoding="utf-8")
        configs.append({"mode": mode, "path": path.name, "sha256": file_sha256(path)})
    assets = [{"path": str(path.relative_to(runtime)), "sha256": file_sha256(path)}
              for path in runtime_asset_paths(runtime)]
    scripts = {path.name: file_sha256(path) for path in (
        Path(__file__), Path(__file__).with_name("bridge_fixture.py"),
        Path(__file__).with_name("gen_session.py"))}
    (output / "inputs.json").write_text(json.dumps({
        "runtime": str(runtime), "assets": assets, "fixtures": fixtures,
        "configs": configs, "scripts": scripts,
        "peer": "sync-only fixture; no downstream analysis",
    }, indent=2) + "\n", encoding="utf-8")
    print(output)


if __name__ == "__main__":
    main()

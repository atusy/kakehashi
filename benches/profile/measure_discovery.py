#!/usr/bin/env python3
"""Alternate exact binaries across discovery scenarios, retaining raw evidence."""
import argparse
import itertools
import json
import os
from pathlib import Path
import platform
import re
import signal
import subprocess
import sys
from urllib.parse import urlsplit

from drive import file_sha256, profile_document_tag
from prepare_discovery import runtime_asset_paths


def phase_samples(log):
    """Only aggregate calls ending inside the driver's measurement window.

    These are invocation samples, not a sum or attribution per request: work
    may overlap, and work started during warmup can finish inside the window.
    """
    active = False
    samples = []
    for line in log.splitlines():
        if line == "[drive] measurement-start":
            active = True
        elif line == "[drive] measurement-end":
            active = False
        elif active:
            match = re.search(r"phase=(\w+) elapsed_us=(\d+)\s+(.*)", line)
            if match:
                samples.append({"phase": match[1], "elapsed_us": int(match[2]),
                                "details": match[3]})
            match = re.search(
                r"compute phases: compute=(\d+)us host=(\d+)us injections=(\d+)us finalize=(\d+)us (.*)",
                line,
            )
            if match:
                for index, phase in enumerate(("compute", "host_tokens", "injection_tokens", "finalize"), 1):
                    samples.append({"phase": phase, "elapsed_us": int(match[index]),
                                    "details": match[5]})
    return samples


def verify_inputs(directory, expected_manifest=None):
    manifest = json.loads((directory / "inputs.json").read_text())
    # Regenerating inputs.json must not redefine the provenance captured before
    # measurement, even when the replacement files match its new hashes.
    if expected_manifest is not None and manifest != expected_manifest:
        raise ValueError("input manifest changed during measurement")
    for item in manifest["fixtures"] + manifest["configs"]:
        if file_sha256(directory / item["path"]) != item["sha256"]:
            raise ValueError(f"input changed: {item['path']}")
    runtime = Path(manifest["runtime"])
    expected_assets = {item["path"] for item in manifest["assets"]}
    actual_assets = {str(path.relative_to(runtime)) for path in runtime_asset_paths(runtime)}
    if actual_assets != expected_assets:
        raise ValueError(f"runtime inventory changed: added={sorted(actual_assets - expected_assets)}, "
                         f"removed={sorted(expected_assets - actual_assets)}")
    for item in manifest["assets"]:
        if file_sha256(runtime / item["path"]) != item["sha256"]:
            raise ValueError(f"runtime changed: {item['path']}")
    for name, digest in manifest["scripts"].items():
        if file_sha256(Path(__file__).with_name(name)) != digest:
            raise ValueError(f"input generator/peer changed: {name}; regenerate inputs")
    return manifest


def run_driver(command, env, log, timeout):
    # The driver owns a server and possibly bridge children. A timeout must
    # reap the entire isolated process group, not just kill the Python parent.
    process = subprocess.Popen(command, env=env, stdout=log, stderr=log,
                               start_new_session=True)
    try:
        code = process.wait(timeout=timeout)
        if code:
            raise subprocess.CalledProcessError(code, command)
    except BaseException:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        process.wait()
        raise


def require_bridge_documents(peers, host_uris, language, mode):
    expected = set()
    if mode == "wildcard":
        expected = {"comment"} if language == "rust" else {"markdown_inline", "lua"}
    elif mode == "narrow" and language == "markdown":
        expected = {"lua"}
    if not expected:
        return
    tags = [profile_document_tag(uri) for uri in host_uris]
    if None in tags or len(set(tags)) != len(host_uris):
        raise ValueError("bridge evidence requires distinct host document tags")
    opened = {(document["uri"], document["language"])
              for peer in peers for document in peer.get("opened_documents", [])}
    for host_uri, tag in zip(host_uris, tags):
        if mode == "wildcard" and (host_uri, language) not in opened:
            raise ValueError(f"intended bridge host open missing for {host_uri}")
        injected = {opened_language for uri, opened_language in opened
                    if uri != host_uri and profile_document_tag(uri) == tag
                    and urlsplit(uri).path.rsplit("/", 1)[-1].startswith("kakehashi-virtual-uri-")}
        missing = expected - injected
        if missing:
            raise ValueError(f"intended bridge injections missing for {host_uri}: {sorted(missing)}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline", type=Path, required=True)
    parser.add_argument("--candidate", type=Path, required=True)
    parser.add_argument("--inputs", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--requests", type=int, default=30)
    parser.add_argument("--warmup", type=int, default=5)
    parser.add_argument("--repeats", type=int, default=3)
    parser.add_argument("--documents", type=int, nargs="+", default=[1, 4])
    parser.add_argument("--sizes", nargs="+", choices=["small", "common", "large"],
                        default=["small", "common", "large"])
    parser.add_argument("--modes", nargs="+", choices=["native", "narrow", "wildcard"],
                        default=["native", "narrow", "wildcard"])
    parser.add_argument("--timeout", type=float, default=180)
    args = parser.parse_args()
    if os.name != "posix":
        parser.error("the matrix runner requires POSIX process-group cleanup")
    if args.requests < 1 or args.warmup < 0 or args.repeats < 1 or min(args.documents) < 1 or args.timeout <= 0:
        parser.error("positive counts/timeout and nonnegative warmup required")
    for name in ("documents", "sizes", "modes"):
        values = getattr(args, name)
        if len(values) != len(set(values)):
            parser.error(f"--{name} cannot contain duplicate values")
    inputs = args.inputs.resolve()
    manifest = verify_inputs(inputs)
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    binaries = {"baseline": args.baseline.resolve(), "candidate": args.candidate.resolve()}
    hashes = {name: file_sha256(path) for name, path in binaries.items()}
    runs = []
    record = {"status": "running", "platform": platform.platform(), "cpu_count": os.cpu_count(),
              "arguments": {k: str(v) if isinstance(v, Path) else v for k, v in vars(args).items()},
              "binaries": {k: {"path": str(v), "sha256": hashes[k]} for k, v in binaries.items()},
              "harness_sha256": {name: file_sha256(Path(__file__).with_name(name))
                                  for name in ("drive.py", "measure_discovery.py")},
              "inputs": manifest, "runs": runs}

    def save():
        (output / "matrix.json").write_text(json.dumps(record, indent=2) + "\n", encoding="utf-8")

    def run(language, size, mode, documents, variant, repetition, profile=False):
        stem = f"{language}-{size}-{mode}-d{documents}-{variant}-{repetition}" + ("-profile" if profile else "")
        run_record = {"name": stem, "language": language, "size": size, "mode": mode,
                      "documents": documents, "variant": variant, "repetition": repetition,
                      "profile": profile, "status": "running"}
        runs.append(run_record)
        save()
        print(stem, flush=True)
        peer_dir = inputs / "peer-summaries"
        before = set(peer_dir.glob("*.json"))
        ext = "rs" if language == "rust" else "md"
        command = [sys.executable, str(Path(__file__).with_name("drive.py")),
                   "--bin", str(binaries[variant]), "--server-arg=--config-file",
                   "--server-arg=" + str(inputs / f"{mode}.toml"),
                   "--file", str(inputs / f"{language}-{size}.{ext}"),
                   "--data-dir", manifest["runtime"], "--requests", str(args.requests),
                   "--warmup", str(args.warmup), "--edits", "1", "--edit-delay-ms", "0",
                   "--tag-document-uris",
                   "--documents", str(documents), "--json-output", str(output / f"{stem}.json")]
        run_record["command"] = command
        env = dict(os.environ, RUST_LOG="kakehashi::profile=debug,kakehashi::semantic=debug" if profile else "")
        try:
            with (output / f"{stem}.log").open("w") as log:
                run_driver(command, env, log, args.timeout)
            result = json.loads((output / f"{stem}.json").read_text())
            samples = result["request_samples"]["textDocument/semanticTokens/full"]
            if result["binary_sha256"] != hashes[variant]:
                raise ValueError("binary changed during measurement")
            if len(samples) != args.requests * documents or result["last_semantic_token_count"] == 0:
                raise ValueError("missing responses or empty token computation")
            if result["server_exit_code"] != 0:
                raise ValueError("server exited unsuccessfully")
            completed_uris = {
                sample["uri"] for sample in samples
                if sample["status"] == "ok" and (sample.get("token_count") or 0) > 0
            }
            if completed_uris != set(result["document_uris"]):
                raise ValueError("at least one document completed no measured token work")
            peers = [json.loads(path.read_text()) for path in sorted(set(peer_dir.glob("*.json")) - before)]
            run_record["peers"] = peers
            require_bridge_documents(peers, result["document_uris"], language, mode)
            if profile:
                phases = phase_samples((output / f"{stem}.log").read_text())
                if not any(sample["phase"] == "discovery" and
                           re.search(r"(?:^|\s)completed=true(?:\s|$)", sample["details"])
                           for sample in phases):
                    raise ValueError("candidate emitted no completed discovery phase samples")
                (output / f"{stem}.phases.json").write_text(json.dumps(phases, indent=2) + "\n")
            run_record["status"] = "complete"
        except BaseException as error:
            run_record["status"] = "failed"
            run_record["error"] = str(error)
            raise
        finally:
            save()

    try:
        scenarios = itertools.product(("rust", "markdown"), args.sizes, args.modes, args.documents)
        for index, (language, size, mode, documents) in enumerate(scenarios):
            for repetition in range(args.repeats):
                order = ("baseline", "candidate") if (index + repetition) % 2 == 0 else ("candidate", "baseline")
                for variant in order:
                    run(language, size, mode, documents, variant, repetition)
            # Logged attribution is a separate run, never part of the A/B latency samples.
            run(language, size, mode, documents, "candidate", 0, profile=True)
        verify_inputs(inputs, manifest)
        for name, digest in record["harness_sha256"].items():
            if file_sha256(Path(__file__).with_name(name)) != digest:
                raise ValueError(f"measurement harness changed: {name}")
    except BaseException as error:
        record["status"] = "failed"
        record["error"] = str(error)
        raise
    else:
        record["status"] = "complete"
    finally:
        save()


if __name__ == "__main__":
    main()

#!/usr/bin/env python3
"""Collect alternating direct/relay run summaries for the Phase 0 probe."""

import argparse
import hashlib
import json
import os
import pathlib
import platform
import re
import signal
import subprocess
import sys


SCENARIOS = {
    "rust_cache_hit": [
        "--lang", "rust", "--size", "15", "--requests", "1000",
        "--warm-semantic-cache",
    ],
    "rust_edit": [
        "--lang", "rust", "--size", "15", "--requests", "100", "--edits", "1"
    ],
    "markdown_edit": [
        "--lang", "markdown", "--size", "150", "--requests", "100", "--edits", "1"
    ],
}

CONTROLLED_ENVIRONMENT_KEYS = (
    "PATH",
    "SYSTEMROOT",
    "WINDIR",
    "TMPDIR",
    "TMP",
    "TEMP",
    "LANG",
    "LC_ALL",
    "LD_LIBRARY_PATH",
    "DYLD_LIBRARY_PATH",
)
TERMINATION_SIGNALS = (signal.SIGHUP, signal.SIGTERM)
CLEANUP_SIGNALS = (*TERMINATION_SIGNALS, signal.SIGINT)
ATTESTED_BUILD_COMMAND = [
    "cargo", "build", "--release", "--bin", "kakehashi"
]


def require_posix(platform_name=os.name):
    if platform_name != "posix":
        raise SystemExit("Phase 0 relay collection requires POSIX lifecycle semantics")


def controlled_environment(source):
    return {
        key: source[key]
        for key in CONTROLLED_ENVIRONMENT_KEYS
        if key in source
    }


def committed_blob(checkout, revision, relative):
    try:
        return subprocess.run(
            ["git", "-C", checkout, "show", f"{revision}:{relative}"],
            check=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        ).stdout
    except subprocess.CalledProcessError as error:
        raise ValueError(
            f"nvim-treesitter revision lacks {relative}"
        ) from error


def artifact_provenance(data_dir, nvim_treesitter_checkout):
    origin = tool_version([
        "git", "-C", str(nvim_treesitter_checkout),
        "remote", "get-url", "origin",
    ])
    canonical_origin = origin.rstrip("/").removesuffix(".git")
    expected_origins = {
        "https://github.com/nvim-treesitter/nvim-treesitter",
        "git@github.com:nvim-treesitter/nvim-treesitter",
        "ssh://git@github.com/nvim-treesitter/nvim-treesitter",
    }
    if canonical_origin not in expected_origins:
        raise ValueError(f"unexpected nvim-treesitter origin: {origin}")
    revision = tool_version([
        "git", "-C", str(nvim_treesitter_checkout), "rev-parse", "HEAD"
    ])
    comparisons = [
        (
            data_dir / "cache/parsers.lua",
            "lua/nvim-treesitter/parsers.lua",
        )
    ]
    comparisons.extend(
        (
            query,
            "runtime/queries/" + query.relative_to(
                data_dir / "queries"
            ).as_posix(),
        )
        for query in runtime_artifact_files(data_dir)
        if query.is_relative_to(data_dir / "queries")
    )
    for installed, upstream_relative in comparisons:
        if not installed.is_file() or installed.is_symlink():
            raise ValueError(f"missing runtime artifact: {installed}")
        expected = committed_blob(
            str(nvim_treesitter_checkout), revision, upstream_relative
        )
        if installed.read_bytes() != expected:
            raise ValueError(
                f"{installed} does not match {revision}:{upstream_relative}"
            )
    return {
        "nvim_treesitter_repository": origin,
        "nvim_treesitter_revision": revision,
        "verification": "parser metadata and every installed query byte-matched",
    }


def run_order(zero_based_run):
    return ("direct", "relay") if zero_based_run % 2 == 0 else ("relay", "direct")


def estimated_tree_compute_budget(logical_cpus):
    return max(1, logical_cpus - 2)


def parse_driver_summary(output, expected_count):
    wall_match = re.search(r"\bwall=([\d.]+)ms", output)
    tokens_match = re.search(r"\btokens/req=(\d+)\b", output)
    method_match = re.search(
        r"method=textDocument/semanticTokens/full "
        r"count=(\d+) ok=(\d+) canceled=(\d+) null=(\d+) errors=(\d+) "
        r"p50=([\d.]+)ms p90=([\d.]+)ms p95=([\d.]+)ms "
        r"p99=([\d.]+)ms max=([\d.]+)ms wire=([\d.]+)KiB",
        output,
    )
    if not wall_match or not tokens_match or not method_match:
        raise ValueError(f"could not parse driver output:\n{output}")
    count, ok, canceled, null, errors = map(
        int, method_match.groups()[:5]
    )
    if count != expected_count:
        raise ValueError(
            f"expected {expected_count} responses, got {count}"
        )
    if ok != count or canceled or null or errors:
        raise ValueError(
            "driver reported non-success responses: "
            f"count={count} ok={ok} canceled={canceled} "
            f"null={null} errors={errors}"
        )
    tokens_per_request = int(tokens_match.group(1))
    if tokens_per_request <= 0:
        raise ValueError("driver reported an empty semantic-token workload")
    return {
        "count": count,
        "ok": ok,
        "canceled": canceled,
        "null": null,
        "errors": errors,
        "tokens_per_request": tokens_per_request,
        "p50": float(method_match.group(6)),
        "p90": float(method_match.group(7)),
        "p95": float(method_match.group(8)),
        "p99": float(method_match.group(9)),
        "max": float(method_match.group(10)),
        "wall": float(wall_match.group(1)),
        "wire_kib": float(method_match.group(11)),
    }


def parse_method_summary(output, method, expected_count):
    match = re.search(
        rf"method={re.escape(method)} "
        r"count=(\d+) ok=(\d+) canceled=(\d+) null=(\d+) errors=(\d+) "
        r"p50=([\d.]+)ms p90=([\d.]+)ms p95=([\d.]+)ms "
        r"p99=([\d.]+)ms max=([\d.]+)ms wire=([\d.]+)KiB",
        output,
    )
    if not match:
        raise ValueError(f"could not parse {method} summary:\n{output}")
    count, ok, canceled, null, errors = map(int, match.groups()[:5])
    if count != expected_count:
        raise ValueError(
            f"expected {expected_count} {method} responses, got {count}"
        )
    if ok != count or canceled or null or errors:
        raise ValueError(
            f"{method} reported non-success responses: "
            f"count={count} ok={ok} canceled={canceled} "
            f"null={null} errors={errors}"
        )
    return {
        "count": count,
        "ok": ok,
        "canceled": canceled,
        "null": null,
        "errors": errors,
        "p50": float(match.group(6)),
        "p90": float(match.group(7)),
        "p95": float(match.group(8)),
        "p99": float(match.group(9)),
        "max": float(match.group(10)),
        "wire_kib": float(match.group(11)),
    }


def parse_capture_pilot_summary(output, expected_count):
    semantic = parse_method_summary(
        output, "textDocument/semanticTokens/full", expected_count
    )
    captures = parse_method_summary(
        output, "kakehashi/captures/full/delta", expected_count
    )
    validation = re.search(
        r"capture-validation count=(\d+) delta_shapes=(\d+) "
        r"lineage_advances=(\d+) full_fallbacks=(\d+)",
        output,
    )
    if not validation:
        raise ValueError("could not parse capture validation summary")
    validation_counts = tuple(map(int, validation.groups()))
    if validation_counts != (expected_count, expected_count, expected_count, 0):
        raise ValueError(
            "capture validation did not prove every measured delta: "
            f"{validation_counts}"
        )
    return {
        "semantic_p50": semantic["p50"],
        "semantic_p95": semantic["p95"],
        "captures_p50": captures["p50"],
        "captures_p95": captures["p95"],
        "semantic_outcomes": {
            key: semantic[key]
            for key in ("count", "ok", "canceled", "null", "errors")
        },
        "capture_outcomes": {
            key: captures[key]
            for key in ("count", "ok", "canceled", "null", "errors")
        },
        "capture_validation_count": validation_counts[0],
        "capture_delta_shapes": validation_counts[1],
        "capture_lineage_advances": validation_counts[2],
        "capture_full_fallbacks": validation_counts[3],
    }


def sha256_file(path):
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def verify_file_sha256(path, expected):
    actual = sha256_file(path)
    if actual != expected:
        raise RuntimeError(
            f"binary changed during collection: expected={expected} actual={actual}"
        )


def load_binary_attestation(path, binary):
    attestation = json.loads(path.read_text())
    required = (
        "schema", "source_repository", "source_commit",
        "source_checkout_clean", "build_command", "rustc", "cargo",
        "build_environment", "built_in_fresh_target", "binary_relative",
        "binary_sha256",
    )
    if any(key not in attestation for key in required):
        raise ValueError("binary attestation is missing required fields")
    valid_schema = (
        attestation["schema"] == 1
        and isinstance(attestation["source_repository"], str)
        and bool(attestation["source_repository"])
        and isinstance(attestation["source_commit"], str)
        and re.fullmatch(r"[0-9a-f]{40}", attestation["source_commit"])
        and attestation["source_checkout_clean"] is True
        and attestation["build_command"] == ATTESTED_BUILD_COMMAND
        and isinstance(attestation["rustc"], str)
        and bool(attestation["rustc"])
        and isinstance(attestation["cargo"], str)
        and bool(attestation["cargo"])
        and isinstance(attestation["build_environment"], dict)
        and "PATH" in attestation["build_environment"]
        and "CARGO_TARGET_DIR" in attestation["build_environment"]
        and all(
            isinstance(key, str) and isinstance(value, str)
            for key, value in attestation["build_environment"].items()
        )
        and not (
            set(attestation["build_environment"])
            - {
                "PATH", "SYSTEMROOT", "WINDIR", "TMPDIR", "TMP", "TEMP",
                "LANG", "LC_ALL", "SDKROOT", "MACOSX_DEPLOYMENT_TARGET",
                "CARGO_TARGET_DIR",
            }
        )
        and attestation["built_in_fresh_target"] is True
        and attestation["binary_relative"] == "target/release/kakehashi"
        and isinstance(attestation["binary_sha256"], str)
        and re.fullmatch(r"[0-9a-f]{64}", attestation["binary_sha256"])
    )
    if not valid_schema:
        raise ValueError("binary attestation schema is invalid")
    actual = sha256_file(binary)
    if attestation["binary_sha256"] != actual:
        raise ValueError(
            "attested binary digest does not match measured binary: "
            f"attested={attestation['binary_sha256']} actual={actual}"
        )
    return attestation


def runtime_artifact_files(root):
    files = []
    for relative_root in ("cache", "parser", "queries"):
        artifact_root = root / relative_root
        if not artifact_root.exists():
            continue
        files.extend(
            item for item in artifact_root.rglob("*")
            if item.is_file() and not item.is_symlink()
        )
    return sorted(
        files, key=lambda item: item.relative_to(root).as_posix()
    )


def parser_library_suffix(system_name=platform.system()):
    return ".dylib" if system_name == "Darwin" else ".so"


def require_benchmark_artifacts(root):
    required_files = [root / "cache/parsers.lua"]
    for language in ("rust", "markdown", "markdown_inline", "lua", "python"):
        required_files.append(root / f"queries/{language}/highlights.scm")
        required_files.append(
            root / f"parser/{language}{parser_library_suffix()}"
        )
    required_files.append(root / "queries/markdown/injections.scm")
    for path in required_files:
        if not path.is_file() or path.is_symlink():
            raise ValueError(f"missing benchmark artifact: {path}")


def artifact_identity(root):
    files = runtime_artifact_files(root)
    return len(files), shasum_tree_digest(root)


def shasum_tree_digest(root):
    """Match `find . | sort | xargs shasum | shasum` from the report."""
    digest = hashlib.sha256()
    for path in runtime_artifact_files(root):
        relative = path.relative_to(root).as_posix()
        digest.update(f"{sha256_file(path)}  ./{relative}\n".encode())
    return digest.hexdigest()


def cpu_model():
    try:
        return tool_version(["sysctl", "-n", "machdep.cpu.brand_string"])
    except (FileNotFoundError, subprocess.CalledProcessError):
        return platform.processor() or "unknown"


def tool_version(command):
    return subprocess.run(
        command, check=True, text=True, stdout=subprocess.PIPE
    ).stdout.strip()


def build_driver_command(kind, binary, data_dir, scenario_args, script_dir):
    if kind == "direct":
        server = str(binary)
        server_args = []
    else:
        server = sys.executable
        server_args = [
            "--server-arg", str(script_dir / "worker_proxy.py")
        ]
    return [
        sys.executable,
        str(script_dir / "drive.py"),
        "--bin",
        server,
        *server_args,
        "--data-dir",
        str(data_dir),
        *scenario_args,
    ]


def signal_process_group(process, signum):
    try:
        os.killpg(process.pid, signum)
    except ProcessLookupError:
        pass


def terminate_process_group(process, grace_seconds):
    signal_process_group(process, signal.SIGTERM)
    try:
        stdout, stderr = process.communicate(timeout=grace_seconds)
    except subprocess.TimeoutExpired:
        stdout = stderr = None
    signal_process_group(process, signal.SIGKILL)
    final_stdout, final_stderr = process.communicate()
    return (
        stdout if stdout is not None else final_stdout,
        stderr if stderr is not None else final_stderr,
    )


def install_termination_handlers():
    previous = {}

    def exit_on_signal(signum, _frame):
        raise SystemExit(128 + signum)

    for signum in TERMINATION_SIGNALS:
        previous[signum] = signal.getsignal(signum)
        signal.signal(signum, exit_on_signal)
    return previous


def restore_signal_handlers(previous):
    for signum, handler in previous.items():
        signal.signal(signum, handler)


def suppress_termination_signals():
    for signum in TERMINATION_SIGNALS:
        signal.signal(signum, signal.SIG_IGN)


def unblock_termination_signals_in_child():
    signal.pthread_sigmask(signal.SIG_UNBLOCK, TERMINATION_SIGNALS)


def bounded_run(
    command,
    environment,
    timeout_seconds,
    termination_grace_seconds=3,
):
    process = None
    previous_mask = signal.pthread_sigmask(
        signal.SIG_BLOCK, TERMINATION_SIGNALS
    )
    try:
        process = subprocess.Popen(
            command,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=environment,
            start_new_session=True,
            preexec_fn=unblock_termination_signals_in_child,
        )
        previous_handlers = install_termination_handlers()
    except BaseException:
        try:
            if process is not None:
                signal.pthread_sigmask(signal.SIG_BLOCK, CLEANUP_SIGNALS)
                terminate_process_group(process, termination_grace_seconds)
        finally:
            signal.pthread_sigmask(signal.SIG_SETMASK, previous_mask)
        raise
    try:
        try:
            signal.pthread_sigmask(signal.SIG_SETMASK, previous_mask)
            stdout, stderr = process.communicate(timeout=timeout_seconds)
        except subprocess.TimeoutExpired as timeout:
            signal.pthread_sigmask(signal.SIG_BLOCK, CLEANUP_SIGNALS)
            suppress_termination_signals()
            stdout, stderr = terminate_process_group(
                process, termination_grace_seconds
            )
            raise subprocess.TimeoutExpired(
                command,
                timeout_seconds,
                output=stdout,
                stderr=stderr,
            ) from timeout
        except BaseException:
            signal.pthread_sigmask(signal.SIG_BLOCK, CLEANUP_SIGNALS)
            suppress_termination_signals()
            terminate_process_group(process, termination_grace_seconds)
            raise
    finally:
        signal.pthread_sigmask(signal.SIG_SETMASK, previous_mask)
        restore_signal_handlers(previous_handlers)
    if process.returncode:
        raise subprocess.CalledProcessError(
            process.returncode, command, output=stdout, stderr=stderr
        )
    return subprocess.CompletedProcess(
        command, process.returncode, stdout=stdout, stderr=stderr
    )


def option_int(arguments, option, default):
    for index, argument in enumerate(arguments):
        if argument == option:
            return int(arguments[index + 1])
        prefix = f"{option}="
        if argument.startswith(prefix):
            return int(argument.removeprefix(prefix))
    return default


def run_driver(
    kind, binary, data_dir, scenario_args, script_dir, timeout_seconds
):
    env = controlled_environment(os.environ)
    if kind == "relay":
        env["KAKEHASHI_WORKER_PROXY_BIN"] = str(binary)
    command = build_driver_command(
        kind, binary, data_dir, scenario_args, script_dir
    )
    completed = bounded_run(
        command, env, timeout_seconds=timeout_seconds
    )
    expected_count = option_int(scenario_args, "--requests", 300) * option_int(
        scenario_args, "--burst", 1
    )
    return parse_driver_summary(completed.stderr, expected_count)


def main():
    require_posix()
    parser = argparse.ArgumentParser()
    parser.add_argument("--bin", type=pathlib.Path, required=True)
    parser.add_argument("--data-dir", type=pathlib.Path, required=True)
    parser.add_argument("--output", type=pathlib.Path, required=True)
    parser.add_argument(
        "--binary-attestation", type=pathlib.Path, required=True
    )
    parser.add_argument("--pairs", type=int, default=10)
    parser.add_argument(
        "--run-timeout",
        type=float,
        default=60,
        help="seconds before a driver and its process group are terminated",
    )
    parser.add_argument(
        "--nvim-treesitter-checkout",
        type=pathlib.Path,
        required=True,
        help="Git checkout whose HEAD supplies parser metadata and queries",
    )
    parser.add_argument(
        "--scenario",
        action="append",
        choices=sorted(SCENARIOS),
        dest="scenarios",
    )
    args = parser.parse_args()
    if args.pairs <= 0:
        parser.error("--pairs must be positive")
    if args.run_timeout <= 0:
        parser.error("--run-timeout must be positive")

    script_dir = pathlib.Path(__file__).resolve().parent
    selected = args.scenarios or list(SCENARIOS)
    require_benchmark_artifacts(args.data_dir)
    initial_artifact_identity = artifact_identity(args.data_dir)
    initial_binary_sha256 = sha256_file(args.bin)
    binary_attestation = load_binary_attestation(
        args.binary_attestation, args.bin
    )
    logical_cpus = os.cpu_count() or 1
    result = {
        "schema": 1,
        "experiment": "single-tree-worker-phase0-raw-relay",
        "environment": {
            "platform": platform.platform(),
            "python": platform.python_version(),
            "rustc": tool_version(["rustc", "--version"]),
            "cpu_model": cpu_model(),
            "logical_cpus": logical_cpus,
            "estimated_tree_compute_budget": estimated_tree_compute_budget(
                logical_cpus
            ),
            "estimated_tree_compute_budget_source": (
                "os.cpu_count approximation of available_parallelism - 2 policy; "
                "not the binary's reported effective pool size"
            ),
            "binary": str(args.bin.resolve()),
            "binary_sha256": initial_binary_sha256,
            "data_dir": str(args.data_dir.resolve()),
            "parser_query_file_count": initial_artifact_identity[0],
            "parser_query_tree_sha256": initial_artifact_identity[1],
            "retained_environment": controlled_environment(os.environ),
        },
        "artifacts": artifact_provenance(
            args.data_dir, args.nvim_treesitter_checkout
        ),
        "binary_attestation": binary_attestation,
        "collector": {
            "pairs": args.pairs,
            "order": "direct-relay on odd runs; relay-direct on even runs",
        },
        "steady_state": {},
    }
    for scenario in selected:
        pairs = []
        for index in range(args.pairs):
            pair = {
                "run": index + 1,
                "order": "-".join(run_order(index)),
            }
            for kind in run_order(index):
                pair[kind] = run_driver(
                    kind,
                    args.bin,
                    args.data_dir,
                    SCENARIOS[scenario],
                    script_dir,
                    args.run_timeout,
                )
            pairs.append(pair)
            print(f"{scenario}: pair {index + 1}/{args.pairs}", file=sys.stderr)
        result["steady_state"][scenario] = {
            "arguments": SCENARIOS[scenario],
            "pairs": pairs,
        }
    final_artifact_identity = artifact_identity(args.data_dir)
    if final_artifact_identity != initial_artifact_identity:
        raise RuntimeError(
            "runtime artifacts changed during collection: "
            f"before={initial_artifact_identity} after={final_artifact_identity}"
        )
    verify_file_sha256(args.bin, initial_binary_sha256)
    args.output.write_text(json.dumps(result, indent=2) + "\n")


if __name__ == "__main__":
    main()

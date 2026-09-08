"""Validate the complete scenario inventory before consuming retained records."""
import itertools
import json
from pathlib import PurePosixPath
from collections import Counter
from urllib.parse import parse_qs, urlsplit


def require_identity(matrix, run, result):
    """Bind portable result records to their declared fixture and invocation."""
    arguments = matrix["arguments"]
    command = run["command"]
    # The runner resolves paths for the invocation, while matrix CLI arguments
    # may remain relative. Do not resolve archived paths against today's cwd.
    fixture_path = PurePosixPath(command[command.index("--file") + 1])
    output_path = PurePosixPath(command[command.index("--json-output") + 1])
    inputs = fixture_path.parent
    extension = "rs" if run["language"] == "rust" else "md"
    fixture = f"{run['language']}-{run['size']}.{extension}"
    fixture_hashes = {item["path"]: item["sha256"] for item in matrix["inputs"]["fixtures"]}
    binary = matrix["binaries"][run["variant"]]
    expected = {
        "bin": binary["path"], "file": str(inputs / fixture),
        "server_arg": ["--config-file", str(inputs / (run["mode"] + ".toml"))],
        "data_dir": matrix["inputs"]["runtime"],
        "requests": arguments["requests"], "warmup": arguments["warmup"],
        "documents": run["documents"], "edits": 1, "edit_delay_ms": 0,
        "tag_document_uris": True,
        "json_output": str(output_path),
    }
    if (fixture_path.name != fixture or output_path.name != run["name"] + ".json"
            or result.get("binary_sha256") != binary["sha256"]
            or result.get("fixture_sha256") != fixture_hashes[fixture]
            or any(result.get("arguments", {}).get(key) != value for key, value in expected.items())):
        raise ValueError(f"Result identity does not match declared measurement: {run['name']}")


def require_comparable(run_name, result):
    samples = result["request_samples"]["textDocument/semanticTokens/full"]
    if not samples or any(sample["status"] != "ok" or (sample.get("token_count") or 0) <= 0
                          for sample in samples):
        raise ValueError(f"Cannot compare {run_name}: every response must be successful and nonempty")


def bridge_coverage(run, result):
    """Check session-level opens, including warmup, independently for each host."""
    language, mode = run["language"], run["mode"]
    expected = set()
    if mode == "wildcard":
        expected = {"comment"} if language == "rust" else {"markdown_inline", "lua"}
    elif mode == "narrow" and language == "markdown":
        expected = {"lua"}
    if not expected:
        return {}

    def document_tag(uri):
        values = parse_qs(urlsplit(uri).query, keep_blank_values=True).get("kakehashi-profile-document", [])
        return values[0] if len(values) == 1 and values[0] else None

    host_uris = result["document_uris"]
    tags = [document_tag(uri) for uri in host_uris]
    if None in tags or len(set(tags)) != len(host_uris):
        raise ValueError("bridge evidence requires distinct host document tags")
    opened = {(document["uri"], document["language"])
              for peer in run["peers"] for document in peer.get("opened_documents", [])}
    coverage = {}
    for host_uri, tag in zip(host_uris, tags):
        host_open = (host_uri, language) in opened
        if mode == "wildcard" and not host_open:
            raise ValueError(f"intended bridge host open missing for {host_uri}")
        injected = {opened_language for uri, opened_language in opened
                    if uri != host_uri and document_tag(uri) == tag
                    and urlsplit(uri).path.rsplit("/", 1)[-1].startswith("kakehashi-virtual-uri-")}
        if expected - injected:
            raise ValueError(f"intended bridge injections missing for {host_uri}: {sorted(expected - injected)}")
        coverage[host_uri] = {"host_open": host_open, "expected_injected_languages": sorted(expected),
                              "injected_languages": sorted(injected)}
    return coverage


def require_workload(run, result, requests):
    documents = run["documents"]
    prefix = f"Incomplete workload for {run['name']}: "
    if requests < 1 or documents < 1:
        raise ValueError(prefix + "invalid declared workload")
    if len(result["cycle_seconds"]) != requests:
        raise ValueError(prefix + "cycle count does not match declared requests")
    samples = result["request_samples"]["textDocument/semanticTokens/full"]
    if len(samples) != requests * documents:
        raise ValueError(prefix + "response count does not match requests times documents")
    uris = result["document_uris"]
    if len(uris) != documents or len(set(uris)) != documents:
        raise ValueError(prefix + "document inventory has duplicate or missing URIs")
    if Counter(sample["uri"] for sample in samples) != Counter({uri: requests for uri in uris}):
        raise ValueError(prefix + "per-document response count does not match declared requests")


def load_matrix(path):
    matrix = json.loads(path.read_text())
    if matrix.get("status") != "complete":
        raise ValueError("Matrix has no successful final verification")
    arguments = matrix["arguments"]
    for name in ("sizes", "modes", "documents"):
        values = arguments[name]
        if not values or len(values) != len(set(values)):
            raise ValueError(f"Invalid scenario dimension: {name}")
    if arguments["repeats"] < 1:
        raise ValueError("Invalid scenario repetition count")
    expected = set()
    for dimensions in itertools.product(("rust", "markdown"), arguments["sizes"],
                                        arguments["modes"], arguments["documents"]):
        for repetition in range(arguments["repeats"]):
            for variant in ("baseline", "candidate"):
                expected.add((*dimensions, variant, repetition, False))
        expected.add((*dimensions, "candidate", 0, True))
    fields = ("language", "size", "mode", "documents", "variant", "repetition", "profile")
    keys = [tuple(run[field] for field in fields) for run in matrix["runs"]]
    if len(keys) != len(expected) or len(set(keys)) != len(keys) or set(keys) != expected:
        raise ValueError("Duplicate, missing, or unexpected scenario records")
    for run, key in zip(matrix["runs"], keys):
        language, size, mode, documents, variant, repetition, profile = key
        name = f"{language}-{size}-{mode}-d{documents}-{variant}-{repetition}"
        if profile:
            name += "-profile"
        if run["name"] != name:
            raise ValueError("Scenario artifact name does not match its identity")
        if run["status"] != "complete":
            raise ValueError("Refusing to consume an incomplete scenario")
    return matrix

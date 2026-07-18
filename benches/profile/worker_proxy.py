#!/usr/bin/env python3
"""Add one resident child-process and pipe hop in front of kakehashi.

This is a transport lower-bound experiment, not the proposed worker protocol:
it relays the existing framed LSP byte stream without decoding it. Set
``KAKEHASHI_WORKER_PROXY_BIN`` to the real kakehashi executable. All command-line
arguments are forwarded to that child.
"""

import os
import subprocess
import sys
import threading


def copy_stream(source, destination, chunk_size=64 * 1024):
    copied = 0
    read = getattr(source, "read1", source.read)
    while chunk := read(chunk_size):
        destination.write(chunk)
        destination.flush()
        copied += len(chunk)
    return copied


def main():
    child_bin = os.environ.get("KAKEHASHI_WORKER_PROXY_BIN")
    if not child_bin:
        raise SystemExit("KAKEHASHI_WORKER_PROXY_BIN is required")

    child = subprocess.Popen(
        [child_bin, *sys.argv[1:]],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
    )
    stdin_thread = threading.Thread(
        target=copy_stream,
        args=(sys.stdin.buffer, child.stdin),
        daemon=True,
    )
    stdout_thread = threading.Thread(
        target=copy_stream,
        args=(child.stdout, sys.stdout.buffer),
    )
    stdin_thread.start()
    stdout_thread.start()
    try:
        return_code = child.wait()
        stdout_thread.join()
        return return_code
    finally:
        if child.poll() is None:
            child.terminate()
            try:
                child.wait(timeout=1)
            except subprocess.TimeoutExpired:
                child.kill()
                child.wait()


if __name__ == "__main__":
    raise SystemExit(main())

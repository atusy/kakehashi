#!/usr/bin/env python3
"""Add one resident child-process and pipe hop in front of kakehashi.

This is a raw-relay feasibility experiment, not the proposed worker protocol:
it relays the existing framed LSP byte stream without decoding it. Set
``KAKEHASHI_WORKER_PROXY_BIN`` to the real kakehashi executable. All command-line
arguments are forwarded to that child.
"""

import os
import signal
import subprocess
import sys
import threading


def require_posix(platform_name=os.name):
    if platform_name != "posix":
        raise SystemExit("worker_proxy Phase 0 relay requires POSIX lifecycle semantics")


def copy_stream(
    source, destination, chunk_size=64 * 1024, close_destination=False
):
    copied = 0
    read = getattr(source, "read1", source.read)
    try:
        while chunk := read(chunk_size):
            destination.write(chunk)
            destination.flush()
            copied += len(chunk)
        return copied
    except OSError:
        return copied
    finally:
        if close_destination:
            try:
                destination.close()
            except OSError:
                pass


def terminate_child(child):
    if child.poll() is not None:
        return
    try:
        child.terminate()
        try:
            child.wait(timeout=1)
        except subprocess.TimeoutExpired:
            child.kill()
            child.wait()
    except OSError:
        # The child may exit between poll and signal delivery.
        pass


def main():
    require_posix()
    child_bin = os.environ.get("KAKEHASHI_WORKER_PROXY_BIN")
    if not child_bin:
        raise SystemExit("KAKEHASHI_WORKER_PROXY_BIN is required")

    def exit_after_cleanup(signum, _frame):
        raise SystemExit(128 + signum)

    signal.signal(signal.SIGTERM, exit_after_cleanup)

    child = subprocess.Popen(
        [child_bin, *sys.argv[1:]],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
    )
    stdin_thread = threading.Thread(
        target=copy_stream,
        # Read the raw descriptor so a blocked daemon thread cannot retain the
        # BufferedReader lock while the proxy exits after its child.
        args=(
            getattr(sys.stdin.buffer, "raw", sys.stdin.buffer),
            child.stdin,
            64 * 1024,
            True,
        ),
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
        terminate_child(child)


if __name__ == "__main__":
    raise SystemExit(main())

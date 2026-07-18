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
    except ProcessLookupError:
        # The child may exit between poll and signal delivery.
        pass


def unblock_sigterm_in_child():
    signal.pthread_sigmask(signal.SIG_UNBLOCK, (signal.SIGTERM,))


def run_relay(child, input_stream, output_stream):
    try:
        stdin_thread = threading.Thread(
            target=copy_stream,
            # Read the raw descriptor so a blocked daemon thread cannot retain
            # the BufferedReader lock while the proxy exits after its child.
            args=(
                getattr(input_stream, "raw", input_stream),
                child.stdin,
                64 * 1024,
                True,
            ),
            daemon=True,
        )
        stdout_thread = threading.Thread(
            target=copy_stream,
            args=(child.stdout, output_stream),
            daemon=True,
        )
        stdin_thread.start()
        stdout_thread.start()
        return_code = child.wait()
        stdout_thread.join(timeout=2.0)
        return return_code
    finally:
        terminate_child(child)


def main():
    require_posix()
    child_bin = os.environ.get("KAKEHASHI_WORKER_PROXY_BIN")
    if not child_bin:
        raise SystemExit("KAKEHASHI_WORKER_PROXY_BIN is required")

    def exit_after_cleanup(signum, _frame):
        # The first termination owns cleanup. Repeats must not interrupt the
        # bounded TERM -> KILL -> reap sequence below.
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
        raise SystemExit(128 + signum)

    signal.signal(signal.SIGTERM, exit_after_cleanup)

    previous_mask = signal.pthread_sigmask(
        signal.SIG_BLOCK, (signal.SIGTERM,)
    )
    try:
        child = subprocess.Popen(
            [child_bin, *sys.argv[1:]],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            preexec_fn=unblock_sigterm_in_child,
        )
    except BaseException:
        signal.pthread_sigmask(signal.SIG_SETMASK, previous_mask)
        raise
    try:
        signal.pthread_sigmask(signal.SIG_SETMASK, previous_mask)
        return run_relay(child, sys.stdin.buffer, sys.stdout.buffer)
    finally:
        signal.pthread_sigmask(signal.SIG_SETMASK, previous_mask)
        terminate_child(child)


if __name__ == "__main__":
    raise SystemExit(main())

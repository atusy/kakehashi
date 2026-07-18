import io
import os
import pathlib
import signal
import subprocess
import sys
import time
import unittest
from unittest import mock

sys.path.insert(0, str(pathlib.Path(__file__).parent))

from worker_proxy import copy_stream, require_posix, run_relay, terminate_child
from process_test_utils import read_count_with_timeout, readline_with_timeout


class CopyStreamTest(unittest.TestCase):
    def test_stdout_relay_cannot_block_process_exit(self):
        child = mock.Mock()
        child.stdin = io.BytesIO()
        child.stdout = io.BytesIO()
        child.wait.return_value = 0
        child.poll.return_value = 0
        stdin_thread = mock.Mock()
        stdout_thread = mock.Mock()

        with mock.patch(
            "worker_proxy.threading.Thread",
            side_effect=(stdin_thread, stdout_thread),
        ) as thread:
            self.assertEqual(run_relay(child, io.BytesIO(), io.BytesIO()), 0)

        self.assertTrue(thread.call_args_list[1].kwargs["daemon"])
        stdout_thread.join.assert_called_once_with(timeout=2.0)

    def test_thread_setup_interruption_reaps_child(self):
        class Child:
            stdin = io.BytesIO()
            stdout = io.BytesIO()

            def __init__(self):
                self.terminated = False

            def poll(self):
                return None

            def terminate(self):
                self.terminated = True

            def wait(self, timeout=None):
                return 0

        child = Child()

        with mock.patch(
            "worker_proxy.threading.Thread",
            side_effect=SystemExit(128 + signal.SIGTERM),
        ):
            with self.assertRaises(SystemExit):
                run_relay(child, io.BytesIO(), io.BytesIO())

        self.assertTrue(child.terminated)

    def test_relay_rejects_non_posix_lifecycle(self):
        with self.assertRaisesRegex(SystemExit, "POSIX"):
            require_posix("nt")

    def test_copies_until_eof_and_flushes_each_chunk(self):
        source = io.BytesIO(b"Content-Length: 2\r\n\r\n{}")
        destination = io.BytesIO()

        copied = copy_stream(source, destination, chunk_size=5)

        self.assertEqual(copied, len(source.getvalue()))
        self.assertEqual(destination.getvalue(), source.getvalue())

    def test_stream_reset_ends_relay_without_traceback(self):
        class ResettingDestination(io.BytesIO):
            def flush(self):
                raise ConnectionResetError("peer reset")

        copied = copy_stream(io.BytesIO(b"payload"), ResettingDestination())

        self.assertEqual(copied, 0)

    def test_close_reset_ends_relay_without_traceback(self):
        class CloseResettingDestination(io.BytesIO):
            def close(self):
                raise ConnectionResetError("peer reset while closing")

        copied = copy_stream(
            io.BytesIO(b"payload"),
            CloseResettingDestination(),
            close_destination=True,
        )

        self.assertEqual(copied, len(b"payload"))

    def test_cleanup_tolerates_child_exit_after_poll(self):
        class ExitedChild:
            def poll(self):
                return None

            def terminate(self):
                raise ProcessLookupError("already exited")

        terminate_child(ExitedChild())

    def test_cleanup_does_not_hide_permission_failure(self):
        child = mock.Mock()
        child.poll.return_value = None
        child.terminate.side_effect = PermissionError("denied")

        with self.assertRaises(PermissionError):
            terminate_child(child)

    def test_child_may_exit_while_proxy_stdin_remains_open(self):
        proxy = pathlib.Path(__file__).with_name("worker_proxy.py")
        env = dict(os.environ, KAKEHASHI_WORKER_PROXY_BIN=sys.executable)
        process = subprocess.Popen(
            [
                sys.executable,
                str(proxy),
                "-c",
                "import sys; sys.stdout.buffer.write(b'ok'); sys.stdout.flush()",
            ],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=env,
        )
        try:
            self.assertEqual(read_count_with_timeout(process.stdout, 2), b"ok")
            return_code = process.wait(timeout=5)
            stderr = process.stderr.read().decode()
        except subprocess.TimeoutExpired:
            terminate_child(process)
            raise
        finally:
            process.stdin.close()
            process.stdout.close()
            process.stderr.close()

        self.assertEqual(return_code, 0, stderr)
        self.assertNotIn("Fatal Python error", stderr)

    def test_proxy_forwards_stdin_eof_to_child(self):
        proxy = pathlib.Path(__file__).with_name("worker_proxy.py")
        env = dict(os.environ, KAKEHASHI_WORKER_PROXY_BIN=sys.executable)

        completed = subprocess.run(
            [
                sys.executable,
                str(proxy),
                "-c",
                "import sys; data=sys.stdin.buffer.read(); print(len(data))",
            ],
            input=b"abc",
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=env,
            timeout=5,
        )

        self.assertEqual(completed.returncode, 0, completed.stderr.decode())
        self.assertEqual(completed.stdout, b"3\n")

    @unittest.skipUnless(
        os.name == "posix" and hasattr(signal, "SIGTERM"),
        "requires POSIX SIGTERM",
    )
    def test_terminating_proxy_reaps_child(self):
        proxy = pathlib.Path(__file__).with_name("worker_proxy.py")
        env = dict(os.environ, KAKEHASHI_WORKER_PROXY_BIN=sys.executable)
        process = subprocess.Popen(
            [
                sys.executable,
                str(proxy),
                "-c",
                "import os,time; print(os.getpid(), flush=True); time.sleep(30)",
            ],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=env,
            text=True,
        )
        child_pid = int(readline_with_timeout(process.stdout))
        try:
            process.terminate()
            process.wait(timeout=5)
            with self.assertRaises(ProcessLookupError):
                os.kill(child_pid, 0)
        finally:
            terminate_child(process)
            try:
                os.kill(child_pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            process.stdin.close()
            process.stdout.close()
            process.stderr.close()

    @unittest.skipUnless(
        os.name == "posix" and hasattr(signal, "SIGTERM"),
        "requires POSIX SIGTERM",
    )
    def test_repeated_termination_reaps_term_ignoring_child(self):
        proxy = pathlib.Path(__file__).with_name("worker_proxy.py")
        env = dict(os.environ, KAKEHASHI_WORKER_PROXY_BIN=sys.executable)
        process = subprocess.Popen(
            [
                sys.executable,
                str(proxy),
                "-c",
                "import os,signal,time; "
                "signal.signal(signal.SIGTERM, signal.SIG_IGN); "
                "print(os.getpid(), flush=True); time.sleep(30)",
            ],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=env,
            text=True,
        )
        child_pid = int(readline_with_timeout(process.stdout))
        try:
            for _ in range(8):
                if process.poll() is not None:
                    break
                process.terminate()
                time.sleep(0.08)
            process.wait(timeout=5)
            with self.assertRaises(ProcessLookupError):
                os.kill(child_pid, 0)
        finally:
            terminate_child(process)
            try:
                os.kill(child_pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            process.stdin.close()
            process.stdout.close()
            process.stderr.close()


if __name__ == "__main__":
    unittest.main()

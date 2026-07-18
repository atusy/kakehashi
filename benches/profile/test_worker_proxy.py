import io
import os
import pathlib
import subprocess
import sys
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).parent))

from worker_proxy import copy_stream


class CopyStreamTest(unittest.TestCase):
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
            self.assertEqual(process.stdout.read(2), b"ok")
            return_code = process.wait(timeout=5)
            stderr = process.stderr.read().decode()
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


if __name__ == "__main__":
    unittest.main()

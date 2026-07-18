import io
import pathlib
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


if __name__ == "__main__":
    unittest.main()

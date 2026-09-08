"""Check the controlled peer's actual wire observations and shutdown artifact."""
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


class BridgeFixtureTest(unittest.TestCase):
    def test_records_opened_uri_and_language_before_shutdown(self):
        opened = [
            {"uri": "file:///profile/input-0.md?kakehashi-profile-document=0", "language": "markdown"},
            {"uri": "file:///profile/kakehashi-virtual-uri-R0.lua?kakehashi-profile-document=0", "language": "lua"},
            {"uri": "file:///profile/kakehashi-virtual-uri-R1.lua?kakehashi-profile-document=1", "language": "lua"},
        ]
        messages = [{"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}]
        messages.extend({"jsonrpc": "2.0", "method": "textDocument/didOpen", "params": {
            "textDocument": {"uri": document["uri"], "languageId": document["language"],
                             "version": 1, "text": ""}}} for document in opened)
        messages.extend([{"jsonrpc": "2.0", "id": 2, "method": "shutdown", "params": None},
                         {"jsonrpc": "2.0", "method": "exit"}])
        wire = b""
        for message in messages:
            body = json.dumps(message).encode("utf-8")
            wire += f"Content-Length: {len(body)}\r\n\r\n".encode() + body
        with tempfile.TemporaryDirectory() as directory:
            result = subprocess.run([
                sys.executable, str(Path(__file__).with_name("bridge_fixture.py")),
                "--summary-dir", directory,
            ], input=wire, capture_output=True, timeout=10)
            self.assertEqual(result.returncode, 0, result.stderr.decode())
            summaries = list(Path(directory).glob("*.json"))
            self.assertEqual(len(summaries), 1)
            summary = json.loads(summaries[0].read_text())
            self.assertEqual(summary.get("opened_documents"), sorted(opened, key=lambda item: (item["uri"], item["language"])))
            self.assertEqual(summary["opened_languages"], {"markdown": 1, "lua": 2})
            self.assertEqual(summary["methods"]["shutdown"], 1)
            self.assertNotIn("exit", summary["methods"])


if __name__ == "__main__":
    unittest.main()

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

HOOK = Path(__file__).with_name("rtk_after.py")


@unittest.skipUnless(os.open in os.supports_dir_fd, "requires Unix directory-relative opens")
class RtkHookSmoke(unittest.TestCase):
    def test_filter_and_passthrough(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp).resolve()
            spill = root / "raw output.txt"
            raw = "running 1 test\ntest demo ... ok\ntest result: ok. 1 passed; 0 failed; 0 ignored;\n"
            spill.write_text(raw)
            rtk = root / "rtk"
            rtk.write_text("#!/usr/bin/env python3\nimport sys\nassert sys.argv[1:] == ['pipe', '--filter', 'cargo-test']\nassert 'test result: ok.' in sys.stdin.read()\nprint('1 test passed')\n")
            rtk.chmod(0o755)
            env = dict(os.environ, PATH=str(root) + os.pathsep + os.environ.get("PATH", ""))
            request = {"hook": "tool.execute.after", "context": {"tool": "bash"}, "input": {"status": "completed", "args": {"command": "cargo test"}, "output": "preview\n" * 30, "title": "shell", "metadata": {"exit": 0, "truncated": True, "outputPath": str(spill), "outputBytes": len(raw)}}}

            def run(value):
                result = subprocess.run([sys.executable, str(HOOK)], input=json.dumps(value), text=True, capture_output=True, check=True, env=env)
                return json.loads(result.stdout)

            response = run(request)
            self.assertEqual(response, {"set": {"output": "1 test passed\n\n[Original raw tool output: " + str(spill) + "]"}})
            self.assertEqual(spill.read_text(), raw)
            for link, make_link in [
                (root / "symlink", lambda p: p.symlink_to(spill)),
                (root / "hardlink", lambda p: os.link(spill, p)),
            ]:
                make_link(link)
                case = json.loads(json.dumps(request))
                case["input"]["metadata"]["outputPath"] = str(link)
                self.assertEqual(run(case), {})
                link.unlink()
            with spill.open("wb") as log:
                log.truncate(10 * 1024 * 1024 + 1)
            self.assertEqual(run(request), {})
            spill.write_text(raw)
            self.assertEqual(run([]), {})
            for change in (lambda r: r["input"]["metadata"].update(exit=1),
                           lambda r: r["input"]["args"].update(command="cargo test; echo unsafe"),
                           lambda r: r["input"]["metadata"].update(outputPath="/missing"),
                           lambda r: r["input"].update(status="error"),
                           lambda r: r["context"].update(tool="read")):
                case = json.loads(json.dumps(request))
                change(case)
                self.assertEqual(run(case), {})
            pytest_case = json.loads(json.dumps(request))
            pytest_case["context"]["tool"] = "shell"
            pytest_case["input"]["args"]["command"] = "pytest"
            spill.write_text("================ 2 passed in 0.10s ================\n")
            rtk.write_text("#!/usr/bin/env python3\nimport sys\nassert sys.argv[1:] == ['pipe', '--filter', 'pytest']\nassert '2 passed' in sys.stdin.read()\nprint('2 tests passed')\n")
            self.assertIn("2 tests passed", run(pytest_case)["set"]["output"])
            spill.write_text("================ 1 passed, 1 failed in 0.10s ================\n")
            self.assertEqual(run(pytest_case), {})
            spill.write_text("malformed")
            self.assertEqual(run(request), {})
            spill.write_text(raw)
            rtk.write_text("#!/bin/sh\nexit 1\n")
            self.assertEqual(run(request), {})
            rtk.unlink()
            env["PATH"] = str(root)
            self.assertEqual(run(request), {})


if __name__ == "__main__":
    unittest.main()

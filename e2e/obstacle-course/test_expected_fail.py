#!/usr/bin/env python3
"""Runner exit: required fails → 1; only expected_fail failing → 0."""
import json
import os
import shutil
import stat
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
RUNNER = HERE / "run.py"

FAKE_OBSCURA = r"""#!/usr/bin/env python3
import sys
url = sys.argv[2]
if "required-fail.html" in url:
    print('""')
elif "parked.html" in url:
    print('""')
else:
    print('"ok"')
"""


def write_course(tmp, stages):
    course = Path(tmp)
    shutil.copy(RUNNER, course / "run.py")
    (course / "fixtures").mkdir()
    for st in stages:
        (course / st["file"]).write_text("<html></html>\n")
    manifest = {
        "wait_secs": 0,
        "timeout_secs": 2,
        "warmup": 0,
        "runs": 1,
        "stages": stages,
    }
    (course / "manifest.json").write_text(json.dumps(manifest))
    bin_path = course / "fake-obscura"
    bin_path.write_text(FAKE_OBSCURA)
    bin_path.chmod(bin_path.stat().st_mode | stat.S_IEXEC)
    return course, bin_path


def run_course(course, bin_path):
    env = os.environ.copy()
    env["OBSCURA_BIN"] = str(bin_path)
    return subprocess.run(
        [sys.executable, str(course / "run.py"), "--json", "--runs", "1", "--warmup", "0"],
        cwd=course,
        env=env,
        capture_output=True,
        text=True,
    )


OK = {
    "name": "ok",
    "file": "fixtures/ok.html",
    "check": "JSON.stringify('ok')",
    "expect": "ok",
}
PARKED = {
    "name": "parked",
    "file": "fixtures/parked.html",
    "check": "JSON.stringify(String(window.__obstacle||''))",
    "expect": "io:50",
    "expected_fail": True,
}
REQUIRED_FAIL = {
    "name": "required-fail",
    "file": "fixtures/required-fail.html",
    "check": "JSON.stringify(String(window.__obstacle||''))",
    "expect": "must-pass",
}


class ExpectedFailExit(unittest.TestCase):
    def test_only_expected_fail_exits_zero(self):
        with tempfile.TemporaryDirectory() as tmp:
            course, bin_path = write_course(tmp, [OK, PARKED])
            proc = run_course(course, bin_path)
            summary = json.loads(proc.stdout)
            self.assertEqual(proc.returncode, 0, proc.stderr)
            parked = next(r for r in summary["results"] if r["name"] == "parked")
            self.assertFalse(parked["pass"])

    def test_required_fail_exits_one(self):
        with tempfile.TemporaryDirectory() as tmp:
            course, bin_path = write_course(tmp, [REQUIRED_FAIL, PARKED])
            proc = run_course(course, bin_path)
            self.assertEqual(proc.returncode, 1, proc.stdout)


if __name__ == "__main__":
    unittest.main()

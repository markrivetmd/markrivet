"""Offline tests for tools/probe-frontmatter-yaml.py: argument checks, report
and count handling, malformed output, timeouts and call records. No rivet
binary runs here; a fake runner stands in for subprocess.run.

    python3 -m unittest discover -s tools -p 'test_probe_frontmatter_yaml.py'
"""

import contextlib
import importlib.util
import io
import json
import os
import stat
import subprocess
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location(
    "probe_frontmatter_yaml", os.path.join(HERE, "probe-frontmatter-yaml.py")
)
probe = importlib.util.module_from_spec(spec)
spec.loader.exec_module(probe)


def completed(argv, code=0, stdout=b"{}", stderr=b""):
    return subprocess.CompletedProcess(argv, code, stdout, stderr)


class Fake:
    """Answers every call with a fixed response and records what it saw."""

    def __init__(self, respond):
        self.respond = respond
        self.seen = []

    def __call__(self, argv, input=None, **kwargs):
        self.seen.append((list(argv), input))
        return self.respond(argv, input)


def ok_body(argv, input):
    if argv[-1] == "init":
        # A real init creates the project directory; the probe checks it.
        os.makedirs(os.path.join(argv[2], ".rivet"), exist_ok=True)
    if argv[-1] == "--version":
        return completed(argv, stdout=b"rivet fake\n")
    return completed(argv, stdout=json.dumps({"ok": True, "revision": "r", "status": "applied", "ops": [{}]}).encode())


class Target(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.exe = os.path.join(self.dir.name, "rivet")
        with open(self.exe, "w") as f:
            f.write("#!/bin/sh\nexit 0\n")
        os.chmod(self.exe, stat.S_IRWXU)

    def tearDown(self):
        self.dir.cleanup()

    def run_main(self, argv, runner=None, timeout=5):
        runner = runner or Fake(ok_body)
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            code = probe.main(argv, runner=runner, timeout=timeout)
        return code, json.loads(out.getvalue())

    def test_missing_rivet_is_a_usage_error(self):
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as e:
            probe.main(["--json"])
        self.assertEqual(e.exception.code, 2)

    def test_missing_json_flag_is_a_usage_error(self):
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as e:
            probe.main(["--rivet", self.exe])
        self.assertEqual(e.exception.code, 2)

    def test_relative_missing_directory_and_nonexecutable_targets_are_refused(self):
        plain = os.path.join(self.dir.name, "plain")
        with open(plain, "w") as f:
            f.write("")
        os.chmod(plain, stat.S_IRUSR | stat.S_IWUSR)
        for target in ["rivet", "./rivet", os.path.join(self.dir.name, "absent"), self.dir.name, plain]:
            code, report = self.run_main(["--rivet", target, "--json"])
            self.assertEqual(code, 2, target)
            self.assertFalse(report["ok"])
            self.assertIn("--rivet", report["error"])

    def test_malformed_output_fails_with_a_report_and_counts(self):
        def garbage(argv, input):
            if argv[-1] == "--version":
                return completed(argv, stdout=b"v")
            return completed(argv, stdout=b"not json", code=0)

        code, report = self.run_main(["--rivet", self.exe, "--json"], runner=Fake(garbage))
        self.assertEqual(code, 1)
        self.assertFalse(report["ok"])
        self.assertTrue(report["errors"], report)
        self.assertIn("no assertions executed", report["errors"])

    def test_timeouts_fail_bounded(self):
        def slow(argv, input=None, **kwargs):
            raise subprocess.TimeoutExpired(argv, kwargs.get("timeout"))

        code, report = self.run_main(["--rivet", self.exe, "--json"], runner=slow)
        self.assertEqual(code, 1)
        self.assertFalse(report["ok"])
        self.assertTrue(any("timed out" in e for e in report["errors"]), report["errors"])

    def test_assertion_failures_are_counted_and_exit_nonzero(self):
        # Every call "succeeds" but no bytes ever change, so the probe's own
        # byte assertions must fail; a green status is never enough.
        code, report = self.run_main(["--rivet", self.exe, "--json"], runner=Fake(ok_body))
        self.assertEqual(code, 1)
        self.assertFalse(report["ok"])
        self.assertGreater(report["assertions_executed"], 0)
        self.assertEqual(report["assertions_failed"], len(report["failures"]))
        self.assertGreater(report["assertions_failed"], 0)
        names = [a["name"] for c in report["cases"] for a in c["assertions"]]
        self.assertEqual(len(names), report["assertions_executed"])

    def test_responses_missing_fields_still_print_a_failing_report(self):
        def sparse(argv, input):
            if argv[-1] == "init":
                os.makedirs(os.path.join(argv[2], ".rivet"), exist_ok=True)
            return completed(argv, stdout=b'{"ok": true}')

        code, report = self.run_main(["--rivet", self.exe, "--json"], runner=Fake(sparse))
        self.assertEqual(code, 1)
        self.assertFalse(report["ok"])
        self.assertGreater(report["assertions_failed"], 0)
        self.assertTrue(
            any("KeyError" in a.get("detail", "") for c in report["cases"] for a in c["assertions"] if not a["ok"] and isinstance(a.get("detail"), str)),
            report["failures"][:5],
        )

    def run_with_passing_cases(self, version):
        """Every case stubbed to one passing assertion, so only --version decides."""
        def respond(argv, input=None, **kwargs):
            if argv[-1] == "--version":
                return version(argv, kwargs)
            return ok_body(argv, input)

        real = probe.run_all
        probe.run_all = lambda p: p.check("stub", True)
        try:
            return self.run_main(["--rivet", self.exe, "--json"], runner=Fake(respond))
        finally:
            probe.run_all = real

    def test_a_good_version_with_passing_cases_is_ok(self):
        code, report = self.run_with_passing_cases(lambda argv, kw: completed(argv, stdout=b"rivet 1\n"))
        self.assertEqual((code, report["ok"], report["errors"]), (0, True, []))

    def test_a_failed_or_silent_version_fails_the_run(self):
        for name, version in [
            ("nonzero", lambda argv, kw: completed(argv, code=1, stdout=b"rivet 1\n")),
            ("silent", lambda argv, kw: completed(argv, stdout=b"")),
        ]:
            code, report = self.run_with_passing_cases(version)
            self.assertEqual(code, 1, name)
            self.assertFalse(report["ok"], name)
            self.assertTrue(any("--version" in e for e in report["errors"]), (name, report["errors"]))

    def test_a_version_timeout_fails_the_run(self):
        def slow(argv, kw):
            raise subprocess.TimeoutExpired(argv, kw.get("timeout"))

        code, report = self.run_with_passing_cases(slow)
        self.assertEqual(code, 1)
        self.assertIsNone(report["version"])
        self.assertTrue(any("--version timed out" in e for e in report["errors"]), report["errors"])

    def test_a_binary_that_cannot_start_still_prints_a_failing_report(self):
        def unstartable(argv, kw):
            raise OSError(8, "Exec format error")

        code, report = self.run_with_passing_cases(unstartable)
        self.assertEqual(code, 1)
        self.assertFalse(report["ok"])
        self.assertTrue(any("--version could not run" in e for e in report["errors"]), report["errors"])
        self.assertIsNotNone(report["binary_before"])

    def test_a_binary_that_disappears_still_prints_a_failing_report(self):
        def vanish(argv, kw):
            os.remove(self.exe)
            return completed(argv, stdout=b"rivet 1\n")

        code, report = self.run_with_passing_cases(vanish)
        self.assertEqual(code, 1)
        self.assertFalse(report["ok"])
        self.assertIsNone(report["binary_after"])
        self.assertTrue(any("could not read the binary after" in e for e in report["errors"]), report["errors"])

    def test_a_binary_changed_during_the_run_is_an_error(self):
        def tamper(argv, input):
            if argv[-1] == "--version":
                with open(self.exe, "a") as f:
                    f.write("# changed\n")
            return ok_body(argv, input)

        code, report = self.run_main(["--rivet", self.exe, "--json"], runner=Fake(tamper))
        self.assertEqual(code, 1)
        self.assertIn("the probed binary changed during the run", report["errors"])
        self.assertNotEqual(report["binary_before"]["sha256"], report["binary_after"]["sha256"])

    def test_report_records_identity_and_invocation(self):
        code, report = self.run_main(["--rivet", self.exe, "--json"], runner=Fake(ok_body))
        self.assertEqual(report["binary_before"]["path"], self.exe)
        self.assertEqual(report["binary_before"]["resolved"], os.path.realpath(self.exe))
        self.assertEqual(len(report["probe"]["sha256"]), 64)
        self.assertEqual(report["version"], "rivet fake")
        self.assertEqual(report["invocation"][-3:], ["--rivet", self.exe, "--json"])


class Checks(unittest.TestCase):
    """Each check on its own, with every other condition met, so one check
    cannot pass on the strength of another one failing."""

    def probe(self, data, stays):
        p = probe.Probe("/bin/true", "/nonexistent", runner=Fake(ok_body))
        p.case = {"name": "c", "calls": [], "assertions": []}
        p.bytes = lambda: data
        p.stays = lambda: stays
        return p

    def test_applied_passes_only_on_the_exact_new_bytes(self):
        p = self.probe(b"after", {"s": 1})
        probe.applied(p, "w", 0, {"status": "applied"}, b"before", "after", {"s": 1})
        self.assertEqual(p.failures, [])
        self.assertEqual(p.assertions, 5)

    def test_applied_fails_on_unchanged_or_wrong_bytes_alone(self):
        for data, failed in [(b"before", {"c: w: bytes", "c: w: changed"}), (b"other", {"c: w: bytes"})]:
            p = self.probe(data, {"s": 1})
            probe.applied(p, "w", 0, {"status": "applied"}, b"before", "after", {"s": 1})
            self.assertEqual(set(p.failures), failed, data)

    def test_applied_fails_on_moved_stays_alone(self):
        p = self.probe(b"after", {"s": 2})
        probe.applied(p, "w", 0, {"status": "applied"}, b"before", "after", {"s": 1})
        self.assertEqual(p.failures, ["c: w: stays address the same units"])

    def test_non_json_output_is_rejected_with_its_text_kept(self):
        for stdout in [b"not json", b"", b"Error: boom\n"]:
            fake = Fake(lambda argv, input, out=stdout: completed(argv, stdout=out))
            p = probe.Probe("/bin/true", "/nonexistent", runner=fake)
            p.case = {"name": "c", "calls": [], "assertions": []}
            with self.assertRaises(probe.Abort) as e:
                p.call("get", {"path": "x.md"})
            self.assertIn("printed no JSON", str(e.exception))
            self.assertEqual(p.case["calls"][0]["stdout"], stdout.decode())


class Records(unittest.TestCase):
    def test_a_reused_then_mutated_request_keeps_each_calls_exact_stdin(self):
        fake = Fake(lambda argv, input: completed(argv, stdout=b'{"ok": true}'))
        p = probe.Probe("/bin/true", "/nonexistent", runner=fake)
        p.case = {"name": "c", "calls": [], "assertions": []}
        request = {"path": "x.md", "ops": [{"op": "set_key_yaml", "guard": "old"}]}
        p.call("write", request)
        request["ops"][0]["guard"] = "new"
        p.call("write", request)
        first, second = p.case["calls"]
        self.assertEqual(json.loads(first["stdin"])["ops"][0]["guard"], "old")
        self.assertEqual(first["request"]["ops"][0]["guard"], "old")
        self.assertEqual(second["request"]["ops"][0]["guard"], "new")
        # What the process received is exactly what was logged.
        self.assertEqual(fake.seen[0][1].decode(), first["stdin"])
        self.assertEqual(fake.seen[1][1].decode(), second["stdin"])

    def test_child_environment_drops_debug_hooks(self):
        os.environ["RIVET_TEST_FAULT"] = "rename"
        try:
            self.assertNotIn("RIVET_TEST_FAULT", probe.child_env())
        finally:
            del os.environ["RIVET_TEST_FAULT"]

    def test_a_failed_read_is_an_abort_not_a_refusal(self):
        fake = Fake(lambda argv, input: completed(argv, code=2, stdout=b'{"ok": false}'))
        p = probe.Probe("/bin/true", "/nonexistent", runner=fake)
        with self.assertRaises(probe.Abort):
            p.read("get", {"path": "x.md"})


if __name__ == "__main__":
    unittest.main()

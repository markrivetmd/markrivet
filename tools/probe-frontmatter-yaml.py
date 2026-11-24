#!/usr/bin/env python3
"""Probe a rivet binary's native add_key_yaml and set_key_yaml operations.

Independent of the engine's own tests: it drives only the supplied
executable, through its JSON CLI, in a private temporary project, and checks
exact file bytes and decoded reads. Python standard library only.

    python3 tools/probe-frontmatter-yaml.py --rivet /abs/path/to/markrivet --json

Exits 0 only when every assertion ran and passed. Any failed assertion, read
failure, timeout, identity change of the binary, or a run that executed no
assertions exits 1, and the JSON report still prints where possible. Bad
arguments exit 2.
"""

import argparse
import hashlib
import json
import os
import re
import subprocess
import sys
import tempfile

TIMEOUT = 30
STAY = re.compile(r"<!-- stay:([A-Za-z0-9_-]+)")

FIXTURE = (
    "---\n"
    "# lead comment\n"
    "scope: 'old scope' # old tail\n"
    "lenses: [a, b]   # list tail\n"
    "generated: {by: 'process:synthetic', at: old}\n"
    "sources:\n"
    "  - one\n"
    "  - two\n"
    "---\n"
    "\n"
    "# Widgets\n"
    "\n"
    "- [ ] Ship synthetic widget.\n"
    "\n"
    "## Log\n"
    "\n"
    "Kept.\n"
)
DOC = "tracking/SYNTHETIC.md"


class Abort(Exception):
    """A call that cannot be judged: a timeout, unreadable output or failed read."""


def sha256_file(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 16), b""):
            h.update(chunk)
    return h.hexdigest()


def sha256_bytes(data):
    return hashlib.sha256(data).hexdigest()


def child_env():
    """The ambient environment without debug hooks a test run may have set."""
    return {k: v for k, v in os.environ.items() if not k.startswith("RIVET_TEST_")}


def check_target(path):
    """The executable to probe: absolute, a regular file, executable."""
    if not path or not os.path.isabs(path):
        return "--rivet must be an absolute path"
    if not os.path.isfile(path):
        return f"--rivet {path} is not a regular file"
    if not os.access(path, os.X_OK):
        return f"--rivet {path} is not executable"
    return None


class Probe:
    def __init__(self, rivet, root, runner=subprocess.run, timeout=TIMEOUT):
        self.rivet = rivet
        self.root = root
        self.runner = runner
        self.timeout = timeout
        self.cases = []
        self.case = None
        self.assertions = 0
        self.failures = []

    # -- calls ------------------------------------------------------------

    def call(self, command, request):
        """Run one CLI command. The record keeps the exact stdin, serialized
        once before the process starts, and a parsed copy of it, so a caller
        reusing or mutating `request` later cannot change what was logged."""
        stdin = json.dumps(request)
        record = {"command": command, "stdin": stdin, "request": json.loads(stdin)}
        if self.case is not None:
            self.case["calls"].append(record)
        argv = [self.rivet, "--root", self.root, command]
        try:
            done = self.runner(
                argv,
                input=stdin.encode(),
                capture_output=True,
                timeout=self.timeout,
                env=child_env(),
            )
        except subprocess.TimeoutExpired:
            record["timeout"] = True
            raise Abort(f"{command} timed out after {self.timeout}s")
        record["exit"] = done.returncode
        record["stderr"] = done.stderr.decode(errors="replace")
        try:
            body = json.loads(done.stdout)
        except ValueError:
            record["stdout"] = done.stdout.decode(errors="replace")
            raise Abort(f"{command} printed no JSON (exit {done.returncode})")
        record["response"] = body
        return done.returncode, body

    def read(self, command, request):
        """A read that must succeed. A failed read is never a refusal."""
        code, body = self.call(command, request)
        if code != 0 or not body.get("ok"):
            raise Abort(f"{command} failed: exit {code} {body}")
        return body

    # -- assertions ---------------------------------------------------------

    def check(self, name, passed, detail=None):
        self.assertions += 1
        entry = {"name": name, "ok": bool(passed)}
        if not passed:
            entry["detail"] = detail
            label = f"{self.case['name']}: {name}" if self.case else name
            self.failures.append(label)
        if self.case is not None:
            self.case["assertions"].append(entry)
        return passed

    def equal(self, name, got, want):
        return self.check(name, got == want, {"got": got, "want": want})

    # -- project helpers ----------------------------------------------------

    def path(self):
        return os.path.join(self.root, DOC)

    def bytes(self):
        with open(self.path(), "rb") as f:
            return f.read()

    def text(self):
        return self.bytes().decode()

    def put(self, data):
        with open(self.path(), "wb") as f:
            f.write(data)

    def doc_guard(self):
        return self.read("map", {"path": DOC})["revision"]

    def key(self, key):
        return self.read(
            "get", {"path": DOC, "select": {"kind": "frontmatter_key", "key": key}}
        )

    def write(self, op, **extra):
        request = {"path": DOC, "ops": [op]}
        request.update(extra)
        return self.call("write", request)

    def stays(self):
        out = {}
        for sid in STAY.findall(self.text()):
            got = self.read("get", {"path": DOC, "select": {"kind": "id", "id": sid}})
            out[sid] = {"revision": got["revision"], "text": got.get("text")}
        return out

    # -- case runner --------------------------------------------------------

    def run_case(self, name, fn, *args):
        self.case = {"name": name, "calls": [], "assertions": []}
        self.cases.append(self.case)
        try:
            fn(self, *args)
        except Exception as e:  # noqa: BLE001 - any case fault is a failed case, reported
            self.case["error"] = f"{type(e).__name__}: {e}"
            self.check("case completed", False, self.case["error"])
        finally:
            self.case = None


# ---------------------------------------------------------------------------
# cases. `base` is the stamped fixture in one newline convention.


def op_revision(body):
    """The first op's returned revision, or None when the response has none."""
    ops = body.get("ops") if isinstance(body, dict) else None
    if isinstance(ops, list) and ops and isinstance(ops[0], dict):
        return ops[0].get("revision")
    return None


def applied(p, name, code, body, before, want, stays):
    """An applied write: exact bytes, unchanged stays, no stray status."""
    p.equal(f"{name}: exit", code, 0)
    p.equal(f"{name}: status", body.get("status"), "applied")
    after = p.bytes()
    p.equal(f"{name}: bytes", after.decode(), want)
    p.check(f"{name}: changed", after != before, sha256_bytes(after))
    p.equal(f"{name}: stays address the same units", p.stays(), stays)


def refused(p, name, code, body, before, error):
    p.equal(f"{name}: exit", code, 2)
    p.equal(f"{name}: error code", (body.get("error") or {}).get("code"), error)
    after = p.bytes()
    p.check(
        f"{name}: bytes unchanged",
        after == before,
        {"before": sha256_bytes(before), "after": sha256_bytes(after)},
    )


def unchanged(p, name, code, body, before):
    p.equal(f"{name}: exit", code, 0)
    p.equal(f"{name}: status", body.get("status"), "unchanged")
    p.check(f"{name}: bytes identical", p.bytes() == before)


def case_add(p, base, nl, key, token, value):
    p.put(base)
    stays = p.stays()
    fence = f"{nl}---{nl}{nl}# Widgets"
    want = base.decode().replace(fence, f"{nl}{key}: {token}{fence}", 1)
    code, body = p.write(
        {"op": "add_key_yaml", "key": key, "yaml": token, "guard": p.doc_guard()}
    )
    applied(p, "add", code, body, base, want, stays)
    got = p.key(key)
    p.equal("decoded value", got.get("value"), value)
    p.equal("returned revision is the key's", op_revision(body), got.get("revision"))


def case_set(p, base, nl, key, old, token, value, replace_line, tail):
    p.put(base)
    stays = p.stays()
    line = f"{key}: {old}{tail}"
    new = f"{key}: {token}" + ("" if replace_line else tail)
    want = base.decode().replace(line, new, 1)
    op = {"op": "set_key_yaml", "key": key, "yaml": token, "guard": p.key(key)["revision"]}
    if replace_line:
        op["replace_line"] = True
    code, body = p.write(op)
    applied(p, "set", code, body, base, want, stays)
    got = p.key(key)
    p.equal("decoded value", got.get("value"), value)
    p.equal("returned revision is the key's", op_revision(body), got.get("revision"))


def case_same_token(p, base, nl):
    """Removing a tail with the same token changes once; then it is a no-op."""
    p.put(base)
    stays = p.stays()
    token = "'old scope'"
    code, body = p.write(
        {"op": "set_key_yaml", "key": "scope", "yaml": token, "guard": p.key("scope")["revision"]}
    )
    unchanged(p, "same token, tail kept", code, body, base)
    code, body = p.write(
        {
            "op": "set_key_yaml",
            "key": "scope",
            "yaml": token,
            "guard": p.key("scope")["revision"],
            "replace_line": True,
        }
    )
    want = base.decode().replace(" # old tail", "", 1)
    applied(p, "same token, tail removed", code, body, base, want, stays)
    removed = p.bytes()
    for replace_line in (True, False):
        code, body = p.write(
            {
                "op": "set_key_yaml",
                "key": "scope",
                "yaml": token,
                "guard": p.key("scope")["revision"],
                "replace_line": replace_line,
            }
        )
        unchanged(p, f"fresh replay replace_line={replace_line}", code, body, removed)


def case_stale_guards(p, base, nl):
    """A comment-only change invalidates the key's and document's guards,
    even for a write that would otherwise change nothing."""
    p.put(base)
    key_guard = p.key("scope")["revision"]
    doc_guard = p.doc_guard()
    code, body = p.write(
        {
            "op": "set_key_yaml",
            "key": "scope",
            "yaml": "'old scope'",
            "guard": key_guard,
            "replace_line": True,
        }
    )
    p.equal("comment-only change: exit", code, 0)
    changed = p.bytes()
    code, body = p.write(
        {"op": "set_key_yaml", "key": "scope", "yaml": "'old scope'", "guard": key_guard}
    )
    refused(p, "stale key guard, would-be no-op", code, body, changed, "guard_conflict")
    code, body = p.write({"op": "add_key_yaml", "key": "added", "yaml": "x", "guard": doc_guard})
    refused(p, "stale document guard", code, body, changed, "guard_conflict")


def case_list_replay(p, base, nl):
    p.put(base)
    original = p.doc_guard()
    op = {"op": "add_key_yaml", "key": "seeded", "yaml": "[a, 'b c']", "guard": original}
    code, body = p.write(op)
    p.equal("seed: exit", code, 0)
    seeded = p.bytes()
    code, body = p.write(op)
    refused(p, "add replay, original guard", code, body, seeded, "guard_conflict")
    op["guard"] = p.doc_guard()
    code, body = p.write(op)
    refused(p, "add replay, fresh guard", code, body, seeded, "already_exists")
    code, body = p.write(
        {"op": "set_key_yaml", "key": "seeded", "yaml": "[a, 'b c']", "guard": p.key("seeded")["revision"]}
    )
    unchanged(p, "list set replay", code, body, seeded)
    p.equal("seeded value", p.key("seeded").get("value"), ["a", "b c"])


def case_refusals(p, base, nl):
    p.put(base)
    for token in ["", " a", "a # c", "[a] x", "{a: 1}", "[[a]]", "&x a", "*x", "!!str a", "a\nb", "~", "|"]:
        code, body = p.write(
            {"op": "set_key_yaml", "key": "scope", "yaml": token, "guard": p.key("scope")["revision"]}
        )
        refused(p, f"set token {token!r}", code, body, base, "unsupported_syntax")
        code, body = p.write(
            {"op": "add_key_yaml", "key": "added", "yaml": token, "guard": p.doc_guard()}
        )
        refused(p, f"add token {token!r}", code, body, base, "unsupported_syntax")
    for key in ["generated", "sources"]:
        code, body = p.write(
            {"op": "set_key_yaml", "key": key, "yaml": "x", "guard": p.key(key)["revision"]}
        )
        refused(p, f"set target {key}", code, body, base, "unsupported_syntax")
    code, body = p.write(
        {"op": "set_key_yaml", "key": "absent", "yaml": "x", "guard": p.key("scope")["revision"]}
    )
    refused(p, "set missing key", code, body, base, "missing_selector")
    code, body = p.write({"op": "add_key_yaml", "key": "scope", "yaml": "x", "guard": p.doc_guard()})
    refused(p, "add existing key", code, body, base, "already_exists")
    for op in [
        {"op": "add_key_yaml", "key": "added", "yaml": 1, "guard": "g"},
        {"op": "add_key_yaml", "key": "added", "yaml": "x", "guard": "g", "replace_line": True},
        {"op": "set_key_yaml", "select": {"kind": "frontmatter_key", "key": "scope"}, "yaml": "x", "guard": "g"},
    ]:
        code, body = p.write(op)
        p.equal(f"invalid request {sorted(op)}: exit", code, 1)
        p.check(f"invalid request {sorted(op)}: bytes unchanged", p.bytes() == base)


def case_preview(p, base, nl):
    p.put(base)
    code, body = p.write(
        {"op": "set_key_yaml", "key": "scope", "yaml": "[p]", "guard": p.key("scope")["revision"]},
        preview=True,
    )
    p.equal("preview: exit", code, 0)
    p.equal("preview: status", body.get("status"), "preview")
    p.check("preview: bytes unchanged", p.bytes() == base)


SCALAR_AND_LIST = [
    ("'it''s'", "it's"),
    ('"double"', "double"),
    ("plain text", "plain text"),
    ("[]", []),
    ("[x, 'y z']", ["x", "y z"]),
    ("[a, 1, true, null, 'q', \"d\",]", ["a", 1, True, None, "q", "d"]),
    ("['#]', \"c#d\", 'é😀']", ["#]", "c#d", "é😀"]),
]


def run_all(p):
    p.read("init", {})
    if not os.path.isdir(os.path.join(p.root, ".rivet")):
        raise Abort("init created no .rivet directory")
    os.makedirs(os.path.join(p.root, "tracking"), exist_ok=True)
    with open(os.path.join(p.root, ".rivet", "project.json"), "w") as f:
        json.dump({"collections": {}, "git": {"commit": "off"}}, f)
    for nl in ("\n", "\r\n"):
        label = "lf" if nl == "\n" else "crlf"
        p.case = {"name": f"{label}: setup", "calls": [], "assertions": []}
        p.cases.append(p.case)
        try:
            p.put(FIXTURE.replace("\n", nl).encode())
            guard = p.doc_guard()
            code, body = p.call(
                "stamp", {"path": DOC, "guard": guard, "select": {"kind": "document"}}
            )
            p.equal("stamp: exit", code, 0)
            base = p.bytes()
            p.check("stamped stays", len(STAY.findall(base.decode())) >= 2)
        except Exception as e:  # noqa: BLE001 - reported as a failed setup
            p.check("setup completed", False, f"{type(e).__name__}: {e}")
            p.case = None
            continue
        p.case = None
        for i, (token, value) in enumerate(SCALAR_AND_LIST):
            p.run_case(f"{label}: add {token}", case_add, base, nl, f"added{i}", token, value)
            p.run_case(
                f"{label}: set scalar to {token}",
                case_set, base, nl, "scope", "'old scope'", token, value, False, " # old tail",
            )
            p.run_case(
                f"{label}: set list to {token}",
                case_set, base, nl, "lenses", "[a, b]", token, value, False, "   # list tail",
            )
        for replace_line in (False, True):
            p.run_case(
                f"{label}: suffix replace_line={replace_line}",
                case_set, base, nl, "scope", "'old scope'", "'new'", "new", replace_line, " # old tail",
            )
            p.run_case(
                f"{label}: list suffix replace_line={replace_line}",
                case_set, base, nl, "lenses", "[a, b]", "[c]", ["c"], replace_line, "   # list tail",
            )
        p.run_case(f"{label}: same token removal then no-op", case_same_token, base, nl)
        p.run_case(f"{label}: stale guards", case_stale_guards, base, nl)
        p.run_case(f"{label}: list replay", case_list_replay, base, nl)
        p.run_case(f"{label}: refusals", case_refusals, base, nl)
        p.run_case(f"{label}: preview", case_preview, base, nl)


def identity(path):
    real = os.path.realpath(path)
    return {"path": path, "resolved": real, "sha256": sha256_file(path), "resolved_sha256": sha256_file(real)}


def main(argv=None, runner=subprocess.run, timeout=TIMEOUT):
    parser = argparse.ArgumentParser(description=(__doc__ or "").splitlines()[0])
    parser.add_argument("--rivet", required=True, help="absolute path of the rivet executable to probe")
    parser.add_argument("--json", action="store_true", required=True, help="print the report as JSON")
    args = parser.parse_args(argv)
    problem = check_target(args.rivet)
    if problem:
        print(json.dumps({"ok": False, "error": problem}))
        return 2
    probe_path = os.path.abspath(__file__)
    errors = []

    def read_identity(when):
        """The binary's identity, or None with an error: a failure to read it
        is a failed run that still prints its report."""
        try:
            return identity(args.rivet)
        except OSError as e:
            errors.append(f"could not read the binary {when} the run: {type(e).__name__}: {e}")
            return None

    report = {
        "probe": {"path": probe_path, "sha256": sha256_file(probe_path)},
        "invocation": [sys.executable, probe_path] + list(argv if argv is not None else sys.argv[1:]),
        "binary_before": read_identity("before"),
        "version": None,
    }
    # The version is part of the binary's identity, so a binary that cannot
    # report one, or cannot be started at all, fails the run rather than
    # leaving a gap in the report.
    try:
        version = runner([args.rivet, "--version"], capture_output=True, timeout=timeout, env=child_env())
        report["version"] = version.stdout.decode(errors="replace").strip()
        if version.returncode != 0:
            errors.append(f"--version exited {version.returncode}")
        elif not report["version"]:
            errors.append("--version printed nothing")
    except subprocess.TimeoutExpired:
        errors.append(f"--version timed out after {timeout}s")
    except OSError as e:
        errors.append(f"--version could not run: {type(e).__name__}: {e}")
    p = None
    try:
        with tempfile.TemporaryDirectory(prefix="rivet-yaml-probe-") as root:
            p = Probe(args.rivet, root, runner=runner, timeout=timeout)
            try:
                run_all(p)
            except Exception as e:  # noqa: BLE001 - reported, never a silent pass
                errors.append(f"{type(e).__name__}: {e}")
    except OSError as e:
        errors.append(f"the private project directory failed: {type(e).__name__}: {e}")
    report["binary_after"] = read_identity("after")
    before, after = report["binary_before"], report["binary_after"]
    if before is not None and after is not None and after != before:
        errors.append("the probed binary changed during the run")
    assertions = p.assertions if p else 0
    failures = p.failures if p else []
    if assertions == 0:
        errors.append("no assertions executed")
    report.update(
        {
            "cases": p.cases if p else [],
            "assertions_executed": assertions,
            "assertions_failed": len(failures),
            "failures": failures,
            "errors": errors,
        }
    )
    report["ok"] = not failures and not errors and assertions > 0
    print(json.dumps(report, ensure_ascii=False, indent=1))
    return 0 if report["ok"] else 1


if __name__ == "__main__":
    sys.exit(main())

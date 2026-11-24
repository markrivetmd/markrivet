#!/usr/bin/env python3
"""Deterministic, tool-only trials: Rivet against two baselines, no LLM.

Each task is a scripted agent that uses one tool condition to reach a stated
end state. The same seeded corpus is the input to every condition. An oracle
written independently of Rivet decides correctness from file contents, and
counts bytes changed outside the intended edit as damage.

Conditions and what they guarantee:

  naive  read a whole file, write a whole file. No guard, no lock, a plain
         in-place write.
  patch  grep across files, read a line range, replace an exact unique old
         text with new text. The old text is the only guard. Writes are
         atomic (temp file and rename) but unlocked: two patches in flight
         can each rename a result built from the same stale read.
  rivet  the `markrivet` CLI. Revisions guard every write; one project lock
         serialises writers; replacement is atomic and durable.

Payload sizes are bytes; token counts use tiktoken cl100k_base when it is
importable and are otherwise omitted. Neither is model usage: they size what
enters and leaves an agent's context, and nothing here calls a model.

    RIVET=target/debug/markrivet python3 benchmark/trials/trials.py --output /tmp/trials.json

A debug build of rivet honours RIVET_TEST_ID_SEED, which makes stamped ids,
and so every payload size, reproducible.
"""
from __future__ import annotations

import argparse
import difflib
import errno
import hashlib
import json
import os
import random
import re
import shutil
import subprocess
import sys
import tempfile
import threading
import time
from dataclasses import asdict, dataclass, field
from pathlib import Path

try:
    import tiktoken

    ENC = tiktoken.get_encoding("cl100k_base")
    TOKENIZER = "tiktoken cl100k_base"
except ImportError:  # pragma: no cover - depends on the environment
    ENC = None
    TOKENIZER = None

ROOT = Path(__file__).resolve().parents[2]
EXAMPLE = ROOT / "examples" / "tracker"
SEED = 20260911
TOPICS = [
    "the ingestion pipeline", "the auth service", "the billing reconciler",
    "the search index", "the notification fanout", "the rate limiter",
    "the caching layer", "the export job", "the webhook dispatcher",
]
ACTORS = ["agent", "human:sam", "process:nightly", None]


def tokens(text: str) -> int | None:
    return len(ENC.encode(text)) if ENC else None


# --------------------------------------------------------------------------
# Corpus


def tracker(n: int, rng: random.Random, actions: int = 6, logs: int = 4, filler: int = 0) -> str:
    lines = [
        "---",
        "status: active",
        f"entity: tracker-{n}",
        "owner: owner",
        "---",
        "",
        f"# Tracker {n}",
        "",
        f"> State for {rng.choice(TOPICS)}. The design lives elsewhere; this",
        "> file keeps the queue and the log.",
        "",
        "## Action",
        "",
    ]
    for k in range(actions):
        actor = ACTORS[(n + k) % len(ACTORS)]
        tag = f"`{actor}` " if actor else ""
        done = "x" if (n + k) % 5 == 4 else " "
        day = 1 + (n * 7 + k * 3) % 28
        lines += [
            f"- [{done}] {tag}**Action {k} of tracker {n}. (2026-08-{day:02d})** Review {rng.choice(TOPICS)}",
            f"      and record what changed since the last pass.",
            f"      **Gate**: gate {k} of tracker {n} holds.",
            "",
        ]
    for i in range(filler):
        lines += [f"## Section {i}", "", f"Notes on {rng.choice(TOPICS)}, paragraph {i}.", ""]
    lines += [
        "## Candidates",
        "",
        f"- [ ] `agent` **Candidate for tracker {n}.** Not queued yet.",
        "",
        "## Log",
        "",
        "Newest first.",
        "",
    ]
    for j in range(logs):
        lines += [f"### 2026-08-{28 - j:02d}: entry {j}", "", f"Worked on {rng.choice(TOPICS)}.", ""]
    return "\n".join(lines)


def corpus(dest: Path, files: int, filler: int = 0) -> dict[str, str]:
    """Write a seeded project with `files` trackers; return path -> text."""
    shutil.copytree(EXAMPLE / ".rivet", dest / ".rivet")
    (dest / "tracking").mkdir(parents=True)
    written = {}
    for n in range(files):
        seed = int(hashlib.sha256(f"{SEED}:{n}:{filler}".encode()).hexdigest()[:16], 16)
        rel = f"tracking/TRACKING_{n:03d}.md"
        text = tracker(n, random.Random(seed), filler=filler)
        (dest / rel).write_text(text)
        written[rel] = text
    return written


# --------------------------------------------------------------------------
# Oracle: a line-based reading of the tracker shape, independent of Rivet.

STAY = re.compile(r"\s*<!-- stay:[^>]*-->")
MARKER_DIGEST = re.compile(r"(<!-- stay:[A-Za-z0-9_-]+ (?:hash|subhash)=sha256:)([0-9a-f]+)( -->)")
MARKER_LINE = re.compile(r"^<!-- stay:[A-Za-z0-9_-]+ (?:hash|subhash)=sha256:[0-9a-f]+ -->\n", re.M)
INLINE_MARKER = re.compile(r" <!-- stay:[A-Za-z0-9_-]+ (?:hash|subhash)=sha256:[0-9a-f]+ -->")


def normalise(text: str) -> str:
    """Mask stay-marker digests, keeping their length: refreshing a digest is
    the one change a correct Rivet write may make unasked."""
    return MARKER_DIGEST.sub(lambda m: m.group(1) + "*" * len(m.group(2)) + m.group(3), text)


def unstamp(text: str) -> str:
    """The text with Rivet-written stay markers removed, to check stamping
    preserved every other byte."""
    return INLINE_MARKER.sub("", MARKER_LINE.sub("", text))


def actions(text: str) -> list[dict]:
    """Checkbox items of the `## Action` section, in order."""
    out, section, item = [], None, None
    for line in text.split("\n"):
        if line.startswith("## "):
            section = line[3:].strip()
            item = None
            continue
        if section != "Action":
            continue
        bare = STAY.sub("", line)
        m = re.match(r"- \[( |x)\] (?:`([^`]+)` )?\*\*(.+?)\*\*", bare)
        if m:
            title, date = re.match(r"(.*?)(?: \((\d{4}-\d{2}-\d{2})\))?$", m.group(3)).groups()
            item = {"done": m.group(1) == "x", "actor": m.group(2), "title": title, "date": date, "gate": None}
            out.append(item)
        elif item and "**Gate**:" in bare:
            item["gate"] = bare.split("**Gate**:", 1)[1].strip()
    return out


TOOL_TIMEOUT_S = 60  # one rivet invocation; it holds the project lock while it runs

# Linux pressure-stall counters. `total=` is cumulative microseconds in which
# at least one task (`some`) or every non-idle task (`full`) waited on the
# resource, host-wide, so the difference across a trial is the host's own
# stall time during it, whatever caused it.
PSI_FILES = {"io": "/proc/pressure/io", "memory": "/proc/pressure/memory"}


def psi_snapshot(files: dict[str, str] | None = None) -> dict:
    """The `some`/`full` totals per resource, in microseconds. A resource
    whose file is missing or unreadable (not Linux, a container without PSI,
    a kernel built without it) is `None`: unmeasured, never zero."""
    snap: dict = {}
    for resource, path in (PSI_FILES if files is None else files).items():
        try:
            totals = {}
            for line in Path(path).read_text().splitlines():
                kind, *fields = line.split()
                for f in fields:
                    if f.startswith("total="):
                        totals[kind] = int(f[len("total="):])
            snap[resource] = totals if "some" in totals else None
        except (OSError, ValueError):
            snap[resource] = None
    return snap


class ProbedPopen(subprocess.Popen):
    """Popen that reaps with wait4, so the child's own rusage survives.

    `_try_wait` is CPython's POSIX reaping hook (every `wait`/`communicate`
    path goes through it); overriding it is the only way to get the rusage of
    this one child, since RUSAGE_CHILDREN mixes in every concurrent writer."""

    rusage = None

    def _try_wait(self, wait_flags):
        try:
            pid, sts, ru = os.wait4(self.pid, wait_flags)
        except ChildProcessError:
            return self.pid, 0
        if pid == self.pid:
            self.rusage = ru
        return pid, sts

    def _internal_poll(self, _deadstate=None, _wait4=os.wait4, _WNOHANG=os.WNOHANG,
                       _ECHILD=errno.ECHILD):
        """CPython's poll, reaping with wait4. `kill()` and `poll()` reap
        through here rather than `_try_wait`, so a child that exits between a
        timeout and its kill would otherwise lose its rusage. Same lock and
        pid handling as the original; locals only, since `__del__` calls it."""
        if self.returncode is None:
            if not self._waitpid_lock.acquire(False):
                return None
            try:
                if self.returncode is not None:
                    return self.returncode
                pid, sts, ru = _wait4(self.pid, _WNOHANG)
                if pid == self.pid:
                    self.rusage = ru
                    self._handle_exitstatus(sts)
            except OSError as e:
                if _deadstate is not None:
                    self.returncode = _deadstate
                elif e.errno == _ECHILD:
                    self.returncode = 0
            finally:
                self._waitpid_lock.release()
        return self.returncode


def _rusage_dict(ru) -> dict | None:
    # maxrss survives execve, so it carries the forking interpreter's own
    # high-water mark as a floor; the CPU, block-I/O and context-switch
    # counts are the child's alone.
    if ru is None:
        return None
    return {"utime_s": round(ru.ru_utime, 6), "stime_s": round(ru.ru_stime, 6),
            "maxrss_kb": ru.ru_maxrss, "inblock": ru.ru_inblock, "oublock": ru.ru_oublock,
            "nvcsw": ru.ru_nvcsw, "nivcsw": ru.ru_nivcsw}


def proc_state(pid: int) -> tuple[str | None, str | None]:
    """The process's scheduler state letter and kernel wait channel, read
    before it is killed. `D` (uninterruptible sleep) in an fsync or I/O wait
    is the host holding the process, not the process holding a lock by
    choice. Either is None when /proc cannot say."""
    state = wchan = None
    try:
        stat = Path(f"/proc/{pid}/stat").read_text()
        # The command name is parenthesised and may contain spaces.
        state = stat[stat.rindex(")") + 2:].split()[0]
    except (OSError, ValueError, IndexError):
        pass
    try:
        wchan = Path(f"/proc/{pid}/wchan").read_text().strip() or None
    except OSError:
        pass
    return state, wchan


def run_probed(argv: list[str], sent: str, env: dict | None, probe: dict,
               timeout: float | None = None) -> tuple[str, int]:
    """Run one tool process as `subprocess.run(..., capture_output=True,
    timeout=)` would, filling `probe` with what the host did to it: monotonic
    duration, the child's rusage, and on a timeout its state and wait channel
    before the kill and how long the kill took to reap. A timeout re-raises
    the same `subprocess.TimeoutExpired` after the kill, as `subprocess.run`
    does; `probe` is complete by then."""
    timeout = TOOL_TIMEOUT_S if timeout is None else timeout
    started = time.monotonic()
    probe["timed_out"] = False
    with ProbedPopen(argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                     text=True, env=env) as p:
        try:
            stdout, _ = p.communicate(sent, timeout=timeout)
        except subprocess.TimeoutExpired:
            probe["timed_out"] = True
            probe["state"], probe["wchan"] = proc_state(p.pid)
            p.kill()
            # True when the child had already exited and kill() only reaped it.
            probe["exited_before_kill"] = p.returncode is not None
            killed = time.monotonic()
            p.wait()
            probe["reap_s"] = round(time.monotonic() - killed, 6)
            probe["duration_s"] = round(time.monotonic() - started, 6)
            probe["rusage"] = _rusage_dict(p.rusage)
            raise
        except BaseException:  # as subprocess.run: never leave the child running
            p.kill()
            raise
        probe["duration_s"] = round(time.monotonic() - started, 6)
        probe["exit_code"] = p.returncode
        probe["rusage"] = _rusage_dict(p.rusage)
        return stdout, p.returncode


def damage(result: str, expected: str) -> int:
    """Bytes on lines that differ from the expected text, counted on both
    sides of a line diff after digest masking."""
    a, b = normalise(result).splitlines(True), normalise(expected).splitlines(True)
    if a == b:
        return 0
    total = 0
    for tag, i1, i2, j1, j2 in difflib.SequenceMatcher(None, a, b, autojunk=False).get_opcodes():
        if tag != "equal":
            total += sum(len(x) for x in a[i1:i2]) + sum(len(x) for x in b[j1:j2])
    return total


# --------------------------------------------------------------------------
# Conditions


@dataclass
class Call:
    tool: str
    sent: str
    received: str


@dataclass
class Log:
    calls: list[Call] = field(default_factory=list)

    def add(self, tool: str, sent: str, received: str) -> None:
        self.calls.append(Call(tool, sent, received))

    def totals(self) -> dict:
        sent = "".join(c.sent for c in self.calls)
        received = "".join(c.received for c in self.calls)
        return {
            "calls": len(self.calls),
            "sent_bytes": len(sent.encode()),
            "received_bytes": len(received.encode()),
            "sent_tokens": tokens(sent),
            "received_tokens": tokens(received),
        }


class Naive:
    name = "naive"
    guarantees = "no guard, no lock, in-place whole-file write"

    def __init__(self, root: Path, log: Log):
        self.root, self.log = root, log

    def read(self, rel: str) -> str:
        text = (self.root / rel).read_text()
        self.log.add("read", rel, text)
        return text

    def write(self, rel: str, text: str) -> None:
        (self.root / rel).write_text(text)
        self.log.add("write", rel + text, "ok")


class Patch:
    name = "patch"
    guarantees = "exact old text as guard, atomic rename, no lock"

    def __init__(self, root: Path, log: Log):
        self.root, self.log = root, log

    def grep(self, pattern: str, rels: list[str]) -> list[tuple[str, int, str]]:
        hits = []
        for rel in rels:
            for n, line in enumerate((self.root / rel).read_text().split("\n"), 1):
                if re.search(pattern, line):
                    hits.append((rel, n, line))
        self.log.add("grep", pattern + " " + " ".join(rels), "\n".join(f"{r}:{n}:{l}" for r, n, l in hits))
        return hits

    def read_range(self, rel: str, start: int, end: int) -> str:
        lines = (self.root / rel).read_text().split("\n")[start - 1 : end]
        text = "\n".join(lines)
        self.log.add("read_range", f"{rel}:{start}-{end}", text)
        return text

    def prepare(self, rel: str, old: str, new: str) -> tuple[bool, str]:
        """The tool's read-and-replace step, before its rename."""
        text = (self.root / rel).read_text()
        if text.count(old) != 1:
            return False, text
        return True, text.replace(old, new, 1)

    def commit(self, rel: str, text: str) -> None:
        # Unique per call: two writers racing on one path is the behaviour
        # this baseline is here to exhibit, but sharing a scratch filename is
        # an artefact of this harness, not of a read-then-rename tool, and it
        # damages the baseline beyond its advertised lack of locking.
        tmp = self.root / f"{rel}.{os.getpid()}.{threading.get_ident()}.tmp"
        tmp.write_text(text)
        os.replace(tmp, self.root / rel)

    def patch(self, rel: str, old: str, new: str) -> bool:
        ok, text = self.prepare(rel, old, new)
        if ok:
            self.commit(rel, text)
        self.log.add("patch", rel + old + new, "ok" if ok else "error: old text not found exactly once")
        return ok


class Rivet:
    name = "rivet"
    guarantees = "revision guards, one project lock, atomic durable rename"

    def __init__(self, root: Path, log: Log, binary: str):
        self.root, self.log, self.binary = root, log, binary
        # One record per invocation, from `run_probed`: what the host was
        # doing to the process, as opposed to what rivet answered.
        self.probes: list[dict] = []

    def __call__(self, command: str, request: dict) -> tuple[int, dict]:
        sent = json.dumps(request)
        env = dict(os.environ, RIVET_TEST_ID_SEED=str(SEED))
        # Bounded by TOOL_TIMEOUT_S inside run_probed: rivet takes a project
        # lock, so a wedged process would otherwise hold every other writer in
        # the trial behind it forever.
        probe = {"command": command}
        self.probes.append(probe)  # before the run, so a timeout still leaves it
        stdout, code = run_probed([self.binary, "--root", str(self.root), command], sent, env, probe)
        self.log.add(command, sent, stdout)
        return code, json.loads(stdout)


def stamp_all(root: Path, rels: list[str], binary: str) -> None:
    quiet = Rivet(root, Log(), binary)
    for rel in rels:
        _, m = quiet("map", {"path": rel})
        code, out = quiet("stamp", {"path": rel, "guard": m["revision"]})
        assert code == 0, out


# --------------------------------------------------------------------------
# Tasks. Each returns metrics for one condition on a fresh copy of the corpus.


@dataclass
class Result:
    task: str
    param: str
    condition: str
    guarantees: str
    correct: bool
    damage_bytes: int
    lost_updates: int
    conflicts_detected: int
    notes: str
    calls: int = 0
    sent_bytes: int = 0
    received_bytes: int = 0
    sent_tokens: int | None = None
    received_tokens: int | None = None


def item_line(text: str, title: str) -> str:
    return next(l for l in text.split("\n") if f"**{title}" in l)


def gate_line(text: str, n: int, k: int) -> str:
    return next(l for l in text.split("\n") if f"gate {k} of tracker {n} holds" in l)


def with_gate(line: str, new: str) -> str:
    """The gate line with new text, keeping a trailing stay marker in place."""
    head, rest = line.split("**Gate**:", 1)
    tail = rest[rest.index(" <!--"):] if " <!--" in rest else ""
    return f"{head}**Gate**: {new}{tail}"


def set_gate(text: str, n: int, k: int, new: str) -> str:
    old = gate_line(text, n, k)
    return text.replace(old, with_gate(old, new), 1)


def complete_one(cond, root: Path, rels: list[str]) -> tuple[str, str]:
    """Mark action 1 of tracker 0 done. Returns (result, expected)."""
    rel, title = rels[0], "Action 1 of tracker 0."
    before = (root / rel).read_text()
    line = item_line(before, title)
    expected = before.replace(line, line.replace("- [ ]", "- [x]", 1), 1)
    if cond.name == "naive":
        text = cond.read(rel)
        cond.write(rel, text.replace(line, line.replace("- [ ]", "- [x]", 1), 1))
    elif cond.name == "patch":
        (_, _, hit), = cond.grep(re.escape(title), [rel])
        cond.patch(rel, hit, hit.replace("- [ ]", "- [x]", 1))
    else:
        _, q = cond("query", {"collection": "actions", "paths": [rel],
                              "where": [{"field": "title", "op": "eq", "value": "Action 1 of tracker 0."}],
                              "fields": ["done"]})
        item = q["items"][0]
        cond("update-item", {"path": rel, "collection": "actions", "id": item["id"],
                             "guard": item["revision"], "set": {"done": True}})
    return (root / rel).read_text(), expected


def two_writers(cond, root: Path, rels: list[str], same: bool) -> tuple[str, str, int, int]:
    """Writers A and B both read, then A writes, then B writes.

    Same node: both change action 1's gate; the right outcome is A's change
    kept and B told it is stale. Disjoint: A changes action 1, B action 2;
    the right outcome is both kept.
    """
    rel = rels[0]
    before = (root / rel).read_text()
    kb = 1 if same else 2
    expected = set_gate(before, 0, 1, "A was here.")
    if not same:
        expected = set_gate(expected, 0, kb, "B was here.")
    conflicts = 0
    if cond.name == "naive":
        a, b = cond.read(rel), cond.read(rel)
        cond.write(rel, set_gate(a, 0, 1, "A was here."))
        cond.write(rel, set_gate(b, 0, kb, "B was here."))
    elif cond.name == "patch":
        ((_, _, la),) = cond.grep(re.escape(f"gate 1 of tracker 0 holds"), [rel])
        ((_, _, lb),) = cond.grep(re.escape(f"gate {kb} of tracker 0 holds"), [rel])
        cond.patch(rel, la, with_gate(la, "A was here."))
        if not cond.patch(rel, lb, with_gate(lb, "B was here.")):
            conflicts += 1
    else:
        def read(k):
            _, q = cond("query", {"collection": "actions", "paths": [rel],
                                  "where": [{"field": "title", "op": "eq", "value": f"Action {k} of tracker 0."}],
                                  "fields": []})
            return q["items"][0]
        ia, ib = read(1), read(kb)
        cond("update-item", {"path": rel, "collection": "actions", "id": ia["id"], "guard": ia["revision"],
                             "set": {"gate": "A was here."}})
        code, out = cond("update-item", {"path": rel, "collection": "actions", "id": ib["id"],
                                         "guard": ib["revision"], "set": {"gate": "B was here."}})
        if code == 2 and out["error"]["code"] == "guard_conflict":
            conflicts += 1
    result = (root / rel).read_text()
    gates = {a["title"]: a["gate"] for a in actions(result)}
    a_kept = gates["Action 1 of tracker 0."] == "A was here."
    b_kept = gates[f"Action {kb} of tracker 0."] == "B was here."
    # A lost update is a write that was neither kept nor explicitly refused.
    lost = int(not a_kept) + int(not b_kept and conflicts == 0)
    return result, expected, lost, conflicts, 1 if same else 0


def racing_patches(cond, root: Path, rels: list[str]) -> tuple[str, str, int, int]:
    """Two disjoint edits whose tool steps interleave: both tools read the
    file, then both rename. Rivet's writers are real concurrent processes."""
    rel = rels[0]
    before = (root / rel).read_text()
    expected = set_gate(set_gate(before, 0, 1, "A was here."), 0, 2, "B was here.")
    if cond.name == "patch":
        la, lb = gate_line(before, 0, 1), gate_line(before, 0, 2)
        ok_a, text_a = cond.prepare(rel, la, with_gate(la, "A was here."))
        ok_b, text_b = cond.prepare(rel, lb, with_gate(lb, "B was here."))
        cond.commit(rel, text_a)
        cond.commit(rel, text_b)
        cond.log.add("patch", rel + la, "ok")
        cond.log.add("patch", rel + lb, "ok")
    elif cond.name == "rivet":
        # A plans its write and holds the lock at the pause point; B starts,
        # having read the same preimage, and must wait; then A is released.
        _, q = cond("query", {"collection": "actions", "paths": [rel], "fields": []})
        items = q["items"]
        pause = root.with_name(root.name + "-pause")
        pause.mkdir()
        env = dict(os.environ, RIVET_TEST_ID_SEED=str(SEED))
        procs = []
        for n, (item, text) in enumerate(((items[1], "A was here."), (items[2], "B was here."))):
            req = json.dumps({"path": rel, "collection": "actions", "id": item["id"],
                              "guard": item["revision"], "set": {"gate": text}})
            extra = {"RIVET_TEST_PAUSE": f"planned:{pause}"} if n == 0 else {}
            p = subprocess.Popen([cond.binary, "--root", str(root), "update-item"],
                                 stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True,
                                 env={**env, **extra})
            p.stdin.write(req)
            p.stdin.close()
            procs.append((req, p))
            if n == 0:
                deadline = time.time() + 20
                while not (pause / "planned.reached").exists():
                    assert time.time() < deadline, "writer A never reached its pause"
                    time.sleep(0.01)
        time.sleep(0.2)  # B is now blocked on the project lock
        (pause / "planned.go").write_text("")
        for req, p in procs:
            out = p.stdout.read()
            p.wait()
            cond.log.add("update-item", req, out)
        shutil.rmtree(pause)
    else:
        raise ValueError("racing applies to atomic-write tools only")
    result = (root / rel).read_text()
    gates = {a["title"]: a["gate"] for a in actions(result)}
    lost = int(gates["Action 1 of tracker 0."] != "A was here.") + int(gates["Action 2 of tracker 0."] != "B was here.")
    return result, expected, lost, 0, 0


def warm_revisits(cond, root: Path, rels: list[str], edits: int = 5) -> tuple[str, str]:
    rel = rels[0]
    before = (root / rel).read_text()
    expected = set_gate(before, 0, 1, f"revision {edits - 1} holds.")
    if cond.name == "naive":
        text = cond.read(rel)
        for i in range(edits):
            text = set_gate(text, 0, 1, f"revision {i} holds.") if i == 0 else text.replace(
                f"revision {i - 1} holds.", f"revision {i} holds.", 1)
            cond.write(rel, text)
    elif cond.name == "patch":
        ((_, _, line),) = cond.grep(re.escape("gate 1 of tracker 0 holds"), [rel])
        for i in range(edits):
            new = with_gate(line, f"revision {i} holds.")
            cond.patch(rel, line, new)
            line = new
    else:
        _, q = cond("query", {"collection": "actions", "paths": [rel],
                              "where": [{"field": "title", "op": "eq", "value": "Action 1 of tracker 0."}],
                              "fields": []})
        item = q["items"][0]
        guard = item["revision"]
        for i in range(edits):
            _, out = cond("update-item", {"path": rel, "collection": "actions", "id": item["id"],
                                          "guard": guard, "set": {"gate": f"revision {i} holds."}})
            guard = out["ops"][0]["revision"]
    return (root / rel).read_text(), expected


def section_body(cond, root: Path, rels: list[str], sections: int) -> tuple[str, str]:
    """Replace the one-paragraph body of the middle filler section."""
    rel, k = rels[0], sections // 2
    before = (root / rel).read_text()
    paragraph = re.search(rf"^Notes on .*, paragraph {k}\.$", before, re.M).group(0)
    expected = before.replace(paragraph, "Replaced body.", 1)
    if cond.name == "naive":
        text = cond.read(rel)
        cond.write(rel, text.replace(paragraph, "Replaced body.", 1))
    elif cond.name == "patch":
        ((_, n, _),) = cond.grep(f"^## Section {k}$", [rel])
        chunk = cond.read_range(rel, n + 1, n + 3)
        body = next(line for line in chunk.split("\n") if line.startswith("Notes on "))
        cond.patch(rel, body, "Replaced body.")
    else:
        select = {"kind": "heading_path", "path": [f"Section {k}"]}
        _, got = cond("get", {"path": rel, "select": select})
        cond("set", {"path": rel, "select": select, "part": "body", "guard": got["revision"],
                     "content": "\nReplaced body.\n\n"})
    return (root / rel).read_text(), expected


def open_agent_actions(cond, root: Path, rels: list[str]) -> tuple[set, set]:
    """Titles of open agent-owned actions in every tracker's Action section.
    Titles are compared without their `(YYYY-MM-DD)` suffix, which the
    schema reads as a separate date field."""
    bare = lambda t: re.sub(r" \(\d{4}-\d{2}-\d{2}\)$", "", t)
    truth = {
        (rel, bare(a["title"]))
        for rel in rels
        for a in actions((root / rel).read_text())
        if not a["done"] and a["actor"] == "agent"
    }
    found = set()
    if cond.name == "naive":
        for rel in rels:
            for a in actions(cond.read(rel)):
                if not a["done"] and a["actor"] == "agent":
                    found.add((rel, bare(a["title"])))
    elif cond.name == "patch":
        # One grep for headings and candidate lines; section membership from line order.
        section = {}
        for rel, _, line in cond.grep(r"^## |^- \[ \] `agent` ", rels):
            if line.startswith("## "):
                section[rel] = line[3:].strip()
            elif section.get(rel) == "Action":
                found.add((rel, bare(re.search(r"\*\*(.+?)\*\*", line).group(1))))
    else:
        cursor = None
        while True:
            req = {"collection": "actions", "fields": ["title"], "limit": 1000,
                   "where": [{"field": "done", "op": "eq", "value": False},
                             {"field": "actor", "op": "eq", "value": "agent"}]}
            if cursor:
                req["cursor"] = cursor
            _, q = cond("query", req)
            found |= {(item["path"], item["fields"]["title"]) for item in q["items"]}
            cursor = q.get("next_cursor")
            if not cursor:
                break
    return found, truth


def whole_document(cond, root: Path, rels: list[str]) -> tuple[str, str]:
    rel = rels[0]
    text = (root / rel).read_text()
    if cond.name == "naive":
        got = cond.read(rel)
    elif cond.name == "patch":
        got = cond.read_range(rel, 1, text.count("\n") + 1)
    else:
        _, out = cond("get", {"path": rel, "select": {"kind": "document"}, "max_bytes": 1 << 22})
        got = out["text"]
    return got, text


# --------------------------------------------------------------------------
# Runner


def run(binary: str, quick: bool = False) -> list[Result]:
    results: list[Result] = []

    def trial(task: str, param: str, files: int, body, filler: int = 0, conds=("naive", "patch", "rivet")):
        for name in conds:
            with tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp) / "project"
                root.mkdir()
                generated = corpus(root, files, filler)
                rels = list(generated)
                stamp_all(root, rels, binary)
                for rel, text in generated.items():
                    # Stamping may add markers and nothing else.
                    assert unstamp((root / rel).read_text()) == text, f"stamping changed {rel}"
                before = {rel: (root / rel).read_text() for rel in rels}
                log = Log()
                cond = {"naive": Naive, "patch": Patch}.get(name)
                cond = cond(root, log) if cond else Rivet(root, log, binary)
                r = body(cond, root, rels)
                # Every file a task does not edit must come back unchanged.
                edited = set() if task in READ_ONLY else {rels[0]}
                untouched = sum(
                    damage((root / rel).read_text(), before[rel]) for rel in rels if rel not in edited
                )
                results.append(finish(task, param, cond, log, r, untouched))

    trial("complete_one", "1 file", 1, complete_one)
    trial("same_node", "A then B, both from one read", 1, lambda c, r, p: two_writers(c, r, p, True))
    trial("disjoint_nodes", "A then B, both from one read", 1, lambda c, r, p: two_writers(c, r, p, False))
    trial("racing_writers", "tool steps interleaved", 1, racing_patches, conds=("patch", "rivet"))
    trial("warm_revisits", "5 edits, one item", 1, warm_revisits)
    for sections in ([10] if quick else [10, 100, 1000]):
        trial("section_body", f"{sections} sections", 1,
              lambda c, r, p, s=sections: section_body(c, r, p, s), filler=sections)
    trial("query", "10 files" if quick else "100 files", 10 if quick else 100, open_agent_actions)
    trial("whole_document", "1 file", 1, whole_document)
    return results


READ_ONLY = {"query", "whole_document"}


def finish(task: str, param: str, cond, log: Log, r, untouched: int) -> Result:
    notes = ""
    lost = conflicts = 0
    if task == "query":
        found, truth = r
        correct, dmg = found == truth, 0
        notes = f"{len(found)} found, {len(truth)} expected"
    elif task == "whole_document":
        got, text = r
        correct, dmg = got == text, 0
    elif len(r) == 5:
        result, expected, lost, conflicts, want_conflicts = r
        dmg = damage(result, expected)
        correct = lost == 0 and conflicts == want_conflicts and dmg == 0
    else:
        result, expected = r
        dmg = damage(result, expected)
        correct = dmg == 0
    dmg += untouched
    correct = correct and untouched == 0
    return Result(task, param, cond.name, cond.guarantees, correct, dmg, lost, conflicts, notes, **log.totals())


def table(results: list[Result]) -> str:
    head = "| task | param | condition | correct | lost | conflicts | calls | sent B | received B | received tok |"
    rows = [head, "|" + "---|" * 10]
    for r in results:
        rows.append(
            f"| {r.task} | {r.param} | {r.condition} | {'yes' if r.correct else 'NO'} | {r.lost_updates} | "
            f"{r.conflicts_detected} | {r.calls} | {r.sent_bytes} | {r.received_bytes} | "
            f"{r.received_tokens if r.received_tokens is not None else '-'} |"
        )
    return "\n".join(rows)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--output", required=True, type=Path, help="JSON results path (must not exist)")
    ap.add_argument("--quick", action="store_true", help="small sizes only, for tests")
    args = ap.parse_args()
    binary = shutil.which(os.environ.get("RIVET", "markrivet"))
    binary = os.path.abspath(binary) if binary else None
    if not binary:
        print("markrivet not found; set RIVET", file=sys.stderr)
        return 2
    if args.output.exists():
        print(f"{args.output} exists; choose a fresh path", file=sys.stderr)
        return 2
    results = run(binary, args.quick)
    meta = {
        "seed": SEED,
        "tokenizer": TOKENIZER,
        "commit": subprocess.run(["git", "-C", str(ROOT), "rev-parse", "--short", "HEAD"],
                                 capture_output=True, text=True).stdout.strip() or None,
        "conditions": {c.name: c.guarantees for c in (Naive, Patch, Rivet)},
        "note": "Deterministic tool-only trials. No model was called; sizes are payloads, not model usage.",
    }
    args.output.write_text(json.dumps({"meta": meta, "results": [asdict(r) for r in results]}, indent=2) + "\n")
    print(table(results))
    return 0 if all(r.correct or r.condition != "rivet" for r in results) else 1


if __name__ == "__main__":
    sys.exit(main())

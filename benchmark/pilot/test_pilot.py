#!/usr/bin/env python3
"""Offline checks for the pilot harness. No API calls, no spend.

The pilot decides what gets published about rivet against its baselines, so
its budget arithmetic and its graders need evidence of their own. Every check
here drives `pilot` with a scripted model or a hand-built trace.

    RIVET=target/debug/markrivet python3 benchmark/pilot/test_pilot.py
"""
from __future__ import annotations

import contextlib
import copy
import hashlib
import io
import json
import time
import types
import os
import pathlib
import shutil
import subprocess
import sys
import tempfile
import threading

HERE = pathlib.Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
sys.path.insert(0, str(HERE.parent / "trials"))
import pilot  # noqa: E402
import trials  # noqa: E402

class NetworkAttempt(BaseException):
    """Not an Exception: `count_tokens` swallows those, so a suite that
    claims to be offline would keep passing while making real requests."""


def forbid_network() -> None:
    """A test that stubs `call_messages` and forgets the counting endpoint
    would otherwise reach the API with a dummy key: free, but a dependency in
    a suite whose whole claim is that it has none, and a stalled connection
    costs the timeout per call."""
    def refuse(*args, **kwargs):
        raise NetworkAttempt("the offline suite attempted a network request")
    pilot.urllib.request.urlopen = refuse


REFUSAL = "error: old text not found exactly once; re-read and retry"
APPLIED = '{"exit_code": 0, "response": {"ok": true, "status": "applied"}}'
PREVIEW = '{"exit_code": 0, "response": {"ok": true, "status": "preview"}}'
UNCHANGED = '{"exit_code": 0, "response": {"ok": true, "status": "unchanged"}}'
CONFLICT = '{"exit_code": 2, "response": {"ok": false, "status": "refused", "error": {"code": "guard_conflict"}}}'
FAILURES: list[str] = []


def check(name: str, cond: bool, detail: str = "") -> None:
    print(f"  {'ok  ' if cond else 'FAIL'}: {name}{(' | ' + detail) if detail else ''}")
    if not cond:
        FAILURES.append(name)


def binary() -> str:
    found = shutil.which(os.environ.get("RIVET", "markrivet"))
    if not found:
        sys.exit("markrivet not found; set RIVET")
    return os.path.abspath(found)


def project(files: int = 1, filler: int = 0):
    tmp = pathlib.Path(tempfile.mkdtemp())
    root = tmp / "project"
    root.mkdir()
    generated = trials.corpus(root, files, filler)
    trials.stamp_all(root, list(generated), binary())
    return tmp, root, {rel: (root / rel).read_text() for rel in generated}


def call(name: str, t0: float, t1: float, out: str = "ok", inp: dict | None = None,
         rnd: int | None = None) -> dict:
    """One tool call. `rnd` is the agent round that requested it; calls sharing
    a round were requested in one assistant turn, so the model did not see an
    earlier one's result before planning a later one."""
    return {"name": name, "input": inp or {}, "output": out,
            "started": t0, "ended": t1, "round": int(t0) if rnd is None else rnd}


def trace(calls: list[dict], final: str = "") -> pilot.Trace:
    t = pilot.Trace()
    t.tool_calls, t.final_text = calls, final
    return t


# --------------------------------------------------------------------------


def test_budget() -> None:
    print("budget")
    b = pilot.Budget(40, 100, 100)
    b.input_tokens, b.output_tokens = 99, 99
    check("a call that would breach the cap is refused, not admitted",
          b.reserve(20, 20) is None and b.input_tokens == 99)

    b = pilot.Budget(40, 10**9, 100)
    granted: list = []
    def grab():
        r = b.reserve(1, 50)
        if r:
            granted.append(r)
    ts = [threading.Thread(target=grab) for _ in range(8)]
    [t.start() for t in ts]
    [t.join() for t in ts]
    check("racing reservations cannot oversubscribe the cap",
          len(granted) == 2 and b.output_tokens == 100, f"{len(granted)} granted, {b.output_tokens}/100")

    b = pilot.Budget(40, 10**6, 10**6)
    b.settle(b.reserve(1000, 1536), {"input_tokens": 120, "output_tokens": 8})
    check("settle reconciles a reservation to measured usage",
          (b.input_tokens, b.output_tokens) == (120, 8))

    b = pilot.Budget(40, 10**6, 10**6)
    b.settle(b.reserve(1000, 1536), None)
    check("an unknown billing outcome keeps the worst case and stops",
          (b.input_tokens, b.output_tokens) == (1000, 1536) and b.unsettled_calls == 1 and b.stopped_reason)

    b = pilot.Budget(40, 10**6, 10**6)
    counts = []
    def spend():
        for _ in range(500):
            b.settle(b.reserve(2, 2), {"input_tokens": 1, "output_tokens": 1})
        counts.append(1)
    ts = [threading.Thread(target=spend) for _ in range(4)]
    [t.start() for t in ts]
    [t.join() for t in ts]
    check("concurrent settle loses no increments",
          b.calls == 2000 and b.input_tokens == 2000, f"{b.calls} calls, {b.input_tokens} input")


def test_settlement_guards() -> None:
    print("settlement guards")
    b = pilot.Budget(40, 100, 10**6)
    # The input reservation is derived from characters, so it can run low.
    # Measured usage is authoritative and must stop the pilot when it lands over.
    b.settle(b.reserve(80, 10), {"input_tokens": 110, "output_tokens": 1})
    check("an under-estimate that settles over the cap stops the pilot",
          b.input_tokens == 110 and b.stopped_reason, b.stopped_reason or "not stopped")
    check("a stopped budget admits no further calls", b.reserve(1, 1) is None)

    b = pilot.Budget(40, 10**6, 10**6)
    b.settle(b.reserve(10, 10), {"output_tokens": 5})
    check("usage missing input_tokens is an unknown, not a zero",
          b.unsettled_calls == 1 and b.stopped_reason)

    b = pilot.Budget(40, 10**6, 10**6)
    r = b.reserve(1000, 1536)
    b.refund(r)
    check("a refunded reservation leaves no trace",
          (b.input_tokens, b.output_tokens, b.calls) == (0, 0, 0))


def test_input_reservation_is_a_bound() -> None:
    """The reservation must cover the request it admits.

    Reserving less than a call can consume cannot be made safe by checking
    afterwards: with a cap of 100, two reservations of 40 are admitted, and if
    each request really consumes 70 the total reaches 140 before anything
    notices. The reservation is the serialised request's own byte length,
    which covers its own serialisation but is an admission estimate rather
    than a proven bound on the prompt the provider renders and tokenises,
    which is why settlement re-checks the caps against measured usage."""
    print("input reservation")
    real = pilot.call_messages
    seen: list[int] = []

    def measure(key, system, messages, tools, max_tokens):
        body = json.dumps({"system": system, "messages": messages, "tools": tools})
        seen.append(len(body.encode()))
        # Claim far more input than any sane estimate would have reserved.
        return ({"content": [{"type": "text", "text": "done"}], "stop_reason": "end_turn",
                 "usage": {"input_tokens": len(body.encode()), "output_tokens": 1}}, 0.01)

    root = pathlib.Path(tempfile.mkdtemp())
    try:
        pilot.call_messages = measure
        budget = pilot.Budget(40, 10**6, 10**6)
        reserved_at_call: list[int] = []
        inner = measure

        def watch(key, system, messages, tools, max_tokens):
            reserved_at_call.append(budget.input_tokens)
            return inner(key, system, messages, tools, max_tokens)

        pilot.call_messages = watch
        pilot.run_agent("k", budget, pilot.condition_for("naive", root, binary()), "hello", 64)
        check("the reservation covers a call that consumes one token per serialised byte",
              reserved_at_call and reserved_at_call[0] >= seen[0],
              f"reserved {reserved_at_call[0]}, consumed {seen[0]}")

        # The same call under a cap just below its byte length must be refused
        # outright rather than admitted and detected afterwards.
        budget = pilot.Budget(40, seen[0] - 1, 10**6)
        pilot.run_agent("k", budget, pilot.condition_for("naive", root, binary()), "hello", 64)
        check("a call larger than the remaining input cap is never sent",
              budget.calls == 0 and budget.stopped_reason, budget.stopped_reason or "not stopped")
    finally:
        pilot.call_messages = real
        shutil.rmtree(root, ignore_errors=True)


def test_grading_failure_keeps_trace() -> None:
    print("grading failures")
    real = pilot.call_messages

    def one_round(key, system, messages, tools, max_tokens):
        return ({"content": [{"type": "text", "text": "done"}], "stop_reason": "end_turn",
                 "usage": {"input_tokens": 11, "output_tokens": 4}}, 0.25)

    def explodes(root, before, trace):
        raise ValueError("grader is broken")

    try:
        pilot.call_messages = one_round
        budget = pilot.Budget(40, 10**6, 10**6)
        result, record = pilot.single_writer("complete_one", "1 file", "k", budget, "naive", binary(),
                                             pilot.prompt_complete, explodes)
        check("a grader that raises does not cost the trial its measurements",
              result.api_calls == 1 and result.latency_s > 0 and result.input_tokens > 0,
              f"calls={result.api_calls} latency={result.latency_s} in={result.input_tokens}")
        check("a grader that raises is recorded as incorrect, with the reason",
              not result.correct and "grading raised" in result.notes, result.notes)
        check("the paid trace survives a grading failure", record["a"]["calls"])
    finally:
        pilot.call_messages = real


def test_approval_ceilings() -> None:
    print("approval")
    tmp = pathlib.Path(tempfile.mkdtemp())
    os.environ["RIVET_PILOT_API_KEY"] = "dummy"
    for flag, value in (("--max-runs", pilot.APPROVED_MAX_RUNS + 1),
                        ("--max-input-tokens", pilot.APPROVED_MAX_INPUT_TOKENS + 1),
                        ("--max-output-tokens", pilot.APPROVED_MAX_OUTPUT_TOKENS + 1),
                        ("--max-cost-usd", pilot.APPROVED_MAX_COST_USD + 1)):
        out = tmp / f"{flag.strip('-')}.json"
        sys.argv = ["pilot", "--output", str(out), flag, str(value)]
        check(f"{flag} above the approved ceiling is refused",
              pilot.main() == 2 and not out.exists())
    shutil.rmtree(tmp)


def test_schedule() -> None:
    print("schedule")
    conds = ["naive", "patch", "rivet"]
    mixed = pilot.build_plan("rivet")
    expected = [(t, c) for t in ("complete_one", "warm_revisits", "section_body", "whole_document",
                                 "query", "same_node", "disjoint_nodes") for c in conds]
    expected += [("complete_one", c) for c in conds for _ in range(3)]
    check("the mixed schedule is unchanged", [(kw["task"], kw["condname"]) for _, kw in mixed] == expected)
    safety = pilot.build_plan("rivet", 2)
    check("safety mode schedules only disjoint_nodes",
          all(kind == "two" and kw["task"] == "disjoint_nodes" for kind, kw in safety))
    check("safety mode interleaves the conditions, N per condition",
          [kw["condname"] for _, kw in safety] == conds * 2)
    check("each safety entry asks A for action 1 and B for action 2",
          all(kw["prompt_a"]([]) == pilot.prompt_gate(pilot.REL0, 1, "A was here.")
              and kw["prompt_b"]([]) == pilot.prompt_gate(pilot.REL0, 2, "B was here.")
              for _, kw in safety))
    check("no two safety entries share kwargs or a grader",
          len({id(kw) for _, kw in safety}) == len(safety)
          and len({id(kw["grade_fn"]) for _, kw in safety}) == len(safety))
    safety[0][1].pop("grade_fn")
    check("consuming one entry leaves the others intact", all("grade_fn" in kw for _, kw in safety[1:]))
    defaults = pilot.parser().parse_args(["--output", "x.json"])
    check("the run caps default to the approval",
          (defaults.max_runs, defaults.max_input_tokens, defaults.max_output_tokens, defaults.max_cost_usd)
          == (pilot.APPROVED_MAX_RUNS, pilot.APPROVED_MAX_INPUT_TOKENS,
              pilot.APPROVED_MAX_OUTPUT_TOKENS, pilot.APPROVED_MAX_COST_USD))
    saved = (sys.argv, pilot.build_plan, pilot.usage_doc_tokens)
    tmp = pathlib.Path(tempfile.mkdtemp())
    os.environ["RIVET_PILOT_API_KEY"] = "dummy"
    try:
        err = io.StringIO()
        out = tmp / "unreachable.json"
        sys.argv = ["pilot", "--output", str(out), "--safety", "2", "--max-runs", "5"]
        with contextlib.redirect_stderr(err):
            code = pilot.main()
        check("a safety count a lowered run cap cannot reach is refused",
              code == 2 and not out.exists() and "--safety 2" in err.getvalue(), err.getvalue())
        sys.argv = ["pilot", "--output", str(out), "--safety", "-1"]
        with contextlib.redirect_stderr(io.StringIO()):
            check("a negative safety count is refused", pilot.main() == 2 and not out.exists())
        asked: list[int] = []
        pilot.build_plan = lambda binary, safety=0: asked.append(safety) or []
        pilot.usage_doc_tokens = lambda key: None
        ran = tmp / "ran.json"
        sys.argv = ["pilot", "--output", str(ran), "--safety", "2", "--max-runs", "6"]
        with contextlib.redirect_stderr(io.StringIO()), contextlib.redirect_stdout(io.StringIO()):
            code = pilot.main()
        meta = json.loads(ran.read_text())["meta"] if ran.exists() else {}
        check("main schedules the requested safety count and records it",
              code == 0 and asked == [2] and meta.get("safety_per_condition") == 2,
              f"code={code} asked={asked} meta={meta.get('safety_per_condition')}")
    finally:
        sys.argv, pilot.build_plan, pilot.usage_doc_tokens = saved
        shutil.rmtree(tmp)


def test_output_allowance() -> None:
    print("output allowance")
    tmp, root, before = project(filler=100)
    size = len(before[pilot.REL0])
    naive = pilot.output_allowance("section_body", "naive", before)
    other = pilot.output_allowance("section_body", "rivet", before)
    check("the whole-file condition is given room to emit the whole file",
          naive > size // 3, f"{size} bytes, {naive} tokens allowed")
    check("a condition that patches keeps the default allowance",
          other == pilot.DEFAULT_MAX_TOKENS)
    check("whole_document enlarges every condition equally",
          len({pilot.output_allowance("whole_document", c, before) for c in ("naive", "patch", "rivet")}) == 1)
    shutil.rmtree(tmp)


def test_path_confinement() -> None:
    print("path confinement")
    tmp, root, before = project()
    cond = pilot.condition_for("naive", root, binary())
    for path in ("/proc/self/environ", "../../../etc/passwd", "/etc/passwd"):
        out = pilot.dispatch(cond, "read_file", {"path": path})
        check(f"refuses {path}", out.startswith("error: ValueError"))
    check("still reads a path inside the project",
          pilot.dispatch(cond, "read_file", {"path": pilot.REL0}) == before[pilot.REL0])
    shutil.rmtree(tmp)


def test_key_handling() -> None:
    print("key handling")
    os.environ["RIVET_PILOT_API_KEY"] = "sk-test-value"
    check("api_key removes the key from the environment subprocesses inherit",
          pilot.api_key() == "sk-test-value" and "RIVET_PILOT_API_KEY" not in os.environ)
    try:
        pilot.api_key()
        missing = None
    except SystemExit as e:
        missing = e.code
    check("api_key without the variable exits 2 and looks nowhere else", missing == 2, str(missing))
    out = subprocess.run(
        [sys.executable, "-c",
         f"import sys; sys.path.insert(0, {str(HERE)!r}); import pilot; pilot.check_key('sk-ant\\nevil: 1')"],
        capture_output=True, text=True)
    check("a key carrying a newline is refused without being echoed",
          out.returncode == 2 and "sk-ant" not in out.stderr + out.stdout)


def test_untouched() -> None:
    print("untouched files")
    tmp, root, before = project(files=2)
    other = [r for r in before if r != pilot.REL0][0]
    check("a freshly stamped project shows no damage",
          pilot.untouched_damage(root, before, {pilot.REL0}) == 0)

    original = (root / other).read_bytes()
    (root / other).write_bytes(original.replace(b"\n", b"\r\n"))
    check("a wholesale line-ending rewrite is not untouched",
          pilot.untouched_damage(root, before, {pilot.REL0}) > 0)
    (root / other).write_bytes(original)

    text = (root / other).read_text()
    start = text.index("stay:")
    changed = text[:start] + text[start:].replace("0", "1", 1)
    (root / other).write_text(changed)
    check("a changed stay digest is not untouched",
          pilot.untouched_damage(root, before, {pilot.REL0}) > 0)
    (root / other).write_bytes(original)
    (root / "scratch.tmp").write_text("x" * 40)
    check("a file the corpus never generated counts as damage",
          pilot.untouched_damage(root, before, {pilot.REL0}) == 40)
    (root / "scratch.tmp").unlink()
    (root / ".rivet" / "extra").write_text("engine state")
    check("engine state under .rivet/ is not user content and does not count",
          pilot.untouched_damage(root, before, {pilot.REL0}) == 0)
    shutil.rmtree(tmp)


def test_grade_warm() -> None:
    print("grade_warm")
    tmp, root, before = project()
    base = before[pilot.REL0]
    doc = lambda v: trials.set_gate(base, 0, 1, v)
    (root / pilot.REL0).write_text(doc("revision 4 holds."))

    jump = trace([call("write_file", 0, 1, inp={"content": doc("revision 4 holds.")})])
    correct, _, notes = pilot.grade_warm(root, before, jump)
    check("one jump to the final value is not five edits, even with a correct file",
          not correct, notes)

    honest = trace([call("write_file", i, i + 1, inp={"content": doc(f"revision {i} holds.")})
                    for i in range(5)])
    check("five accepted writes in order pass", pilot.grade_warm(root, before, honest)[0])

    backwards = trace([call("write_file", i, i + 1, inp={"content": doc(f"revision {4 - i} holds.")})
                       for i in range(5)])
    check("five writes in the wrong order do not pass",
          not pilot.grade_warm(root, before, backwards)[0])

    refused = trace([call("patch", i, i + 1, out=REFUSAL,
                          inp={"old": "x", "new": f"revision {i} holds."}) for i in range(5)])
    check("five refused writes are not five edits", not pilot.grade_warm(root, before, refused)[0])

    # `patch` carries the replaced text too, so a payload-wide search credits
    # states the file never held.
    smuggled = trace(
        [call("write_file", 0, 1, inp={"content": doc("revision 0 holds. revision 1 holds. "
                                                      "revision 2 holds. revision 3 holds.")})]
        + [call("patch", i, i + 1, inp={"old": f"revision {i} holds.", "new": f"revision {i} holds."})
           for i in range(1, 4)]
        + [call("write_file", 5, 6, inp={"content": doc("revision 4 holds.")})])
    correct, _, notes = pilot.grade_warm(root, before, smuggled)
    check("one write naming every state, plus no-op patches, is not five edits",
          not correct, notes)

    rivet = trace([call("rivet", i, i + 1, out=APPLIED,
                        inp={"command": "update-item", "request": {"gate": f"revision {i} holds."}})
                   for i in range(5)])
    check("five applied rivet writes pass", pilot.grade_warm(root, before, rivet)[0])

    previews = trace([call("rivet", i, i + 1, out=PREVIEW,
                           inp={"command": "update-item", "request": {"preview": True,
                                                                      "gate": f"revision {i} holds."}})
                      for i in range(4)]
                     + [call("rivet", 4, 5, out=APPLIED,
                             inp={"command": "update-item", "request": {"gate": "revision 4 holds."}})])
    correct, _, notes = pilot.grade_warm(root, before, previews)
    check("four previews and one write are not five edits", not correct, notes)

    unchanged = trace([call("rivet", i, i + 1, out=UNCHANGED,
                            inp={"command": "update-item", "request": {"gate": f"revision {i} holds."}})
                       for i in range(5)])
    check("five writes that changed nothing are not five edits",
          not pilot.grade_warm(root, before, unchanged)[0])
    shutil.rmtree(tmp)


def test_edited_file_byte_damage() -> None:
    """The file a task may edit is still held to its bytes outside the edit.

    `untouched_damage` skips it by design, so if its own grader reads through
    a newline-translating API, a correct edit plus a wholesale CRLF rewrite
    grades clean: the one file the task was allowed to touch becomes the one
    place damage is invisible."""
    print("edited-file damage")
    tmp, root, before = project()
    base = before[pilot.REL0]
    line = trials.item_line(base, "Action 1 of tracker 0.")
    done = base.replace(line, line.replace("- [ ]", "- [x]", 1), 1)

    (root / pilot.REL0).write_bytes(done.encode())
    correct, dmg, _ = pilot.grade_complete(root, before, trace([]))
    check("the requested edit alone is correct", correct and dmg == 0)

    (root / pilot.REL0).write_bytes(done.replace("\n", "\r\n").encode())
    correct, dmg, _ = pilot.grade_complete(root, before, trace([]))
    check("the requested edit plus a line-ending rewrite is damage",
          not correct and dmg > 0, f"damage={dmg}")
    shutil.rmtree(tmp)


def test_grade_two_same_node() -> None:
    print("grade_two, same node")
    tmp, root, before = project()
    (root / pilot.REL0).write_text(trials.set_gate(before[pilot.REL0], 0, 1, "B was here."))
    a = trace([call("read_file", 0, 1), call("write_file", 3, 3.1)])
    grade = pilot.grade_two(True)

    b_stale = trace([call("read_file", 1, 1.1), call("write_file", 4, 4.1)])
    correct, _, lost, _, notes = grade(root, before, a, b_stale)
    check("a write planned before the lost one landed is a silent loss",
          lost == 1 and not correct, notes)

    b_serial = trace([call("read_file", 3.5, 3.6, out="gate: A was here."), call("write_file", 4, 4.1)])
    correct, _, lost, _, notes = grade(root, before, a, b_serial)
    check("a writer whose read returned the other's value superseded it",
          lost == 0 and correct, notes)

    # Read after the lost write, but of something else entirely.
    # Read and write requested in the SAME assistant turn: the dispatcher runs
    # them in order, so the read returns the value the write is about to
    # destroy, but the model never saw it before planning that write.
    b_same_turn = trace([call("read_file", 3.5, 3.6, out="gate: A was here.", rnd=1),
                         call("write_file", 4, 4.1, rnd=1)])
    correct, _, lost, _, notes = grade(root, before, a, b_same_turn)
    check("a read beside an already-planned write is not evidence the model saw it",
          lost == 1 and not correct, notes)

    b_elsewhere = trace([call("read_file", 1, 1.1, out="stale copy"),
                         call("rivet", 3.5, 3.6, out='{"exit_code": 0, "response": {"root": "/x"}}',
                              inp={"command": "map"}),
                         call("write_file", 4, 4.1)])
    correct, _, lost, _, notes = grade(root, before, a, b_elsewhere)
    check("a read that never returned the lost value does not prove it was seen",
          not correct and "ambiguous" in notes, notes)

    # `find` answers with an excerpt, so a find that returned the lost value
    # is a read that saw it, as a get or a query would be.
    b_found = trace([call("rivet", 3.5, 3.6, inp={"command": "find"},
                          out='{"exit_code": 0, "response": {"matches": [{"excerpt": "gate: A was here."}]}}'),
                     call("rivet", 4, 4.1, inp={"command": "update-item"},
                          out='{"exit_code": 0, "response": {"status": "applied"}}')])
    check("a find result is a read", pilot.is_read(b_found.tool_calls[0]))
    correct, _, lost, _, notes = grade(root, before, a, b_found)
    check("a writer whose find returned the other's value superseded it",
          lost == 0 and correct, notes)

    # A refusal echoes what was sent, so the lost value in a failed read's
    # error is the model's own request, not something it was shown.
    for why, failed in [
        ("a refused find", call("rivet", 3.5, 3.6, inp={"command": "find"},
                                out='{"exit_code": 2, "response": {"ok": false, "error": '
                                    '{"code": "not_found", "path": "gate: A was here."}}}')),
        ("a tool error", dict(call("read_file", 3.5, 3.6, out="error: FileNotFoundError: gate: A was here."),
                              tool_error=True)),
    ]:
        b_failed = trace([failed, call("rivet", 4, 4.1, inp={"command": "update-item"},
                                       out='{"exit_code": 0, "response": {"status": "applied"}}')])
        correct, _, lost, _, notes = grade(root, before, a, b_failed)
        check(f"{why} that echoed the lost value is not a read that saw it",
              not correct and "ambiguous" in notes, notes)

    # Content is content whatever it begins with: only the recorded failure
    # empties a read.
    b_prefixed = trace([call("read_file", 3.5, 3.6, out="error: summary\n**Gate**: A was here."),
                        call("write_file", 4, 4.1)])
    correct, _, lost, _, notes = grade(root, before, a, b_prefixed)
    check("a successful read whose content begins error: still shows the value",
          lost == 0 and correct, notes)
    check("dispatch records a harness failure as a tool error, not as content",
          isinstance(pilot.dispatch(trials.Naive(root, trials.Log()), "read_file",
                                    {"path": "no/such.md"}), pilot.ToolError)
          and not isinstance(pilot.dispatch(trials.Naive(root, trials.Log()), "read_file",
                                            {"path": pilot.REL0}), pilot.ToolError))

    # Through the agent loop: the flag is recorded on the call, so a failed
    # read's echoed path still cannot pass for content.
    def reads(path):
        def scripted(key, system, messages, tools, max_tokens):
            if not any(m["role"] == "assistant" for m in messages):
                return ({"content": [{"type": "tool_use", "id": "r", "name": "read_file",
                                      "input": {"path": path}}],
                         "stop_reason": "tool_use", "usage": {"input_tokens": 10, "output_tokens": 5}}, 0.01)
            return ({"content": [{"type": "text", "text": "done"}], "stop_reason": "end_turn",
                     "usage": {"input_tokens": 10, "output_tokens": 5}}, 0.01)
        return scripted

    missing = "gate: A was here.md"
    real = pilot.call_messages
    recorded = {}
    try:
        for path in (missing, pilot.REL0):
            pilot.call_messages = reads(path)
            t = pilot.run_agent("k", pilot.Budget(40, 10**6, 10**6),
                                pilot.condition_for("naive", root, binary()), "task", 64)
            recorded[path] = dict(json.loads(json.dumps(t.tool_calls[0])), started=3.5, ended=3.6, round=3)
    finally:
        pilot.call_messages = real
    check("the agent loop records a failed read as a tool error, and a good read as not",
          recorded[missing]["tool_error"] is True and recorded[pilot.REL0]["tool_error"] is False
          and "A was here." in recorded[missing]["output"], str(recorded[missing]))
    correct, _, lost, _, notes = grade(root, before, a, trace([recorded[missing], call("write_file", 4, 4.1)]))
    check("a recorded failed read that echoed the lost value did not show it",
          not correct and "ambiguous" in notes, notes)

    a_refused = trace([call("read_file", 0, 1), call("patch", 3, 3.1, out=REFUSAL)])
    correct, _, lost, _, notes = grade(root, before, a_refused, b_stale)
    check("a refusal the writer was told about is not a silent loss",
          lost == 0 and correct, notes)

    b_blind = trace([call("write_file", 4, 4.1)])
    correct, _, lost, _, notes = grade(root, before, a, b_blind)
    check("a write with no read at all counts as stale", lost == 1, notes)

    correct, _, lost, _, notes = grade(root, before, a, trace([]))
    check("a trial where one writer never wrote is not a concurrency observation",
          not correct and "never attempted" in notes, notes)

    # A refused attempt the writer then retried successfully does not excuse
    # that successful write being destroyed afterwards.
    a_retried = trace([call("read_file", 0, 1), call("patch", 2, 2.1, out=REFUSAL),
                       call("read_file", 2.2, 2.3), call("write_file", 3, 3.1)])
    correct, _, lost, _, notes = grade(root, before, a_retried, b_stale)
    check("an earlier refusal does not excuse a later acknowledged write vanishing",
          lost == 1, notes)
    shutil.rmtree(tmp)


def test_grade_two_disjoint() -> None:
    print("grade_two, disjoint nodes")
    tmp, root, before = project()
    grade = pilot.grade_two(False)
    w = [call("write_file", 1, 2)]
    both = trials.set_gate(trials.set_gate(before[pilot.REL0], 0, 1, "A was here."), 0, 2, "B was here.")

    (root / pilot.REL0).write_text(both)
    correct, _, lost, _, notes = grade(root, before, trace(w), trace(w))
    check("both disjoint edits surviving is correct", correct and lost == 0, notes)

    (root / pilot.REL0).write_text(trials.set_gate(before[pilot.REL0], 0, 1, "A was here."))
    correct, _, lost, _, notes = grade(root, before, trace(w), trace(w))
    check("a disjoint edit that vanished silently is a lost update",
          not correct and lost == 1, notes)

    correct, _, lost, _, notes = grade(root, before, trace(w), trace([call("patch", 1, 2, out=REFUSAL)]))
    check("a disjoint edit that was refused out loud is not a lost update",
          lost == 0 and "refused=1" in notes, notes)
    shutil.rmtree(tmp)


def test_report_grading() -> None:
    print("report grading")
    import re
    tmp, root, before = project(files=3)
    rels = list(before)
    truth = [(r, re.sub(r" \(\d{4}-\d{2}-\d{2}\)$", "", a["title"]))
             for r in rels for a in trials.actions(before[r]) if not a["done"] and a["actor"] == "agent"]
    body = "\n".join(f"{r}: {t}" for r, t in truth)
    grade = pilot.grade_query(rels)
    check("an exact answer passes", grade(root, before, trace([], body))[0])
    check("trailing commentary fails a task that said nothing else",
          not grade(root, before, trace([], body + "\nHope that helps!"))[0])
    check("a duplicated answer line fails",
          not grade(root, before, trace([], body + "\n" + body.splitlines()[0]))[0])

    want = before[pilot.REL0]
    for label, text, expect in (("bare", want, True),
                                ("one-line preamble", "Here it is:\n" + want, True),
                                ("fenced", "```markdown\n" + want + "\n```", True),
                                ("leading whitespace", "   " + want, False),
                                ("two-line preamble", "One\nTwo\n" + want, False),
                                ("truncated", want[: len(want) // 2], False)):
        correct, _, notes = pilot.grade_wholedoc(root, before, trace([], text))
        check(f"verbatim report, {label}: {'accepted' if expect else 'rejected'}", correct == expect, notes)
    shutil.rmtree(tmp)


def test_run_accounting() -> None:
    print("run accounting")
    tmp = pathlib.Path(tempfile.mkdtemp())
    os.environ["RIVET_PILOT_API_KEY"] = "dummy"
    real = pilot.call_messages

    def scripted(key, system, messages, tools, max_tokens):
        turns = len([m for m in messages if m["role"] == "assistant"])
        names = [t["name"] for t in tools]
        if "write_file" in names and turns == 0:
            return ({"content": [{"type": "tool_use", "id": "r", "name": "read_file",
                                  "input": {"path": pilot.REL0}}],
                     "stop_reason": "tool_use", "usage": {"input_tokens": 10, "output_tokens": 5}}, 0.01)
        if "write_file" in names and turns == 1:
            text = messages[-1]["content"][0]["content"]
            line = trials.item_line(text, "Action 1 of tracker 0.")
            return ({"content": [{"type": "tool_use", "id": "w", "name": "write_file",
                                  "input": {"path": pilot.REL0,
                                            "content": text.replace(line, line.replace("- [ ]", "- [x]", 1), 1)}}],
                     "stop_reason": "tool_use", "usage": {"input_tokens": 20, "output_tokens": 40}}, 0.01)
        return ({"content": [{"type": "text", "text": "done"}], "stop_reason": "end_turn",
                 "usage": {"input_tokens": 5, "output_tokens": 3}}, 0.01)

    measured = pilot.usage_doc_tokens
    try:
        pilot.call_messages = scripted
        # `main` measures the guide before it opens the budget. This suite
        # refuses the network, so supply the figure the way a run would have.
        pilot.usage_doc_tokens = lambda key: 9_000
        out = tmp / "ok.json"
        sys.argv = ["pilot", "--output", str(out), "--max-runs", "6"]
        os.environ["RIVET_PILOT_API_KEY"] = "dummy"  # api_key pops it, by design
        pilot.main()
        data = json.loads(out.read_text())
        first = data["results"][0]
        check("a scripted agent completes the whole-file condition correctly",
              first["correct"] and first["damage_bytes"] == 0, first["notes"])
        check("summed result tokens reconcile with the meta totals",
              (sum(r["input_tokens"] for r in data["results"]),
               sum(r["output_tokens"] for r in data["results"]))
              == (data["meta"]["input_tokens"], data["meta"]["output_tokens"]))

        def explode(key, system, messages, tools, max_tokens):
            raise pilot.TransportError("connection reset", billable=True)

        pilot.call_messages = explode
        out = tmp / "lost.json"
        sys.argv = ["pilot", "--output", str(out), "--max-runs", "3"]
        os.environ["RIVET_PILOT_API_KEY"] = "dummy"
        pilot.main()
        data = json.loads(out.read_text())
        check("a transport failure still writes the results file", out.exists())
        check("the reservation for a call of unknown billing survives into the result",
              data["results"][0]["input_tokens"] == data["meta"]["input_tokens"] > 0)
        check("an unknown billing outcome stops the pilot",
              data["meta"]["unsettled_calls"] == 1 and data["meta"]["stopped_reason"])

        # A refusal the API served without doing work is free to retry, and
        # each attempt must reserve and release on its own.
        attempts = {"n": 0}

        def flaky(key, system, messages, tools, max_tokens):
            attempts["n"] += 1
            if attempts["n"] < 3:
                raise pilot.TransportError("HTTP 529: overloaded", billable=False)
            return ({"content": [{"type": "text", "text": "done"}], "stop_reason": "end_turn",
                     "usage": {"input_tokens": 7, "output_tokens": 2}}, 0.01)

        pilot.call_messages = flaky
        slept = pilot.time.sleep
        pilot.time.sleep = lambda _: None
        budget = pilot.Budget(40, 10**6, 10**6)
        # Observed inside each attempt: a run that reserved once for all three
        # would show the same final totals, so totals alone prove nothing.
        seen = []
        inner = flaky

        def watched(key, system, messages, tools, max_tokens):
            seen.append((budget.calls, budget.output_tokens))
            return inner(key, system, messages, tools, max_tokens)

        pilot.call_messages = watched
        cond_root = pathlib.Path(tempfile.mkdtemp())
        try:
            pilot.run_agent("k", budget, pilot.condition_for("naive", cond_root, binary()), "hello", 1536)
            check("every attempt holds its own reservation while in flight",
                  seen == [(1, 1536), (1, 1536), (1, 1536)], str(seen))
            check("an unbilled refusal is retried and its reservation released",
                  budget.calls == 1 and attempts["n"] == 3 and budget.input_tokens == 7,
                  f"{budget.calls} call charged over {attempts['n']} attempts, {budget.input_tokens} input")
        finally:
            pilot.time.sleep = slept
            shutil.rmtree(cond_root, ignore_errors=True)

        # A billable failure must not be retried at all.
        tries = {"n": 0}

        def billed(key, system, messages, tools, max_tokens):
            tries["n"] += 1
            raise pilot.TransportError("ConnectionResetError", billable=True)

        pilot.call_messages = billed
        budget = pilot.Budget(40, 10**6, 10**6)
        cond_root = pathlib.Path(tempfile.mkdtemp())
        try:
            t = pilot.run_agent("k", budget, pilot.condition_for("naive", cond_root, binary()), "hello", 1536)
            check("a failure that may have been billed is not retried",
                  tries["n"] == 1 and budget.unsettled_calls == 1 and "billing unknown" in (t.aborted or ""),
                  f"{tries['n']} attempt, aborted={t.aborted}")
        finally:
            shutil.rmtree(cond_root, ignore_errors=True)
    finally:
        pilot.call_messages = real
        pilot.usage_doc_tokens = measured
        shutil.rmtree(tmp)


def test_concurrent_path_runs() -> None:
    """End to end through `two_writers`, with a scripted model.

    The graders are tested above on hand-built traces, which never calls
    `run_agent` the way a concurrent trial does. That gap let a `two_writers`
    call pass an argument `run_agent` did not accept: every concurrent trial
    raised, all six were recorded as failures, and the pilot produced no
    concurrency data at all while still exiting 0."""
    print("concurrent path")
    real = pilot.call_messages
    attempted: set[str] = set()

    def scripted(key, system, messages, tools, max_tokens):
        turns = len([m for m in messages if m["role"] == "assistant"])
        # Block form, not a string: reading `messages[0]["content"]` as text
        # matches nothing, and both writers would then attempt B's edit while
        # the outcome assertions below still passed.
        prompt = "".join(b.get("text", "") for b in messages[0]["content"])
        assert "A was here." in prompt or "B was here." in prompt, f"unrecognised prompt: {prompt!r}"
        want = "A was here." if "A was here." in prompt else "B was here."
        attempted.add(want)
        if turns == 0:
            return ({"content": [{"type": "tool_use", "id": "r", "name": "read_file",
                                  "input": {"path": pilot.REL0}}],
                     "stop_reason": "tool_use", "usage": {"input_tokens": 9, "output_tokens": 4}}, 0.01)
        if turns == 1:
            text = messages[-1]["content"][0]["content"]
            k = 1 if want == "A was here." else 2
            try:
                content = trials.set_gate(text, 0, k, want)
            except StopIteration:
                # `naive` writes whole files without atomic replacement, so a
                # concurrent read can land mid-write and come back torn. That
                # is the baseline behaving as advertised; a real agent would
                # give up rather than write back something it could not parse.
                return ({"content": [{"type": "text", "text": "the file did not parse"}],
                         "stop_reason": "end_turn", "usage": {"input_tokens": 6, "output_tokens": 3}}, 0.01)
            return ({"content": [{"type": "tool_use", "id": "w", "name": "write_file",
                                  "input": {"path": pilot.REL0, "content": content}}],
                     "stop_reason": "tool_use", "usage": {"input_tokens": 12, "output_tokens": 30}}, 0.01)
        return ({"content": [{"type": "text", "text": "done"}], "stop_reason": "end_turn",
                 "usage": {"input_tokens": 4, "output_tokens": 2}}, 0.01)

    try:
        pilot.call_messages = scripted
        budget = pilot.Budget(40, 10**6, 10**6)
        try:
            result, record = pilot.two_writers(
                "disjoint_nodes", "A then B, concurrent", "k", budget, "naive", binary(),
                lambda rels: pilot.prompt_gate(pilot.REL0, 1, "A was here."),
                lambda rels: pilot.prompt_gate(pilot.REL0, 2, "B was here."),
                pilot.grade_two(False))
        except Exception as e:
            check("a concurrent trial runs to a verdict instead of raising",
                  False, f"{type(e).__name__}: {e}")
            return
        check("a concurrent trial runs to a verdict instead of raising",
              result.aborted is None, result.aborted or result.notes)
        check("both writers are traced", bool(record["a"]["calls"]) and bool(record["b"]["calls"]))
        check("the two writers attempt their own distinct edits",
              attempted == {"A was here.", "B was here."}, str(sorted(attempted)))
        check("a concurrent trial charges both writers to the budget",
              result.input_tokens > 0 and result.api_calls >= 4,
              f"in={result.input_tokens} calls={result.api_calls}")
        # Which writer wins, and whether a torn read happens, are properties of
        # the race. That a verdict is reached and accounted for is not.
        check("a concurrent trial reaches a classified outcome",
              result.notes and ("lost_silently" in result.notes or "never attempted" in result.notes),
              result.notes)

        # A cancelled writer stops before its next call.
        budget = pilot.Budget(40, 10**6, 10**6)
        cancelled = threading.Event()
        cancelled.set()
        root = pathlib.Path(tempfile.mkdtemp())
        try:
            t = pilot.run_agent("k", budget, pilot.condition_for("naive", root, binary()),
                                "anything", 1536, cancel=cancelled)
            check("a cancelled agent makes no calls at all",
                  t.rounds == 0 and budget.calls == 0 and "cancelled" in (t.aborted or ""),
                  f"rounds={t.rounds} calls={budget.calls}")
        finally:
            shutil.rmtree(root, ignore_errors=True)

        # Both writers record into one tally, so the trial's attribution
        # covers every call it was charged for. A result built from one
        # writer's would cover half of them and still look complete.
        shared = pilot.Budget(40, 10**6, 10**6)
        paired, _ = pilot.two_writers(
            "same_node", "A then B, concurrent", "k", shared, "naive", binary(),
            lambda rels: pilot.prompt_gate(pilot.REL0, 1, "A was here."),
            lambda rels: pilot.prompt_gate(pilot.REL0, 2, "B was here."),
            pilot.grade_two(False))
        check("a concurrent trial's attribution covers both writers' charged calls",
              paired.attribution_known and shared.calls >= 4,
              f"known={paired.attribution_known} over {shared.calls} calls")
    finally:
        pilot.call_messages = real


def test_output_preflight() -> None:
    print("output preflight")
    tmp = pathlib.Path(tempfile.mkdtemp())
    os.environ["RIVET_PILOT_API_KEY"] = "dummy"
    out = tmp / "taken.json"
    out.write_text("{}")
    sys.argv = ["pilot", "--output", str(out)]
    check("an existing output path is refused", pilot.main() == 2)
    out.unlink()
    out.with_name("taken-traces.json").write_text("[]")
    sys.argv = ["pilot", "--output", str(out)]
    check("an existing companion traces file is refused before it is clobbered",
          pilot.main() == 2)
    shutil.rmtree(tmp)


def rendered(messages: list[dict]) -> list[str]:
    """Each message as it was transmitted, markers removed.

    The cache key is the rendered prompt, so this compares serialised text
    rather than dictionaries: `==` on dicts ignores key order, and reordered
    keys are a documented way to break a prefix while every structural
    comparison still passes. Insertion order is preserved throughout, and
    nothing here sorts."""
    out = []
    for m in messages:
        blocks = [{k: v for k, v in b.items() if k != "cache_control"} for b in m["content"]]
        out.append(json.dumps({k: (blocks if k == "content" else v) for k, v in m.items()}))
    return out


def marks(messages: list[dict]) -> list[tuple[int, int]]:
    return [(mi, bi) for mi, m in enumerate(messages)
            for bi, b in enumerate(m["content"]) if "cache_control" in b]


def capture_requests(bodies: list[dict]):
    """Stub the transport, not the request builder.

    A stub that replaces `call_messages` never sees what would be sent, so a
    value that varies per request between the builder and the socket, or a
    serialisation that reorders keys, would pass every assertion made on the
    arguments. These bodies are the bytes."""
    class Response:
        def __init__(self, payload):
            self.payload = payload

        def __enter__(self):
            return self

        def __exit__(self, *exc):
            return False

        def read(self):
            return self.payload

    def fake_urlopen(req, timeout=None):
        bodies.append(json.loads(req.data))
        turns = len([m for m in bodies[-1]["messages"] if m["role"] == "assistant"])
        if turns < 2:
            content = [{"type": "thinking", "thinking": "considering", "signature": "s"},
                       {"type": "text", "text": f"step {turns}"},
                       {"type": "tool_use", "id": f"t{turns}", "name": "read_file",
                        "input": {"path": pilot.REL0}}]
            stop = "tool_use"
        else:
            content = [{"type": "text", "text": "done"}]
            stop = "end_turn"
        return Response(json.dumps({
            "content": content, "stop_reason": stop,
            "usage": {"input_tokens": 10, "output_tokens": 5},
        }).encode())

    return fake_urlopen


def test_cache_breakpoints() -> None:
    """Caching is not free when it misses: an entry nothing reads back costs
    1.25x where an uncached token costs 1x, so a breakpoint that cannot be
    found again makes the run dearer than no caching at all. Two properties
    carry that: the prefix must be byte-identical across rounds, and every
    request must mark the block where the previous request wrote, which is the
    only placement a wide turn cannot push out of the lookup window."""
    print("cache breakpoints")
    real = pilot.urllib.request.urlopen
    bodies: list[dict] = []
    tmp, root, _ = project()
    try:
        pilot.urllib.request.urlopen = capture_requests(bodies)
        budget = pilot.Budget(40, 10**6, 10**6)
        pilot.run_agent("k", budget, pilot.condition_for("naive", root, binary()), "task", 64)
        payloads = [b["messages"] for b in bodies]
        check("more than one round was captured", len(payloads) >= 3, f"{len(payloads)} requests")

        counts = [len(marks(pl)) for pl in payloads]
        check("a request never carries more than the breakpoint limit",
              all(1 <= c <= 4 for c in counts), str(counts))
        check("the tail of every request is marked",
              all(marks(pl)[-1][0] == len(pl) - 1 for pl in payloads), str([marks(pl) for pl in payloads]))
        check("the request the model answered carried thinking blocks",
              any(b.get("type") == "thinking" for pl in payloads for m in pl for b in m["content"]))
        check("no breakpoint lands on a block that cannot carry one",
              not any(pl[mi]["content"][bi].get("type") in pilot.UNCACHEABLE_BLOCKS
                      for pl in payloads for mi, bi in marks(pl)),
              "a marker on a thinking block is rejected outright and stops the pilot")

        # The property a wide turn cannot break: every request marks the block
        # the previous request wrote at, so the lookup has no distance to walk.
        anchored = [marks(nxt)[0] == marks(prev)[-1] for prev, nxt in zip(payloads, payloads[1:])]
        check("every request marks where the previous one wrote",
              all(anchored), f"{sum(anchored)}/{len(anchored)} rounds")

        overlaps = [rendered(a) == rendered(b)[:len(a)] for a, b in zip(payloads, payloads[1:])]
        check("each request's prefix reappears unchanged in the next",
              all(overlaps), f"{sum(overlaps)}/{len(overlaps)} rounds")
        check("system and tools are byte-identical across rounds, key order included",
              len({(json.dumps(b["system"]), json.dumps(b["tools"])) for b in bodies}) == 1)
        check("the task prompt is sent in block form, so the first round is cacheable too",
              payloads[0][0]["content"][0]["type"] == "text")

        # One turn wide enough to push the previous endpoint out of the lookup
        # window on its own. Short turns do not exercise this: a placement
        # that drops the anchor once the gap opens still passes against them,
        # which is what makes the width of this fixture the whole test.
        wide = [{"role": "user", "content": [{"type": "text", "text": "task"}]}]
        turn = []
        for i in range(13):
            turn.append({"type": "text", "text": f"step {i}"})
            turn.append({"type": "tool_use", "id": f"t{i}", "name": "read_file", "input": {}})
        wide.append({"role": "assistant", "content": turn})
        wide.append({"role": "user", "content":
                     [{"type": "tool_result", "tool_use_id": f"t{i}", "content": "ok"}
                      for i in range(13)]})
        anchor = 1  # the previous request was the task prompt alone
        gap = len(turn)
        check("the fixture turn is wider than any lookup window",
              gap > 20, f"{gap} blocks in one turn")
        with_anchor = marks(pilot.cached_messages(wide, anchor))
        check("a wide turn is still anchored to the previous endpoint",
              with_anchor == [(0, 0), (2, 12)], str(with_anchor))
        check("the invariant would fail against a tail-only marker",
              marks(pilot.cached_messages(wide)) == [(2, 12)],
              "a tail-only placement leaves the previous endpoint unmarked")
        check("marking leaves the conversation itself unmarked",
              not marks(wide))

        thinking_only = [{"role": "assistant",
                          "content": [{"type": "thinking", "thinking": "x", "signature": "s"}]}]
        check("a turn with nowhere to put a marker gets none rather than an illegal one",
              not marks(pilot.cached_messages(thinking_only)))
        check("an empty conversation is returned untouched", pilot.cached_messages([]) == [])
    finally:
        pilot.urllib.request.urlopen = real
        shutil.rmtree(tmp, ignore_errors=True)


def test_cost_accounting() -> None:
    """The re-run is approved in dollars, so the dollars have to be bounded
    the way the tokens are: reserved before the call at a rate no settlement
    can exceed, then settled to measured usage."""
    print("cost accounting")
    m = 1_000_000
    check("an uncached input token is priced at the base rate",
          abs(pilot.cost_of(m, 0, 0, 0) - pilot.PRICE_INPUT_PER_MTOK) < 1e-9)
    check("a cache write costs 1.25x base input",
          abs(pilot.cost_of(0, m, 0, 0) - pilot.PRICE_INPUT_PER_MTOK * 1.25) < 1e-9)
    check("a cache read costs 0.1x base input",
          abs(pilot.cost_of(0, 0, m, 0) - pilot.PRICE_INPUT_PER_MTOK * 0.10) < 1e-9)
    check("output is priced at the output rate",
          abs(pilot.cost_of(0, 0, 0, m) - pilot.PRICE_OUTPUT_PER_MTOK) < 1e-9)

    est, out = 10_000, 500
    worst = pilot.worst_case_cost(est, out)
    splits = [(est, 0, 0), (0, est, 0), (0, 0, est), (est // 2, est // 4, est - est // 2 - est // 4)]
    check("the reservation bounds every way the same prompt could settle",
          all(pilot.cost_of(*sp, out) <= worst + 1e-12 for sp in splits),
          f"worst ${worst:.4f}")

    b = pilot.Budget(40, 10**9, 10**9, 0.01)
    check("a call that would breach the cost cap is refused, not admitted",
          b.reserve(10_000, 1_000) is None and b.cost_usd == 0 and b.calls == 0 and b.stopped_reason,
          b.stopped_reason or "not stopped")

    b = pilot.Budget(40, 10**9, 10**9, 100.0)
    usage = {"input_tokens": 10, "cache_creation_input_tokens": 1_000,
             "cache_read_input_tokens": 5_000, "output_tokens": 20}
    b.settle(b.reserve(1_000, 100), usage)
    check("settlement prices the cache split rather than the token total",
          abs(b.cost_usd - pilot.cost_of(10, 1_000, 5_000, 20)) < 1e-12, f"${b.cost_usd:.6f}")
    check("the token cap still counts cached tokens, which cost dollars but not headroom",
          b.input_tokens == 6_010 and b.cache_read_tokens == 5_000 and b.cache_creation_tokens == 1_000,
          f"in={b.input_tokens} read={b.cache_read_tokens} written={b.cache_creation_tokens}")

    b = pilot.Budget(40, 10**9, 10**9, 100.0)
    b.refund(b.reserve(1_000, 100))
    check("a refunded reservation leaves no cost behind", b.cost_usd == 0 and b.calls == 0)

    b = pilot.Budget(40, 10**9, 10**9, 0.05)
    b.settle(b.reserve(10, 10), {"input_tokens": 10**6, "output_tokens": 10**6})
    check("a settlement over the cost cap stops the pilot",
          b.stopped_reason and "cost cap" in b.stopped_reason, b.stopped_reason or "not stopped")

    # NaN loses every comparison, so a cap set to it is admitted by the
    # approval check and then binds nothing: the run spends to the token
    # ceilings with a dollar cap that cannot be exceeded by construction.
    tmp = pathlib.Path(tempfile.mkdtemp())
    try:
        for value in ("nan", "-1", "inf"):
            out = tmp / f"cost-{value}.json"
            sys.argv = ["pilot", "--output", str(out), "--max-cost-usd", value]
            os.environ["RIVET_PILOT_API_KEY"] = "dummy"
            check(f"--max-cost-usd {value} is refused before anything is spent",
                  pilot.main() == 2 and not out.exists())
        try:
            pilot.Budget(40, 10**9, 10**9, float("nan"))
            check("a budget cannot hold a cap nothing can exceed", False, "NaN accepted")
        except ValueError:
            check("a budget cannot hold a cap nothing can exceed", True)
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def test_orientation_subset() -> None:
    """A compact orientation is the guide's own sections, cut by heading. The
    failures that matter are the quiet ones: a renamed section dropped without
    a word, a `#` comment in a code block taken for a heading and a section cut
    short at it, or `read_docs` still handing back the whole guide."""
    print("orientation subset")
    doc = ("# Guide\nintro\n## A\na body\n```sh\n# not a heading\n```\na tail\n"
           "### A1\na1 body\n## B\nb body\n### B1\nb1 body\n## C\nc body\n")
    got = pilot.guide_subset(doc, [("## A", "tree"), ("## B", "own")])
    check("a tree section keeps its subsections and runs past a fenced # line",
          got.startswith("## A\na body\n```sh\n# not a heading\n```\na tail\n### A1\na1 body\n"), repr(got))
    check("an own section stops at its first subsection",
          got.endswith("## B\nb body\n") and "B1" not in got, repr(got))
    check("sections come back in document order whatever order they are named",
          pilot.guide_subset(doc, [("## C", "tree"), ("## A", "own")]).startswith("## A"))
    for wanted, why in (([("## Missing", "tree")], "a missing heading"),
                        ([("## A", "tree"), ("### A1", "tree")], "overlapping sections"),
                        ([("## A", "whole")], "an unknown mode")):
        try:
            pilot.guide_subset(doc, wanted)
            check(f"{why} is refused", False)
        except ValueError:
            check(f"{why} is refused", True)
    try:
        pilot.guide_subset(doc + "## A\nagain\n", [("## A", "tree")])
        check("a heading that appears twice is refused", False)
    except ValueError:
        check("a heading that appears twice is refused", True)

    for name in ("engine", "minimal", "minimal-find"):
        text = pilot.orientation_text(name)
        heads = [t for t, _ in pilot.ORIENTATIONS[name]]
        check(f"the {name} orientation resolves against the current guide, each section once",
              all(text.count(h + "\n") == 1 for h in heads))
        check(f"the {name} orientation is smaller than the guide", len(text) < len(pilot.FULL_GUIDE) / 2,
              f"{len(text)} of {len(pilot.FULL_GUIDE)}")
    engine = pilot.orientation_text("engine")
    check("minimal is a cut of engine, not a different text",
          all(h in engine for h, _ in pilot.ORIENTATIONS["minimal"])
          and len(pilot.orientation_text("minimal")) < len(engine))
    check("full is the whole guide", pilot.orientation_text("full") == pilot.FULL_GUIDE)
    minimal = [t for t, _ in pilot.ORIENTATIONS["minimal"]]
    check("minimal-find is minimal plus the find section and nothing else",
          sorted(t for t, _ in pilot.ORIENTATIONS["minimal-find"])
          == sorted(minimal + [pilot.FIND_HEADING]))
    # Each cut's lines naming a command it does not document, as reviewed
    # (the protocol's `doctor`, the `status` field, the guard table's
    # pointers; engine's "`adopt_stay` (see Repairs)" is a real one, left
    # while engine is not the measured cut), pinned by digest. A changed line fails here until someone has
    # checked it does not send an agent to a command the cut leaves out, as
    # § find's "goes straight to `get`" did on 2026-10-19; then the digest
    # moves to what the failure prints.
    # Delete documents verify-delete as structural evidence, not queue membership.
    # The path contract mentions doctor beside archive path normalization;
    # it adds no navigation to a command omitted by these cuts.
    # Frontmatter guards cover a top-level key's complete source lines.
    # The Writing command list also names the two guarded key operations;
    # engine already omits the other operations in that same pointer list.
    # Writing lists the paragraph-removal shorthand beside the other omitted
    # structural commands; this is a protocol list, not a next-step pointer.
    # The action guard table describes the canonical binding fingerprint.
    # Only that description changed; its command pointers are the same.
    # The Writing shorthand list adds `append-key-item` beside the other key
    # writes engine omits; a protocol list again, not a next-step pointer.
    # Selectors compares a returned position handle to `find`'s `select`
    # (copied whole); it does not send the agent to `find`. § `delete` lists
    # the structural writes that refuse a position, a protocol list again,
    # and its next step names only `set` and `stamp`, both in engine.
    # § File records and § Record writes name key writes (`add_key`,
    # `set_key_field`, `delete_key`, `set_key_yaml`, `append_key_item`) in the
    # "As" column of the record-write table and in prose, as the native
    # operation a record edit matches, never as a next step; `append` and `set`
    # there are update-item's request fields, `status` is a frontmatter key,
    # and `doctor`, `find`, `move` and `create` appear in scope and refusal
    # lists. None sends an agent to a command the cut leaves out.
    reviewed = {"engine": "a1ca2de0d5d9f0c0", "minimal": "38a7f2526c2b431a",
                "minimal-find": "02c267f96d36b0fa"}
    saved = pilot.FULL_GUIDE
    try:
        pilot.FULL_GUIDE = ("## A\nUse `append` or `commit-pending` next.\n"
                            "```\n### `append`\n```\n### `append`\nbody\n"
                            "### Recovery: `commit_pending`\nbody\n")
        named = pilot.undocumented_commands("## A\nUse `append` or `commit-pending` next.\n"
                                            "```\n### `append`\n```\n")
        check("a fenced heading does not carry its command, and a prefixed heading names one",
              sorted(named) == ["append", "commit_pending"], str(named))
    finally:
        pilot.FULL_GUIDE = saved
    for name, expected in reviewed.items():
        named = pilot.undocumented_commands(pilot.orientation_text(name))
        digest = hashlib.sha256(json.dumps(named, sort_keys=True).encode()).hexdigest()[:16]
        check(f"the {name} orientation names no command it leaves out beyond the reviewed lines",
              digest == expected,
              "" if digest == expected else f"{digest} over {json.dumps(named, indent=1)}")
    offered = {name: pilot.advertises_find(pilot.orientation_text(name)) for name in pilot.ORIENTATIONS}
    check("find is offered exactly where the guide documents it",
          offered == {"full": True, "engine": False, "minimal": False, "minimal-find": True}, str(offered))
    described = [pilot.rivet_tools(f)[0]["description"] for f in (False, True)]
    check("the rivet tool names find only when offered, and is otherwise unchanged",
          "find" not in described[0] and "map, find, get, query" in described[1]
          and described[1].replace("find, ", "") == described[0])

    saved = pilot.USAGE_DOC
    try:
        pilot.USAGE_DOC = engine
        cond = trials.Rivet(pathlib.Path("/nonexistent"), trials.Log(), "rivet")
        check("read_docs returns the orientation main selected, not the whole guide",
              pilot.dispatch(cond, "read_docs", {}) == engine)
    finally:
        pilot.USAGE_DOC = saved
    ap = pilot.parser()
    check("the orientation defaults to the whole guide",
          ap.parse_args(["--output", "x"]).orientation == "full")


def test_usage_doc_measurement() -> None:
    """The guide is measured by difference, and nothing else was exercising
    that. Injecting `USAGE_DOC_TOKENS` and stubbing `usage_doc_tokens` both
    leave the measurement itself untested, so reverting it to counting the
    guide as a standalone message would have kept every check green."""
    print("guide measurement")
    real = pilot.count_tokens
    seen: list[list[dict]] = []
    try:
        def counted(key, messages, tools):
            seen.append(copy.deepcopy(messages))
            # A count that scales with content, plus fixed framing the
            # difference has to cancel.
            return 1_000 + sum(len(json.dumps(m)) for m in messages)

        pilot.count_tokens = counted
        got = pilot.usage_doc_tokens("k")
        check("two counts are taken, one carrying the guide and one not", len(seen) == 2)
        carried, empty = seen
        check("the two requests differ only in the guide's own content",
              [json.dumps(m) for m in carried][:-1] == [json.dumps(m) for m in empty][:-1]
              and carried[-1]["content"][0]["content"] == pilot.USAGE_DOC
              and empty[-1]["content"][0]["content"] == "")
        check("the guide arrives in a tool result, the shape a trial gives it",
              carried[-1]["content"][0]["type"] == "tool_result")
        expected = len(json.dumps(carried[-1])) - len(json.dumps(empty[-1]))
        check("the framing both requests share cancels out",
              got == expected, f"{got} against {expected}")
        check("the measurement tracks the guide's own size",
              got > 0 and abs(got - len(pilot.USAGE_DOC)) < len(pilot.USAGE_DOC) * 0.1,
              f"{got} for a {len(pilot.USAGE_DOC)}-char guide")

        pilot.count_tokens = lambda key, messages, tools: None
        check("a count that could not be taken is not guessed at",
              pilot.usage_doc_tokens("k") is None)

        answers = iter([1_234, None])
        pilot.count_tokens = lambda key, messages, tools: next(answers)
        check("half a measurement is no measurement",
              pilot.usage_doc_tokens("k") is None)
    finally:
        pilot.count_tokens = real


def test_attribution_covers_charged_calls() -> None:
    """Attribution and spend have to cover the same calls.

    A call whose response is lost is charged and has no round. Counting the
    guide from completed rounds leaves that call's spend in the total and its
    copies nowhere, and the difference lands in the remainder, which is the
    figure a write-up would quote as the engine's own cost."""
    print("attribution coverage")
    real = pilot.call_messages
    saved = pilot.USAGE_DOC_TOKENS
    tmp, root, _ = project()
    try:
        pilot.USAGE_DOC_TOKENS = 9_000
        calls = {"n": 0}

        def reads_then_dies(key, system, messages, tools, max_tokens):
            calls["n"] += 1
            if calls["n"] == 1:
                return ({"content": [{"type": "tool_use", "id": "d", "name": "read_docs",
                                      "input": {}}],
                         "stop_reason": "tool_use",
                         "usage": {"input_tokens": 900, "output_tokens": 20}}, 0.01)
            raise pilot.TransportError("ConnectionResetError", billable=True)

        pilot.call_messages = reads_then_dies
        budget = pilot.Budget(40, 10**6, 10**6)
        tally = pilot.Attribution()
        pilot.run_agent("k", budget, pilot.condition_for("rivet", root, binary()), "task", 64,
                        attribution=tally)
        check("a charged call whose response was lost is still attributed",
              tally.copies == [0, 1], str(tally.copies))
        check("the lost call's guide copy reaches the attribution",
              tally.totals() == (1, 9_000), str(tally.totals()))
        check("the trial is charged for two calls and attributed for two",
              budget.calls == tally.calls(), f"{budget.calls} charged, {tally.calls()} attributed")

        # A refunded attempt is not spend, so it is not attribution either.
        attempts = {"n": 0}

        def flaky(key, system, messages, tools, max_tokens):
            attempts["n"] += 1
            if attempts["n"] == 1:
                return ({"content": [{"type": "tool_use", "id": "d", "name": "read_docs",
                                      "input": {}}],
                         "stop_reason": "tool_use",
                         "usage": {"input_tokens": 900, "output_tokens": 20}}, 0.01)
            if attempts["n"] == 2:
                raise pilot.TransportError("HTTP 529: overloaded", billable=False)
            return ({"content": [{"type": "text", "text": "done"}], "stop_reason": "end_turn",
                     "usage": {"input_tokens": 1_000, "output_tokens": 5}}, 0.01)

        pilot.call_messages = flaky
        slept, pilot.time.sleep = pilot.time.sleep, lambda _: None
        try:
            budget = pilot.Budget(40, 10**6, 10**6)
            tally = pilot.Attribution()
            pilot.run_agent("k", budget, pilot.condition_for("rivet", root, binary()), "task", 64,
                            attribution=tally)
        finally:
            pilot.time.sleep = slept
        check("a refunded attempt is charged to nobody and attributed to nobody",
              tally.copies == [0, 1] and budget.calls == 2 and attempts["n"] == 3,
              f"{tally.copies}, {budget.calls} charged over {attempts['n']} attempts")

        # A baseline can ask for a tool it does not have. The error it gets
        # back is not a guide, and charging it for one reports a remainder
        # below zero.
        def asks_for_docs(key, system, messages, tools, max_tokens):
            turns = len([m for m in messages if m["role"] == "assistant"])
            if turns == 0:
                return ({"content": [{"type": "tool_use", "id": "d", "name": "read_docs",
                                      "input": {}}],
                         "stop_reason": "tool_use",
                         "usage": {"input_tokens": 100, "output_tokens": 10}}, 0.01)
            return ({"content": [{"type": "text", "text": "done"}], "stop_reason": "end_turn",
                     "usage": {"input_tokens": 150, "output_tokens": 5}}, 0.01)

        pilot.call_messages = asks_for_docs
        budget = pilot.Budget(40, 10**6, 10**6)
        tally = pilot.Attribution()
        trace = pilot.run_agent("k", budget, pilot.condition_for("naive", root, binary()), "task", 64,
                                attribution=tally)
        check("a baseline that asks for the guide is not charged for carrying it",
              tally.totals() == (0, 0) and trace.tool_calls[0]["output"].startswith("error"),
              f"{tally.totals()} from {tally.copies}")
    finally:
        pilot.call_messages = real
        pilot.USAGE_DOC_TOKENS = saved
        shutil.rmtree(tmp, ignore_errors=True)


def test_failed_trial_accounting() -> None:
    """A trial that raised keeps its spend, so it has to keep its attribution
    or say it lost it. Reporting zero copies alongside the tokens they carried
    puts the whole of a rivet trial into the remainder, which is the number a
    write-up quotes as the engine's own cost."""
    print("failed trials")
    saved = pilot.USAGE_DOC_TOKENS
    try:
        pilot.USAGE_DOC_TOKENS = 9_000

        def carried(copies):
            tally = pilot.Attribution()
            for c in copies:
                tally.charged(c)
            return tally

        # Both writers read the guide and then carried it: four charged calls.
        both = carried([0, 1, 0, 1])
        recovered = pilot.failed_run("same_node", "A then B", "rivet", "a writer did not finish",
                                     (21_800, 400, 0.05), both, charged_calls=4)
        check("a failed trial keeps the attribution its writers produced",
              (recovered.doc_calls, recovered.orientation_input_tokens) == (2, 18_000),
              f"{recovered.doc_calls} calls, {recovered.orientation_input_tokens} tokens")
        check("attribution covering every charged call is not marked unknown",
              recovered.attribution_known)

        # One writer raised before its calls reached the tally: the trial was
        # charged for four and two are accounted for, which is not the split.
        partial = pilot.failed_run("same_node", "A then B", "rivet", "a writer raised",
                                   (21_800, 400, 0.05), carried([0, 1]), charged_calls=4)
        check("attribution that covers only part of the spend is marked unknown",
              not partial.attribution_known, f"known over {partial.doc_calls} calls")

        lost = pilot.failed_run("same_node", "A then B", "rivet", "something else",
                                (21_800, 400, 0.05), charged_calls=4)
        check("a failure with no tally at all reports its attribution as unknown",
              not lost.attribution_known and lost.doc_calls == 0)

        # A failure that spent nothing has nothing to attribute, and saying so
        # is different from not knowing.
        nothing = pilot.failed_run("complete_one", "1 file", "naive", "raised before any call",
                                   (0, 0, 0.0), pilot.Attribution(), charged_calls=0)
        check("a failure before any call is charged has known, empty attribution",
              nothing.attribution_known and nothing.doc_calls == 0)

        provisional = pilot.failed_run("same_node", "A then B", "rivet", "a writer is still running",
                                       (21_800, 400, 0.05), both, charged_calls=4, provisional=True)
        check("a trial a live writer outlived says its accounting may still move",
              provisional.accounting_provisional)
        check("an ordinary failure does not claim provisional accounting",
              not recovered.accounting_provisional and not lost.accounting_provisional)

        # The summary must not present the unknown split as a measured one.
        table = pilot.summarise_conditions([lost])
        row = [ln.split() for ln in table.splitlines() if ln.strip().startswith("rivet")][0]
        check("a condition whose attribution was lost reports no split",
              row[3:5] == ["?", "?"], " ".join(row))
        known = pilot.summarise_conditions([recovered])
        row = [ln.split() for ln in known.splitlines() if ln.strip().startswith("rivet")][0]
        check("a condition whose attribution survived reports the split it recovered",
              row[3:5] == ["18000", "3800"], " ".join(row))
    finally:
        pilot.USAGE_DOC_TOKENS = saved


def test_attribution_reads_are_coherent() -> None:
    """Coverage and totals have to come from one reading.

    Read separately, a writer appending between them yields a coverage count
    that certifies a total it was not part of: the result says the attribution
    covered every charged call while publishing the figure from before the
    last one landed."""
    print("attribution coherence")
    saved = pilot.USAGE_DOC_TOKENS
    try:
        pilot.USAGE_DOC_TOKENS = 9_000
        tally = pilot.Attribution()

        def append():
            for _ in range(400):
                tally.charged(1)
                time.sleep(0)  # yield, so reads land mid-sequence

        writer = threading.Thread(target=append)
        writer.start()
        try:
            # Every copy is 1, so a coherent reading has carried == covered
            # and tokens == carried * 9,000. Two separate reads drift apart.
            drift = []
            while writer.is_alive():
                covered, carried, tokens = tally.freeze()
                if not (covered == carried and tokens == carried * 9_000):
                    drift.append((covered, carried, tokens))
        finally:
            writer.join()
        check("coverage and totals never describe different moments",
              not drift, f"{len(drift)} incoherent readings, first {drift[:1]}")

        # Nothing carried is zero tokens, whatever the measurement did.
        empty = pilot.Attribution()
        for _ in range(2):
            empty.charged(0)
        pilot.USAGE_DOC_TOKENS = None
        check("a trial that carried nothing reports zero, not unknown",
              empty.freeze() == (2, 0, 0), str(empty.freeze()))
        check("a trial that carried something unmeasured reports unknown",
              carried_one().freeze()[2] is None, str(carried_one().freeze()))
    finally:
        pilot.USAGE_DOC_TOKENS = saved


def carried_one() -> "pilot.Attribution":
    tally = pilot.Attribution()
    tally.charged(1)
    return tally


def test_trial_spend_and_coverage_agree() -> None:
    """The check that a trial's attribution covered its spend has to read both
    from one moment.

    A surviving writer refunding between the two readings leaves the charged
    count one lower than the coverage was measured against, and the result
    then certifies a completeness it does not have. Testing `freeze()` alone
    does not catch that: the split reads live in the caller."""
    print("trial coverage")
    tmp = pathlib.Path(tempfile.mkdtemp())
    real_run, real_measure = pilot.run_one, pilot.usage_doc_tokens
    real_snapshot = pilot.Budget.snapshot
    try:
        pilot.usage_doc_tokens = lambda key: 9_000
        state = {"budget": None, "refunded": False}

        def charges_two(kind, kw, key, budget):
            state["budget"] = budget
            held = budget.reserve(1_000, 100)
            budget.settle(budget.reserve(9_000, 100), {"input_tokens": 9_000, "output_tokens": 20})
            # Only the second call reached the tally: the writer that made the
            # first one is still running.
            tally = pilot.Attribution()
            tally.charged(1)
            state["held"] = held
            raise pilot.TrialFailed("a writer is still running", tally, provisional=True)

        def refunding(self):
            # A surviving writer refunds the moment the ledger is read, which
            # is the interleaving that made the old two-read check certify a
            # coverage it never had.
            ledger = real_snapshot(self)
            if not state["refunded"] and state.get("held") is not None:
                state["refunded"] = True
                self.refund(state["held"])
            return ledger

        pilot.run_one = charges_two
        pilot.Budget.snapshot = refunding
        out = tmp / "coverage.json"
        sys.argv = ["pilot", "--output", str(out), "--max-runs", "1"]
        os.environ["RIVET_PILOT_API_KEY"] = "dummy"
        with contextlib.redirect_stdout(io.StringIO()):
            pilot.main()
        pilot.Budget.snapshot = real_snapshot
        row = json.loads(out.read_text())["results"][0]
        check("a refund between readings cannot certify coverage the trial lacked",
              row["attribution_known"] is False,
              f"known={row['attribution_known']} with {row['doc_calls']} attributed calls")
    finally:
        pilot.Budget.snapshot = real_snapshot
        pilot.run_one, pilot.usage_doc_tokens = real_run, real_measure
        shutil.rmtree(tmp, ignore_errors=True)


def test_publication_is_one_snapshot() -> None:
    """The results file and the summary must describe the same moment.

    A writer that outlived its trial can settle between two readings, and a
    JSON file and a console table taken either side of that describe two
    different runs while looking like one."""
    print("publication coherence")
    tmp = pathlib.Path(tempfile.mkdtemp())
    real_run, real_measure = pilot.run_one, pilot.usage_doc_tokens
    real_snapshot = pilot.Budget.snapshot
    try:
        pilot.usage_doc_tokens = lambda key: 9_000

        def one_call(kind, kw, key, budget):
            budget.settle(budget.reserve(1_000, 100), {"input_tokens": 900, "output_tokens": 20})
            tally = pilot.Attribution()
            tally.charged(0)
            raise pilot.TrialFailed("stopped here", tally)

        # Every reading after the first reports a ledger that has moved on.
        readings = {"n": 0}

        def moving(self):
            readings["n"] += 1
            ledger = real_snapshot(self)
            ledger["input_tokens"] += 1_000 * readings["n"]
            return ledger

        pilot.run_one = one_call
        pilot.Budget.snapshot = moving
        out = tmp / "coherent.json"
        sys.argv = ["pilot", "--output", str(out), "--max-runs", "1"]
        os.environ["RIVET_PILOT_API_KEY"] = "dummy"
        printed = io.StringIO()
        with contextlib.redirect_stdout(printed):
            pilot.main()
        pilot.Budget.snapshot = real_snapshot
        meta = json.loads(out.read_text())["meta"]
        line = [ln for ln in printed.getvalue().splitlines() if "API calls" in ln][0]
        check("the console reports the input total the results file was written from",
              f"{meta['input_tokens']} input" in line, f"meta {meta['input_tokens']}, console {line!r}")
    finally:
        pilot.Budget.snapshot = real_snapshot
        pilot.run_one, pilot.usage_doc_tokens = real_run, real_measure
        shutil.rmtree(tmp, ignore_errors=True)


def test_failure_reaches_publication() -> None:
    """The flags only matter if they survive into what gets read.

    A result can carry `attribution_known: false` and still be published
    beside a console table that prints a confident split, or a meta block
    whose totals were read while a writer was still settling."""
    print("failure publication")
    tmp = pathlib.Path(tempfile.mkdtemp())
    real_run, real_key = pilot.run_one, pilot.usage_doc_tokens
    saved = pilot.USAGE_DOC_TOKENS
    try:
        pilot.usage_doc_tokens = lambda key: 9_000

        def raises(kind, kw, key, budget):
            # Spend first, so the trial has tokens with no attribution behind
            # them, then fail with only one writer's tally recovered.
            budget.settle(budget.reserve(1_000, 100), {"input_tokens": 900, "output_tokens": 20})
            budget.settle(budget.reserve(1_000, 100), {"input_tokens": 900, "output_tokens": 20})
            tally = pilot.Attribution()
            tally.charged(1)
            raise pilot.TrialFailed("a writer did not finish", tally, provisional=True)

        pilot.run_one = raises
        out = tmp / "failed.json"
        sys.argv = ["pilot", "--output", str(out), "--max-runs", "1"]
        os.environ["RIVET_PILOT_API_KEY"] = "dummy"
        pilot.main()
        data = json.loads(out.read_text())
        row = data["results"][0]
        check("a partly attributed failure is published as unknown, not as zero",
              row["attribution_known"] is False and row["input_tokens"] > 0,
              f"known={row['attribution_known']} over {row['input_tokens']} tokens")
        check("a trial a live writer outlived publishes that its accounting may move",
              row["accounting_provisional"] is True)
        check("the meta reports what was still in flight", "calls_in_flight" in data["meta"])
        table = pilot.summarise_conditions([pilot.RunResult(**{
            k: v for k, v in row.items()})])
        check("the console table refuses to print a split for that trial",
              " ? " in table or "?" in table.split("\n")[3], table.split("\n")[3])
        check("the console table marks the row as still moving",
              "~" in table.split("\n")[3], table.split("\n")[3])
    finally:
        pilot.run_one, pilot.usage_doc_tokens = real_run, real_key
        pilot.USAGE_DOC_TOKENS = saved
        shutil.rmtree(tmp, ignore_errors=True)


def test_orientation_attribution() -> None:
    """A rivet run reads a usage guide no baseline is given, and then carries
    it in every later round. Reporting one input figure for that against a
    baseline's compares the guide, so the guide's share is measured and
    reported apart from the work."""
    print("orientation")
    saved = pilot.USAGE_DOC_TOKENS
    try:
        pilot.USAGE_DOC_TOKENS = 9_000

        def carried(copies: list[int]) -> pilot.Attribution:
            tally = pilot.Attribution()
            for c in copies:
                tally.charged(c)
            return tally

        check("every charged call after read_docs carries the guide",
              carried([0, 1, 1, 1, 1]).totals() == (4, 36_000))
        check("a guide read on the second call is not charged to the first",
              carried([0, 0, 1]).totals() == (1, 9_000))
        check("a second read of the guide is carried as a second copy",
              carried([0, 1, 2]).totals() == (2, 27_000))
        check("a trial that never read the guide is charged nothing for it",
              carried([0, 0]).totals() == (0, 0))
        t = carried([0, 1, 1, 1, 1])

        pilot.USAGE_DOC_TOKENS = None
        check("an unmeasured guide reports no token figure rather than a zero one",
              t.totals()[1] is None, str(t.totals()))
        check("an unmeasured guide does not erase the call count already collected",
              t.totals()[0] == 4, str(t.totals()))

        pilot.USAGE_DOC_TOKENS = 9_000
        rows = [pilot.RunResult("complete_one", "1 file", "rivet", True, 0, 0, 0, 5, 4, 5,
                                47_000, 500, 0, 0, 1.0, 2.0, None, "", 0.09, 4, 36_000),
                pilot.RunResult("complete_one", "1 file", "naive", True, 0, 0, 0, 3, 2, 3,
                                7_500, 400, 0, 0, 1.0, 2.0, None, "", 0.02, 0, 0)]
        pilot.USAGE_DOC_TOKENS = None
        rows = [pilot.RunResult("complete_one", "1 file", "rivet", True, 0, 0, 0, 5, 4, 5,
                                47_000, 500, 0, 0, 1.0, 2.0, None, "", 0.09, 4, None),
                pilot.RunResult("complete_one", "1 file", "naive", True, 0, 0, 0, 3, 2, 3,
                                7_500, 400, 0, 0, 1.0, 2.0, None, "", 0.02, 0, 0)]
        unmeasured = pilot.summarise_conditions(rows)
        rivet_row = [ln.split() for ln in unmeasured.splitlines() if ln.strip().startswith("rivet")][0]
        naive_row = [ln.split() for ln in unmeasured.splitlines() if ln.strip().startswith("naive")][0]
        check("an unmeasured guide makes the remainder unknown, not the total",
              rivet_row[3:5] == ["?", "?"], " ".join(rivet_row))
        check("a condition that carried no guide still reports its own task tokens",
              naive_row[3:5] == ["0", "7500"], " ".join(naive_row))

        pilot.USAGE_DOC_TOKENS = 9_000
        rows = [pilot.RunResult("complete_one", "1 file", "rivet", True, 0, 0, 0, 5, 4, 5,
                                47_000, 500, 0, 0, 1.0, 2.0, None, "", 0.09, 4, 36_000),
                pilot.RunResult("complete_one", "1 file", "naive", True, 0, 0, 0, 3, 2, 3,
                                7_500, 400, 0, 0, 1.0, 2.0, None, "", 0.02, 0, 0)]
        table = pilot.summarise_conditions(rows)
        rivet = [ln.split() for ln in table.splitlines() if ln.strip().startswith("rivet")][0]
        naive = [ln.split() for ln in table.splitlines() if ln.strip().startswith("naive")][0]
        check("the table reports the guide's share and the task's separately",
              rivet[2:5] == ["47000", "36000", "11000"], " ".join(rivet))
        check("a baseline is never charged for a guide it was never given",
              naive[3] == "0" and naive[4] == "7500", " ".join(naive))
        idle = pilot.Budget(40, 10**6, 10**6)
        check("a run that made no calls claims nothing about caching",
              "nothing about caching" in pilot.cache_note(idle.snapshot()), pilot.cache_note(idle.snapshot()))
        uncached = pilot.Budget(40, 10**6, 10**6)
        uncached.settle(uncached.reserve(10, 10), {"input_tokens": 5, "output_tokens": 1})
        check("a run that cached nothing is not reported as paying a write premium",
              "ordinary input rate" in pilot.cache_note(uncached.snapshot()))
        written = pilot.Budget(40, 10**6, 10**6)
        written.settle(written.reserve(10, 10),
                       {"input_tokens": 5, "cache_creation_input_tokens": 2_000, "output_tokens": 1})
        check("a run that wrote entries nobody read is reported as the premium it is",
              "1.25x" in pilot.cache_note(written.snapshot()))
        read = pilot.Budget(40, 10**6, 10**6)
        read.settle(read.reserve(10, 10),
                    {"input_tokens": 5, "cache_read_input_tokens": 100, "output_tokens": 1})
        check("a run with reads says the breakpoints are landing",
              "landing" in pilot.cache_note(read.snapshot()))

        # An outstanding call has reported nothing, so nothing is known about
        # what it cached; concluding "billed at the ordinary rate" from the
        # calls that did settle claims it for them too.
        outstanding = pilot.Budget(40, 10**6, 10**6)
        outstanding.settle(outstanding.reserve(10, 10), {"input_tokens": 5, "output_tokens": 1})
        outstanding.reserve(10, 10)
        note = pilot.cache_note(outstanding.snapshot())
        check("an outstanding call is not reported as having cached nothing",
              "unknown rather than nothing" in note, note)

        # Every branch describes settled usage, so every branch has to say
        # what it does not cover. Removing the qualification from the read and
        # write branches has to fail here, or round 10's fix is unprotected.
        with_reads = pilot.Budget(40, 10**6, 10**6)
        with_reads.settle(with_reads.reserve(10, 10),
                          {"input_tokens": 5, "cache_read_input_tokens": 100, "output_tokens": 1})
        with_reads.reserve(10, 10)
        note = pilot.cache_note(with_reads.snapshot())
        check("reads reported alongside an outstanding call still say what is not covered",
              "nothing here covers them" in note, note)

        with_writes = pilot.Budget(40, 10**6, 10**6)
        with_writes.settle(with_writes.reserve(10, 10),
                           {"input_tokens": 5, "cache_creation_input_tokens": 2_000, "output_tokens": 1})
        with_writes.reserve(10, 10)
        note = pilot.cache_note(with_writes.snapshot())
        check("writes reported alongside an outstanding call still say what is not covered",
              "nothing here covers them" in note, note)

        # One reading, taken under the lock, is what gets published.
        live = pilot.Budget(40, 10**6, 10**6)
        held = live.reserve(1_000, 100)
        mid = live.snapshot()
        live.settle(held, {"input_tokens": 5, "output_tokens": 1})
        check("a snapshot reports a call still in flight",
              mid["in_flight"] == 1 and live.snapshot()["in_flight"] == 0,
              f"{mid['in_flight']} then {live.snapshot()['in_flight']}")
        check("a snapshot does not move when the ledger does",
              mid["input_tokens"] == 1_000, str(mid["input_tokens"]))
    finally:
        pilot.USAGE_DOC_TOKENS = saved


def psi(io_some_us: int, mem_some_us: int = 0) -> dict:
    return {"io": {"some": io_some_us, "full": io_some_us // 2},
            "memory": {"some": mem_some_us, "full": 0}}


def test_env_stall() -> None:
    print("host stall classification")
    over = int((pilot.ENV_STALL_IO_S + 5) * 1e6)
    under = int((pilot.ENV_STALL_IO_S / 2) * 1e6)
    c = pilot.classify_env(psi(1_000), psi(1_000 + over), [])
    check("an IO pressure rise past the threshold is an env_stall",
          c["env_stall"] is True and c["io_stall_s"] == (over / 1e6), str(c))
    c = pilot.classify_env(psi(1_000), psi(1_000 + under), [])
    check("an IO pressure rise under the threshold is measured and not a stall",
          c["env_stall"] is False and c["env_stall_reason"] is None, str(c))
    unread = {"io": None, "memory": None}
    c = pilot.classify_env(unread, unread, [])
    check("unreadable PSI is unmeasured, not stalled and not calm",
          c["env_stall"] is None and c["io_stall_s"] is None, str(c))
    c = pilot.classify_env(psi(5), psi(1), [])
    check("a counter that went backwards is unmeasured", c["env_stall"] is None, str(c))
    d_state = [call("rivet", 1, 61) | {"processes": [
        {"command": "update-item", "timed_out": True, "state": "D", "wchan": "folio_wait_bit"}]}]
    c = pilot.classify_env(unread, unread, d_state)
    check("a tool timeout in state D is an env_stall even with PSI unmeasured",
          c["env_stall"] is True and "state D" in (c["env_stall_reason"] or ""), str(c))
    s_state = [call("rivet", 1, 61) | {"processes": [
        {"command": "update-item", "timed_out": True, "state": "S", "wchan": "do_epoll_wait"}]}]
    c = pilot.classify_env(psi(0), psi(0), s_state)
    check("a timeout in interruptible sleep is the tool's own, not the host's",
          c["env_stall"] is False, str(c))

    missing = trials.psi_snapshot({"io": "/nonexistent/pressure/io", "memory": "/nonexistent/pressure/memory"})
    check("missing PSI files read as unmeasured without raising",
          missing == {"io": None, "memory": None}, str(missing))
    if pathlib.Path("/proc/pressure/io").exists():
        live = trials.psi_snapshot()
        check("the live PSI files parse to some/full totals",
              isinstance(live["io"], dict) and isinstance(live["io"]["some"], int), str(live))

    # End to end through a graded trial: the flag joins the result and the
    # grader's verdict stands, for a stall and for an unmeasured host alike.
    real, saved_psi = pilot.call_messages, trials.psi_snapshot

    def scripted(key, system, messages, tools, max_tokens):
        turns = len([m for m in messages if m["role"] == "assistant"])
        if turns == 0:
            return ({"content": [{"type": "tool_use", "id": "r", "name": "read_file",
                                  "input": {"path": pilot.REL0}}],
                     "stop_reason": "tool_use", "usage": {"input_tokens": 10, "output_tokens": 5}}, 0.01)
        if turns == 1:
            text = messages[-1]["content"][0]["content"]
            line = trials.item_line(text, "Action 1 of tracker 0.")
            return ({"content": [{"type": "tool_use", "id": "w", "name": "write_file",
                                  "input": {"path": pilot.REL0,
                                            "content": text.replace(line, line.replace("- [ ]", "- [x]", 1), 1)}}],
                     "stop_reason": "tool_use", "usage": {"input_tokens": 20, "output_tokens": 40}}, 0.01)
        return ({"content": [{"type": "text", "text": "done"}], "stop_reason": "end_turn",
                 "usage": {"input_tokens": 5, "output_tokens": 3}}, 0.01)

    def trial(readings):
        it = iter(readings)
        trials.psi_snapshot = lambda files=None: next(it)
        return pilot.single_writer("complete_one", "1 file", "k", pilot.Budget(40, 10**6, 10**6),
                                   "naive", binary(), pilot.prompt_complete, pilot.grade_complete)

    try:
        pilot.call_messages = scripted
        calm, _ = trial([psi(0), psi(under)])
        stalled, tr = trial([psi(0), psi(over)])
        blind, _ = trial([unread, unread])
        check("a stalled trial is flagged env_stall", stalled.env_stall is True, str(stalled.env_stall_reason))
        check("a stalled trial is graded exactly as a calm one",
              (stalled.correct, stalled.damage_bytes, stalled.lost_updates)
              == (calm.correct, calm.damage_bytes, calm.lost_updates) and stalled.correct,
              f"{stalled.correct} {stalled.notes}")
        check("the trial's PSI snapshots reach its trace",
              tr["psi"] == {"start": psi(0), "end": psi(over)}, str(tr.get("psi")))
        check("a calm trial is measured and not flagged", calm.env_stall is False)
        check("a trial with unreadable PSI is unmeasured, not stalled",
              blind.env_stall is None and blind.correct, str(blind.env_stall))
        summary = pilot.summarise_env([calm, stalled, blind])
        row = next(l for l in summary.splitlines() if l.strip().startswith("naive"))
        check("the summary counts the stall apart and leaves correct untouched",
              row.split()[1:] == ["3", "3", "0", "1", "0", "1"], row)
    finally:
        pilot.call_messages = real
        trials.psi_snapshot = saved_psi


def test_tool_timeout_probe() -> None:
    print("tool process probe")
    probe: dict = {}
    t0 = time.monotonic()
    try:
        trials.run_probed(["sleep", "30"], "", None, probe, timeout=0.3)
        raised = None
    except subprocess.TimeoutExpired as e:
        raised = e
    check("a timeout still raises subprocess.TimeoutExpired", raised is not None and raised.timeout == 0.3)
    check("the timed-out child is killed, not waited out", time.monotonic() - t0 < 10)
    check("the probe records the state and wait channel before the kill",
          probe.get("timed_out") is True and probe.get("state") in ("S", "D", "R")
          and "wchan" in probe, str(probe))
    check("the probe records how long the kill took to reap",
          isinstance(probe.get("reap_s"), float) and 0 <= probe["reap_s"] < 10
          and probe["duration_s"] >= 0.3, str(probe))

    probe = {}
    out, code = trials.run_probed(["cat"], '{"x": 1}', None, probe)
    check("a call that finishes returns its output and exit code unchanged",
          (out, code) == ('{"x": 1}', 0), repr(out))
    check("a finished call records duration and the child's own rusage",
          probe["timed_out"] is False and probe["exit_code"] == 0 and probe["duration_s"] >= 0
          and isinstance(probe["rusage"], dict) and probe["rusage"]["maxrss_kb"] > 0, str(probe))

    # Through the rivet condition and the dispatcher the model sees: the
    # message is the timeout it always was, and the probe joins the trace.
    tmp = pathlib.Path(tempfile.mkdtemp())
    saved = trials.TOOL_TIMEOUT_S
    try:
        slow = tmp / "slow-rivet"
        slow.write_text("#!/bin/sh\nexec sleep 30\n")
        slow.chmod(0o755)
        trials.TOOL_TIMEOUT_S = 0.3
        cond = pilot.condition_for("rivet", tmp, str(slow))
        out = pilot.dispatch(cond, "rivet", {"command": "map", "request": {}})
        check("a rivet timeout reaches the model as the same TimeoutExpired error",
              out.startswith("error: TimeoutExpired:") and "timed out after 0.3 seconds" in out, out)
        check("the rivet condition keeps the timed-out call's probe",
              len(cond.probes) == 1 and cond.probes[0]["command"] == "map"
              and cond.probes[0]["timed_out"] and "reap_s" in cond.probes[0], str(cond.probes))

        real_cond = pilot.condition_for("rivet", tmp, binary())

        def one_rivet_call(key, system, messages, tools, max_tokens):
            if len([m for m in messages if m["role"] == "assistant"]) == 0:
                return ({"content": [{"type": "tool_use", "id": "m", "name": "rivet",
                                      "input": {"command": "map", "request": {"path": "x.md"}}}],
                         "stop_reason": "tool_use", "usage": {"input_tokens": 1, "output_tokens": 1}}, 0.01)
            return ({"content": [{"type": "text", "text": "done"}], "stop_reason": "end_turn",
                     "usage": {"input_tokens": 1, "output_tokens": 1}}, 0.01)

        trials.TOOL_TIMEOUT_S = saved
        real = pilot.call_messages
        pilot.call_messages = one_rivet_call
        try:
            t = pilot.run_agent("k", pilot.Budget(40, 10**6, 10**6), real_cond, "hello", 1536)
        finally:
            pilot.call_messages = real
        procs = t.tool_calls[0].get("processes") if t.tool_calls else None
        check("a rivet tool call in the trace carries its process record",
              procs is not None and len(procs) == 1 and procs[0]["command"] == "map"
              and procs[0]["timed_out"] is False and "rusage" in procs[0], str(procs))
    finally:
        trials.TOOL_TIMEOUT_S = saved
        shutil.rmtree(tmp, ignore_errors=True)


def rivet_script(write_for: dict | None = None, raise_on: str | None = None):
    """A scripted model for the rivet condition: one `update-item` with an
    unusable request (exit 1, invalid rather than refused, so the grader
    counts the missing edit as lost), then a final reply. `raise_on` names the
    writer ("A" or "B", or "any") whose second call raises instead."""
    def scripted(key, system, messages, tools, max_tokens):
        turns = len([m for m in messages if m["role"] == "assistant"])
        prompt = "".join(b.get("text", "") for b in messages[0]["content"])
        who = "B" if "B was here." in prompt else "A"
        if turns == 0:
            return ({"content": [{"type": "tool_use", "id": "u", "name": "rivet",
                                  "input": {"command": "update-item", "request": {}}}],
                     "stop_reason": "tool_use", "usage": {"input_tokens": 11, "output_tokens": 7}}, 0.01)
        if raise_on in (who, "any"):
            raise RuntimeError(f"scripted failure in writer {who}")
        return ({"content": [{"type": "text", "text": "done"}], "stop_reason": "end_turn",
                 "usage": {"input_tokens": 5, "output_tokens": 3}}, 0.01)
    return scripted


def test_env_stall_failures_and_writers() -> None:
    print("host stall on failed and concurrent trials")
    over = int((pilot.ENV_STALL_IO_S + 5) * 1e6)
    real, saved_psi, saved_state = pilot.call_messages, trials.psi_snapshot, trials.proc_state
    saved_probe = trials.run_probed
    tmp = pathlib.Path(tempfile.mkdtemp())

    def readings(*snaps):
        it = iter(snaps)
        trials.psi_snapshot = lambda files=None: next(it)

    def disjoint(budget, binary_path):
        return pilot.two_writers(
            "disjoint_nodes", "A then B, concurrent", "k", budget, "rivet", binary_path,
            lambda rels: pilot.prompt_gate(pilot.REL0, 1, "A was here."),
            lambda rels: pilot.prompt_gate(pilot.REL0, 2, "B was here."),
            pilot.grade_two(False))

    try:
        # Concurrent rivet trial: both writers' probes reach the classifier,
        # and the flag moves nothing in the verdict or the spend.
        pilot.call_messages = rivet_script()
        seen: list[list[dict]] = []
        classify = pilot.classify_env

        def watched(start, end, tool_calls):
            seen.append(tool_calls)
            return classify(start, end, tool_calls)

        pilot.classify_env = watched
        try:
            readings(psi(0), psi(0))
            calm, _ = disjoint(pilot.Budget(40, 10**6, 10**6), binary())
            readings(psi(0), psi(over))
            stalled, rec = disjoint(pilot.Budget(40, 10**6, 10**6), binary())
        finally:
            pilot.classify_env = classify
        b_probes = [p for c in rec["b"]["tool_calls"] for p in c.get("processes", [])]
        classified = [p for c in seen[-1] for p in c.get("processes", [])]
        check("writer B's rivet probes are in its trace", len(b_probes) == 1
              and b_probes[0]["command"] == "update-item", str(b_probes))
        check("writer B's probes are among those the classifier read",
              all(any(p is q for q in classified) for p in b_probes) and len(classified) == 2,
              f"{len(classified)} probes classified")
        check("a concurrent rivet trial with an IO pressure rise is flagged",
              stalled.env_stall is True and calm.env_stall is False,
              f"{stalled.env_stall} / {calm.env_stall}")
        same = ("correct", "damage_bytes", "lost_updates", "conflicts_detected",
                "input_tokens", "output_tokens", "cost_usd", "api_calls", "tool_calls")
        check("the flag changes neither lost updates nor spend",
              stalled.lost_updates == 2 and stalled.cost_usd > 0
              and all(getattr(stalled, f) == getattr(calm, f) for f in same),
              f"lost={stalled.lost_updates} cost={stalled.cost_usd} "
              f"diff={[f for f in same if getattr(stalled, f) != getattr(calm, f)]}")

        # Failed trials: a slow rivet times out in state D (proc_state is
        # stubbed: a real D state cannot be produced on demand), then the
        # model call raises. The failure must carry the stall out.
        # Setup deliberately takes longer than the synthetic write deadline.
        # Only update-item gets that deadline; map/stamp keep their normal budget.
        slow = tmp / "slow-rivet"
        slow.write_text(f'#!/bin/sh\ncase "$3" in stamp) sleep 0.6;; update-item) exec sleep 30;; esac\nexec "{binary()}" "$@"\n')
        slow.chmod(0o755)
        setup_probes: list[dict] = []

        def slow_write_probe(argv, sent, env, probe, timeout=None):
            if probe["command"] == "update-item":
                timeout = 0.3
            result = saved_probe(argv, sent, env, probe, timeout=timeout)
            if probe["command"] == "stamp":
                setup_probes.append(probe)
            return result

        trials.run_probed = slow_write_probe
        trials.proc_state = lambda pid: ("D", "folio_wait_bit")

        pilot.call_messages = rivet_script(raise_on="any")
        readings(psi(0), psi(over))
        try:
            pilot.single_writer("complete_one", "1 file", "k", pilot.Budget(40, 10**6, 10**6),
                                "rivet", str(slow), pilot.prompt_complete, pilot.grade_complete)
            failed = None
        except pilot.TrialFailed as e:
            failed = e
        check("a single-writer trial that raised carries its stall out",
              failed is not None and failed.env and failed.env["env_stall"] is True
              and "state D" in failed.env["env_stall_reason"]
              and failed.evidence["psi"]["end"] == psi(over)
              and failed.evidence["processes"][0]["timed_out"],
              str(failed and failed.env))

        pilot.call_messages = rivet_script(raise_on="B")
        readings(psi(0), psi(0))
        try:
            disjoint(pilot.Budget(40, 10**6, 10**6), str(slow))
            failed2 = None
        except pilot.TrialFailed as e:
            failed2 = e
        procs = (failed2.evidence or {}).get("processes", []) if failed2 else []
        check("a concurrent trial that raised classifies both writers' probes",
              failed2 is not None and len(procs) == 2 and failed2.env["env_stall"] is True,
              f"{len(procs)} probes, env={failed2 and failed2.env}")
        check("delayed setup finishes outside the synthetic write timeout for both trial shapes",
              len(setup_probes) == 2 and all(p["duration_s"] >= 0.6 and not p["timed_out"]
                                                 for p in setup_probes), str(setup_probes))
        check("only the slow writes time out and retain their blocked-state classification",
              len(procs) == 2 and all(p["command"] == "update-item" and p["timed_out"]
                                     and p["state"] == "D" for p in procs), str(procs))

        # Through main: the failed row is flagged, with the same verdict,
        # spend and coverage as before the flag existed.
        real_run, real_key = pilot.run_one, pilot.usage_doc_tokens
        pilot.usage_doc_tokens = lambda key: 9_000

        def raises(kind, kw, key, budget):
            budget.settle(budget.reserve(1_000, 100), {"input_tokens": 900, "output_tokens": 20})
            raise pilot.TrialFailed("a writer did not finish", pilot.Attribution(), provisional=True,
                                    env=failed.env, evidence=failed.evidence)

        def bare(kind, kw, key, budget):
            budget.settle(budget.reserve(1_000, 100), {"input_tokens": 900, "output_tokens": 20})
            raise pilot.TrialFailed("a writer did not finish", pilot.Attribution(), provisional=True)

        rows = {}
        try:
            for label, fn in (("flagged", raises), ("bare", bare)):
                pilot.run_one = fn
                out = tmp / f"{label}.json"
                sys.argv = ["pilot", "--output", str(out), "--max-runs", "1"]
                os.environ["RIVET_PILOT_API_KEY"] = "dummy"
                with contextlib.redirect_stdout(io.StringIO()):
                    pilot.main()
                rows[label] = (json.loads(out.read_text())["results"][0],
                               json.loads((tmp / f"{label}-traces.json").read_text())[0])
        finally:
            pilot.run_one, pilot.usage_doc_tokens = real_run, real_key
        flagged, flagged_trace = rows["flagged"]
        plain, _ = rows["bare"]
        check("a failed trial is published as env_stall", flagged["env_stall"] is True,
              str(flagged["env_stall_reason"]))
        check("a failed trial's trace keeps the PSI snapshots and probes",
              flagged_trace.get("psi", {}).get("end") == psi(over) and flagged_trace.get("processes"))
        moved = [k for k in plain if k not in ("env_stall", "env_stall_reason", "io_stall_s",
                                               "memory_stall_s") and plain[k] != flagged[k]]
        check("the flag leaves the failed row's verdict, spend and coverage as they were",
              not moved and flagged["correct"] is False and flagged["lost_updates"] == 0
              and flagged["accounting_provisional"] is True and plain["env_stall"] is None,
              f"moved={moved}")
    finally:
        pilot.call_messages = real
        trials.psi_snapshot, trials.proc_state = saved_psi, saved_state
        trials.run_probed = saved_probe
        shutil.rmtree(tmp, ignore_errors=True)


def test_probe_child_exits_before_kill() -> None:
    print("tool process probe, child exits before the kill")
    real_state = trials.proc_state

    def slow_state(pid):
        # Long enough for the child to exit between the timeout and the kill.
        time.sleep(0.8)
        return real_state(pid)

    probe: dict = {}
    trials.proc_state = slow_state
    try:
        try:
            trials.run_probed(["sleep", "0.4"], "", None, probe, timeout=0.1)
            raised = False
        except subprocess.TimeoutExpired:
            raised = True
    finally:
        trials.proc_state = real_state
    check("the timeout still raises when the child exits before the kill", raised)
    check("the race happened: the child had exited before the kill",
          probe.get("exited_before_kill") is True, str(probe))
    check("the child that exited before the kill keeps its rusage and reap time",
          isinstance(probe.get("rusage"), dict) and isinstance(probe.get("reap_s"), float), str(probe))



def test_failure_evidence_is_a_snapshot() -> None:
    print("failed-trial evidence is a snapshot")
    # A writer that outlives the join deadline can still be filling its last
    # probe; what was published must keep reclassifying to the published flag.
    saved = trials.psi_snapshot
    trials.psi_snapshot = lambda files=None: None
    try:
        live = {"timed_out": False, "duration_s": 0.01}
        cond = types.SimpleNamespace(probes=[live])
        env, evidence = pilot.failure_env(None, [cond])
        live.update(timed_out=True, state="D", wchan="io_schedule")
        cond.probes.append({"timed_out": True, "state": "D"})
        again = pilot.classify_env(None, None, [{"processes": evidence["processes"]}])
        json.dumps(evidence)
    finally:
        trials.psi_snapshot = saved
    check("a probe updated after the failure does not reach its evidence",
          evidence["processes"] == [{"timed_out": False, "duration_s": 0.01}], str(evidence))
    check("the published evidence reclassifies to the published flag",
          again["env_stall"] == env["env_stall"], f"{env} {again}")


def main() -> int:
    forbid_network()
    for fn in (test_budget, test_settlement_guards, test_input_reservation_is_a_bound, test_grading_failure_keeps_trace, test_approval_ceilings, test_schedule, test_output_allowance, test_path_confinement,
               test_key_handling, test_untouched, test_grade_warm, test_grade_two_same_node,
               test_grade_two_disjoint, test_edited_file_byte_damage, test_report_grading, test_run_accounting,
               test_concurrent_path_runs, test_output_preflight, test_cache_breakpoints,
               test_cost_accounting, test_orientation_subset, test_usage_doc_measurement, test_attribution_covers_charged_calls,
               test_orientation_attribution, test_failed_trial_accounting,
               test_failure_reaches_publication, test_attribution_reads_are_coherent,
               test_publication_is_one_snapshot, test_trial_spend_and_coverage_agree,
               test_env_stall, test_tool_timeout_probe, test_env_stall_failures_and_writers,
               test_probe_child_exits_before_kill, test_failure_evidence_is_a_snapshot):
        fn()
    print()
    if FAILURES:
        print(f"{len(FAILURES)} failed:")
        for name in FAILURES:
            print(f"  - {name}")
        return 1
    print("all pilot harness checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())

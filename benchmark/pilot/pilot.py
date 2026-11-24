#!/usr/bin/env python3
"""Bounded pilot: a real model drives three tool conditions, no scripted agent.

First approved 2026-09-12 for claude-sonnet-5 at 40 runs; the ceilings it
runs under now are the APPROVED_* constants below, which are the record of the
current approval. `--safety N` runs only the two-writer disjoint_nodes trial,
N per condition, for the silent-loss bound. This makes real,
billed Anthropic API calls. It reuses the corpus, oracle and tool
implementations from `benchmark/trials/trials.py`; the only difference is
that the model, not a script, decides which tool calls to make and in what
order.

    RIVET=target/debug/markrivet \\
        python3 benchmark/pilot/pilot.py --output /tmp/pilot.json

Reads the API key from `RIVET_PILOT_API_KEY` and nowhere else. The name is
deliberately not
ANTHROPIC_API_KEY: that variable is read by Claude Code itself, so exporting
it to run this pilot would silently move an interactive session off its
subscription and onto this key's billing. Never pass it on the command line.

Every run this makes is capped twice: `--max-runs` bounds how many trials are
attempted, and `--max-input-tokens`/`--max-output-tokens` bound cumulative
measured usage across all calls in the process. Either limit stops the pilot
before the next API call, not mid-trial; a trial in progress is allowed to
finish its current round.

The token caps are reserved, not merely checked: a call debits its worst case
(a derived input estimate, the full requested output allowance) before it is
sent, and settles to the measured figures afterwards, so neither a single call
nor two concurrent ones can be admitted against a balance that cannot cover
them. A call whose billing outcome is unknown (a transport failure, or a
response carrying no usage) keeps its reservation and stops the pilot, rather
than being counted as zero.

A retry is a second request and reserves for itself. Only the statuses taken
as refused without being served (429, 529, 500, 502, 503) are retried, and
their reservation is released; any other failure may already have been billed,
so it keeps its worst case and stops the pilot rather than retrying blind.
The 5xx members of that set are an assumption, not a documented billing
guarantee: if one was billed after all, the ledger is short by that call. The input
reservation is derived from serialised characters and is an admission
heuristic, not a bound on tokenisation, so settlement re-checks the caps
against measured usage and stops if an estimate ran low.

Every request carries two prompt-cache breakpoints, one on the block where the
previous request wrote and one on its own tail, so each round reads the
conversation the round before it wrote rather than paying full price for it
again. Caching changes what a run costs, not how many tokens it processes:
the token caps count every token the model read, cached or not, and it is the
dollar figure that moves. That figure is the third cap, `--max-cost-usd`,
reserved and settled like the other two and priced from the table below.

The rivet condition's `read_docs` returns the usage guide (the whole of it,
or under `--orientation engine` only the sections documenting the tool's
commands), and every later call of that trial carries it again. It is most of what separates
rivet's token count from a baseline's, so a run reports it apart from the
rest: `orientation_input_tokens` is the guide's share of a trial's input, and
what is left is remaining input rather than the engine's own cost, since the
guide stays in the context the model reasons from whatever its tokens are
charged to. In the runs recorded so far the guide is most of the difference
between the conditions, which is an observed result and not something the
harness guarantees. Quote the two separately, and neither as the whole story.

Attribution has three states and they are not interchangeable: complete
coverage carrying nothing is `0`; something carried that could not be measured
is `null`; coverage short of the calls the trial was charged for reports no
split at all, whatever partial counts it holds.
"""
from __future__ import annotations

import argparse
import copy
import itertools
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request
from dataclasses import asdict, dataclass, field
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "benchmark" / "trials"))
import trials  # noqa: E402  (corpus, oracle, tool executors)

MODEL = "claude-sonnet-5"
API_URL = "https://api.anthropic.com/v1/messages"
API_VERSION = "2023-06-01"
CALL_TIMEOUT_S = 120
TOOL_TIMEOUT_S = 60
# Statuses treated as rejected without serving the request, so a retry is free
# and its reservation is released. Any other failure may have been billed
# before the response was lost. The 5xx members are an assumption rather than
# a documented billing guarantee: if one of them was in fact billed, the
# ledger is short by that call, which is the direction to know about when
# quoting a total.
# A trial is flagged `env_stall` when the host's IO pressure `some` counter
# rose by more than this over the trial: that many seconds in which at least
# one task on the machine was waiting on I/O. A rivet call's median is about
# 2 ms and a whole trial runs tens of seconds, while this host idles near
# 0.3% IO pressure (about 0.2 s a minute), so 5 s is far above the background
# and far below the two-minute stall that cost run 90 of 2026-10-16b. The flag
# says the host was in trouble during the trial; it does not say the trial's
# outcome was caused by it, which is why it sits beside the counts rather than
# changing them.
ENV_STALL_IO_S = 5.0
RETRYABLE_UNBILLED = frozenset({429, 529, 500, 502, 503})
MAX_ATTEMPTS = 3


class TrialFailed(RuntimeError):
    """A trial that could not be graded, carrying what it was charged for.

    The tally matters because the spend does not disappear with the failure: a
    result that keeps the tokens and drops the guide copies reports the whole
    of a rivet trial as remaining input. `provisional` says a writer was still
    running when this was raised, so the trial's spend could still move.
    `env` is the trial's host-stall classification and `evidence` the PSI
    snapshots and tool-process probes behind it: a stall bad enough to fail a
    trial is the one most worth flagging, so it leaves with the failure."""

    def __init__(self, message: str, attribution: "Attribution | None" = None,
                 provisional: bool = False, env: dict | None = None,
                 evidence: dict | None = None) -> None:
        super().__init__(message)
        self.attribution = attribution
        self.provisional = provisional
        self.env = env
        self.evidence = evidence


class TransportError(RuntimeError):
    def __init__(self, message: str, billable: bool) -> None:
        super().__init__(message)
        self.billable = billable
MAX_ROUNDS = 8  # API calls per agent, not tool calls: the final reply is a round
FULL_GUIDE = (ROOT / "docs" / "usage.md").read_text()

# What `read_docs` returns. `full` is the whole guide, as every run before
# 2026-10-16 used. `engine` is the guide cut by heading to the sections that
# document the commands the rivet tool advertises, plus the protocol, selector,
# guard and write-safety sections those commands depend on: the same text,
# fewer sections, so a run under it measures what the guide's size costs
# without rewording what it says. Each entry is a heading line and whether it
# takes its subsections ("tree") or only its own body ("own").
ORIENTATIONS: dict[str, list[tuple[str, str]] | None] = {
    "full": None,
    "engine": [
        ("## Protocol", "tree"),
        ("### `map`: outline, ids and revisions", "tree"),
        ("### `get`: one unit and its guard", "tree"),
        ("### `query`: typed items across files", "tree"),
        ("## Selectors", "tree"),
        ("## Revisions and guards", "tree"),
        ("## Writing", "own"),
        ("### `set`", "tree"),
        ("### `insert`", "tree"),
        ("### `delete`", "tree"),
        ("### `stamp`", "tree"),
        ("### Typed writes", "tree"),
        ("## Write safety", "tree"),
        ("## Typed collections", "tree"),
    ],
    # The sections for the three commands every 2026-10-05b rivet run used
    # (`map`, `query`, `update-item`) and the guards they pass. `usage.md` grew
    # from 35k to 86k bytes after that run, so `engine` carries only a fifth
    # less than 05b's whole guide did; this one carries about a quarter of it.
    "minimal": [
        ("## Protocol", "tree"),
        ("### `map`: outline, ids and revisions", "tree"),
        ("### `query`: typed items across files", "tree"),
        ("## Revisions and guards", "tree"),
        ("### Typed writes", "tree"),
    ],
    # `minimal` plus `find`, to measure whether answering a text lookup with
    # an address and its guards moves orientation's share of the gap. The
    # rivet tool names `find` only under a guide that documents it.
    "minimal-find": [
        ("## Protocol", "tree"),
        ("### `map`: outline, ids and revisions", "tree"),
        ("### `find`: units holding a text", "tree"),
        ("### `query`: typed items across files", "tree"),
        ("## Revisions and guards", "tree"),
        ("### Typed writes", "tree"),
    ],
}
FIND_HEADING = "### `find`: units holding a text"


def headings(lines: list[str]) -> list[tuple[int, int, str]]:
    """(line index, level, heading line) for each ATX heading outside fenced
    code."""
    heads = []
    fence = None
    for i, line in enumerate(lines):
        stripped = line.lstrip()
        if stripped.startswith(("```", "~~~")):
            mark = stripped[:3]
            fence = None if fence == mark else (fence or mark)
            continue
        if fence is None and line.startswith("#"):
            level = len(line) - len(line.lstrip("#"))
            if line[level:level + 1] == " ":
                heads.append((i, level, line.rstrip("\n")))
    return heads


def guide_subset(text: str, wanted: list[tuple[str, str]]) -> str:
    """The guide's sections named in `wanted`, in document order, verbatim.

    A heading that is missing or appears twice is refused rather than skipped,
    so a guide edit that renames a section stops the run instead of quietly
    shrinking the orientation it measures. Lines inside fenced code are not
    headings."""
    lines = text.splitlines(keepends=True)
    heads = headings(lines)
    spans = []
    for title, mode in wanted:
        if mode not in ("tree", "own"):
            raise ValueError(f"unknown mode {mode!r} for {title!r}")
        at = [n for n, (_, _, h) in enumerate(heads) if h == title]
        if len(at) != 1:
            raise ValueError(f"guide heading {title!r} found {len(at)} times, need exactly 1")
        n = at[0]
        start, level = heads[n][0], heads[n][1]
        end = len(lines)
        for i, lv, _ in heads[n + 1:]:
            if mode == "own" or lv <= level:
                end = i
                break
        spans.append((start, end))
    spans.sort()
    for (_, e1), (s2, _) in itertools.pairwise(spans):
        if s2 < e1:
            raise ValueError("guide sections overlap; name each section once")
    return "".join("".join(lines[s:e]) for s, e in spans)


def orientation_text(name: str) -> str:
    wanted = ORIENTATIONS[name]
    return FULL_GUIDE if wanted is None else guide_subset(FULL_GUIDE, wanted)


def section_commands(text: str) -> set[str]:
    """The commands named in backticks in `text`'s headings, as in
    "### `get`: ..." and "### Recovery: `commit-pending`"."""
    return {name for _, _, line in headings(text.splitlines())
            for name in re.findall(r"`([a-z_-]+)`", line)}


def undocumented_commands(text: str) -> dict[str, list[str]]:
    """Each command the full guide gives a section that `text` names in
    backticks without carrying that section, with the lines naming it.

    The 10-19 `minimal-find` run lost 22 agents to a cut whose `find` section
    pointed at `get` and left `get` out, so a cut's mentions are checked, not
    assumed harmless."""
    carried = section_commands(text)
    found: dict[str, list[str]] = {}
    for command in sorted(section_commands(FULL_GUIDE) - carried):
        spellings = {command, command.replace("_", "-"), command.replace("-", "_")}
        lines = [line for line in text.splitlines()
                 if any(f"`{s}`" in line for s in spellings)]
        if lines:
            found[command] = lines
    return found


USAGE_DOC = FULL_GUIDE
# Set once per process from the model's own tokeniser; None when the free
# count was unavailable, in which case orientation is reported as unmeasured
# rather than estimated from bytes and quoted as if it had been counted.
USAGE_DOC_TOKENS: int | None = None

# The approval this pilot runs under. The CLI may lower these; it may not
# raise them, because they are the only representation of what was agreed.
# Approved 2026-10-05 (first agreed 2026-09-14): the 180-run target, which
# bounds the silent-loss rate at 5% per condition, priced at the first
# pilot's measured 26,058 input and 1,550 output tokens per run.
APPROVED_MAX_RUNS = 180
APPROVED_MAX_INPUT_TOKENS = 4_700_000
APPROVED_MAX_OUTPUT_TOKENS = 280_000

# Room the model is allowed for one reply. A whole-file write must fit, or the
# condition that has to emit whole files cannot perform the task at all.
DEFAULT_MAX_TOKENS = 1536

# Claude Sonnet 5 list prices, USD per million tokens, read 2026-09-13. A
# cache write costs 1.25x base input at the five-minute TTL this uses and a
# cache read 0.1x, so a written token is dearer than an uncached one and only
# pays for itself once a later round reads it.
PRICE_INPUT_PER_MTOK = 2.00
PRICE_OUTPUT_PER_MTOK = 10.00
CACHE_WRITE_MULTIPLIER = 1.25
CACHE_READ_MULTIPLIER = 0.10

# The dollar ceiling this pilot runs under, the same kind of record as the
# token caps: the CLI may lower it and may not raise it. Derived from the
# token approval rather than granting a new one, at the dearest each token can
# be: 4.7M input written to cache at $2.50 is $11.75, and 280k output at
# $10.00 is $2.80. It is the token approval restated in the unit a re-run is
# argued in, and it binds admission exactly as the token caps do, on the same
# request-size estimate and with the same settlement re-check behind it.
# All-uncached, the same run prices at $12.20; with the prompt cache working
# as measured, about $7.40.
APPROVED_MAX_COST_USD = 14.55


def cost_of(uncached: int, cache_creation: int, cache_read: int, output: int) -> float:
    """Dollars for one call's measured usage.

    `usage.input_tokens` is the uncached remainder only: the three input
    figures are disjoint parts of one prompt, which is why they are priced
    apart rather than summed and multiplied."""
    return (
        uncached * PRICE_INPUT_PER_MTOK
        + cache_creation * PRICE_INPUT_PER_MTOK * CACHE_WRITE_MULTIPLIER
        + cache_read * PRICE_INPUT_PER_MTOK * CACHE_READ_MULTIPLIER
        + output * PRICE_OUTPUT_PER_MTOK
    ) / 1_000_000


def worst_case_cost(est_input: int, max_output: int) -> float:
    """The most one call can cost, given the input estimate it is handed.

    Charged at the cache-write rate, the dearest an input token can be:
    reserving at the base rate would admit a call whose whole prompt is
    written to cache at 1.25x and land over the cap. It bounds every way the
    *estimated* prompt can settle across cached and uncached tokens, and no
    further: the estimate is the serialised request's own byte length, which
    is an admission heuristic rather than a proven bound on what the provider
    renders and tokenises. So this is not a hard spend bound, and settlement
    re-checks the caps against measured usage for exactly that reason.
    """
    return cost_of(0, est_input, 0, max_output)


# --------------------------------------------------------------------------
# Budget: enforced against the API's own reported usage, not an estimate.


@dataclass(frozen=True)
class Reservation:
    """Worst-case spend debited before a request is sent."""

    input_tokens: int
    output_tokens: int
    cost_usd: float = 0.0


@dataclass
class Budget:
    """Spend bound, shared across the two concurrent writer threads a
    `two_writers` trial starts.

    Admission reserves before it spends. Checking a running total against the
    cap cannot bound anything: at 99 of 100 tokens a call is still admitted
    and lands at 119, and two threads can pass the same check against the same
    balance. So a call debits its worst case up front (a derived input
    estimate, the full requested output allowance) and settles to the measured
    figures afterwards. Every counter read and write holds the lock, because
    `x += 1` is not atomic and a lost increment here spends real money."""

    max_runs: int
    max_input_tokens: int
    max_output_tokens: int
    max_cost_usd: float = float("inf")
    runs: int = 0
    input_tokens: int = 0
    output_tokens: int = 0
    cost_usd: float = 0.0
    # Settled measurements only, for reporting: a reservation is a worst case
    # and has no cache breakdown, so mixing one into these would price a call
    # that has not happened yet.
    uncached_input_tokens: int = 0
    cache_creation_tokens: int = 0
    cache_read_tokens: int = 0
    calls: int = 0
    unsettled_calls: int = 0
    # Reservations admitted and not yet settled or refunded. A run with one of
    # these outstanding has a request whose billing is still to come, which is
    # not the same as a run whose calls all settled.
    in_flight: int = 0
    stopped_reason: str | None = None
    _lock: threading.Lock = field(default_factory=threading.Lock, repr=False, compare=False)

    def __post_init__(self) -> None:
        # NaN loses every comparison, so a budget holding it would admit every
        # call and report a cap that nothing can exceed. Infinity is a real
        # answer (no dollar cap) and stays allowed; not-a-number is not.
        if self.max_cost_usd != self.max_cost_usd:
            raise ValueError("max_cost_usd cannot be NaN: a cap nothing can exceed is not a cap")

    def can_start_run(self) -> bool:
        with self._lock:
            return self.stopped_reason is None and self.runs < self.max_runs

    def reserve(self, est_input: int, max_output: int) -> Reservation | None:
        """Debit the worst case, or refuse and stop. Returns None when this
        call would breach any cap."""
        cost = worst_case_cost(est_input, max_output)
        with self._lock:
            if self.stopped_reason:
                return None
            if self.input_tokens + est_input > self.max_input_tokens:
                self.stopped_reason = (
                    f"input token cap would be exceeded "
                    f"({self.input_tokens}+{est_input}/{self.max_input_tokens})"
                )
                return None
            if self.output_tokens + max_output > self.max_output_tokens:
                self.stopped_reason = (
                    f"output token cap would be exceeded "
                    f"({self.output_tokens}+{max_output}/{self.max_output_tokens})"
                )
                return None
            if self.cost_usd + cost > self.max_cost_usd:
                self.stopped_reason = (
                    f"cost cap would be exceeded "
                    f"(${self.cost_usd:.2f}+${cost:.2f}/${self.max_cost_usd:.2f})"
                )
                return None
            self.input_tokens += est_input
            self.output_tokens += max_output
            self.cost_usd += cost
            self.calls += 1
            self.in_flight += 1
            return Reservation(est_input, max_output, cost)

    def refund(self, res: Reservation) -> None:
        """Return a reservation for a request the API refused without serving
        it. Only 429/529/5xx qualify; anything else may have been billed."""
        with self._lock:
            self.input_tokens -= res.input_tokens
            self.output_tokens -= res.output_tokens
            self.cost_usd -= res.cost_usd
            self.calls -= 1
            self.in_flight -= 1

    def _stop_if_over_locked(self) -> None:
        """The input reservation is derived from serialised characters, which
        is an admission heuristic and not a bound on how the model tokenises.
        Measured usage is authoritative, so settlement re-checks the caps: an
        estimate that ran low stops the pilot here rather than going unnoticed."""
        if self.stopped_reason:
            return
        if self.input_tokens > self.max_input_tokens:
            self.stopped_reason = f"input cap exceeded on settlement ({self.input_tokens}/{self.max_input_tokens})"
        elif self.output_tokens > self.max_output_tokens:
            self.stopped_reason = f"output cap exceeded on settlement ({self.output_tokens}/{self.max_output_tokens})"
        elif self.cost_usd > self.max_cost_usd:
            self.stopped_reason = f"cost cap exceeded on settlement (${self.cost_usd:.2f}/${self.max_cost_usd:.2f})"

    def settle(self, res: Reservation, usage: dict | None) -> None:
        """Replace the reservation with measured usage. `usage is None` means
        the billing outcome is unknown (a transport failure, or a response
        that carried no usage block): the worst case stands and the pilot
        stops, because counting an unknown as zero is how a cap is overrun
        without anyone noticing."""
        with self._lock:
            # An incomplete usage block is an unknown, not a zero.
            self.in_flight -= 1
            complete = isinstance(usage, dict) and {"input_tokens", "output_tokens"} <= usage.keys()
            if not complete:
                self.unsettled_calls += 1
                self.stopped_reason = self.stopped_reason or (
                    "a call's usage was missing or incomplete; its reserved worst case stands "
                    "and the pilot stopped"
                )
                return
            uncached = usage.get("input_tokens", 0)
            created = usage.get("cache_creation_input_tokens", 0)
            read = usage.get("cache_read_input_tokens", 0)
            out = usage.get("output_tokens", 0)
            self.input_tokens += uncached + created + read - res.input_tokens
            self.output_tokens += out - res.output_tokens
            self.cost_usd += cost_of(uncached, created, read, out) - res.cost_usd
            self.uncached_input_tokens += uncached
            self.cache_creation_tokens += created
            self.cache_read_tokens += read
            self._stop_if_over_locked()

    def snapshot(self) -> dict:
        """Every counter as of one moment, read under the lock.

        Published figures come from here rather than from separate reads: a
        writer that outlives its trial can settle between two of them, and a
        report assembled from several reads describes no state the run was
        ever in."""
        with self._lock:
            return {
                "runs": self.runs,
                "calls": self.calls,
                "input_tokens": self.input_tokens,
                "output_tokens": self.output_tokens,
                "cost_usd": self.cost_usd,
                "uncached_input_tokens": self.uncached_input_tokens,
                "cache_creation_tokens": self.cache_creation_tokens,
                "cache_read_tokens": self.cache_read_tokens,
                "unsettled_calls": self.unsettled_calls,
                "in_flight": self.in_flight,
                "stopped_reason": self.stopped_reason,
            }

    def stop(self, reason: str) -> None:
        """Refuse all further admissions. Used when something outside the
        budget's own accounting has gone wrong enough that continuing would
        spend against numbers nobody can trust."""
        with self._lock:
            self.stopped_reason = self.stopped_reason or reason

    def start_run(self) -> int:
        with self._lock:
            self.runs += 1
            return self.runs


@dataclass
class Attribution:
    """Guide copies per charged call, collected outside the agent.

    A trace only reaches its caller when `run_agent` returns, so a writer that
    raises takes with it whatever it recorded, including calls the budget was
    already charged for. Counting those as zero reports a rivet trial's whole
    input as remaining input; counting the trial as unknown when the other
    writer did return reports less than is known. Written here as each call is
    charged, so how much of the spend it covers is a fact to check rather than
    an inference from whether an object came back."""

    copies: list[int] = field(default_factory=list)
    _lock: threading.Lock = field(default_factory=threading.Lock, repr=False, compare=False)

    def charged(self, copies: int) -> None:
        with self._lock:
            self.copies.append(copies)

    def calls(self) -> int:
        with self._lock:
            return len(self.copies)

    def freeze(self) -> tuple[int, int, int | None]:
        """Calls covered, calls that carried the guide, and the tokens that
        accounts for, all from one reading.

        One reading because they are checked against each other: taken
        separately, a writer that appends between them yields a coverage count
        that certifies a total it was not part of, which publishes a confident
        split over the wrong data.

        The token figure is None only when the guide could not be measured
        *and* something was carried. Nothing carried is zero tokens whatever
        the measurement did, and reporting that as unknown hides a fact the
        run does have."""
        with self._lock:
            copies = list(self.copies)
        carried = sum(1 for c in copies if c)
        total = sum(copies)
        if not total:
            return len(copies), 0, 0
        if USAGE_DOC_TOKENS is None:
            return len(copies), carried, None
        return len(copies), carried, total * USAGE_DOC_TOKENS

    def totals(self) -> tuple[int, int | None]:
        covered, carried, tokens = self.freeze()
        return carried, tokens


# --------------------------------------------------------------------------
# Anthropic Messages API, stdlib only.


def api_key() -> str:
    # Popped, not read: `trials.Rivet` hands `os.environ` to every rivet
    # subprocess and `main` shells out to git, and neither needs the key.
    key = os.environ.pop("RIVET_PILOT_API_KEY", None)
    if not key:
        print("no RIVET_PILOT_API_KEY in the environment", file=sys.stderr)
        sys.exit(2)
    return check_key(key)


def check_key(raw: str) -> str:
    """A key with a newline in it reaches header validation unchanged, and the
    exception Python raises there quotes the value into an uncaught traceback.
    Reject it here, where the message can say nothing about the value."""
    key = raw.strip()
    if not key or not key.isprintable():
        print("RIVET_PILOT_API_KEY is empty or contains control characters", file=sys.stderr)
        sys.exit(2)
    return key


def call_messages(key: str, system: str, messages: list[dict], tools: list[dict],
                  max_tokens: int) -> tuple[dict, float]:
    body = json.dumps(
        {"model": MODEL, "max_tokens": max_tokens, "system": system, "messages": messages, "tools": tools}
    ).encode()
    req = urllib.request.Request(
        API_URL,
        data=body,
        headers={
            "x-api-key": key,
            "anthropic-version": API_VERSION,
            "content-type": "application/json",
        },
        method="POST",
    )
    start = time.perf_counter()
    try:
        with urllib.request.urlopen(req, timeout=CALL_TIMEOUT_S) as resp:
            return json.loads(resp.read()), time.perf_counter() - start
    except urllib.error.HTTPError as e:
        payload = e.read().decode(errors="replace")
        raise TransportError(f"HTTP {e.code}: {payload}", billable=e.code not in RETRYABLE_UNBILLED) from None
    except Exception as e:
        # Everything else: a lost connection, a read timeout, a truncated body,
        # a response that will not parse. The request reached the API and may
        # have been served and billed, so none of these may be retried blind
        # or counted as zero. Only the statuses above are known-unbilled.
        raise TransportError(f"{type(e).__name__}: {e}", billable=True) from None


def count_tokens(key: str, messages: list[dict], tools: list[dict]) -> int | None:
    """The model's own count for a request, or None if it cannot be had.

    The count_tokens endpoint is not billed, so this runs before the budget
    opens. It reports the model's estimate for a prompt rather than a billing
    figure, and a count from any other tokeniser would be a different model's
    answer entirely, so this is the only source the guide's share is quoted
    from."""
    body = json.dumps({"model": MODEL, "messages": messages, "tools": tools}).encode()
    req = urllib.request.Request(
        API_URL + "/count_tokens",
        data=body,
        headers={"x-api-key": key, "anthropic-version": API_VERSION, "content-type": "application/json"},
        method="POST",
    )
    try:
        with urllib.request.urlopen(req, timeout=CALL_TIMEOUT_S) as resp:
            return int(json.loads(resp.read())["input_tokens"])
    except Exception:
        # Attribution is reporting, not accounting. A pilot that has a budget
        # and a corpus does not stop because a free count was unavailable.
        return None


def usage_doc_tokens(key: str) -> int | None:
    """What the guide adds to a prompt, by difference.

    Counting it as a standalone message would charge it that message's own
    framing, and in a trial it arrives inside a tool result rather than as a
    message of its own. Two counts of the same request shape, one carrying the
    guide and one carrying nothing, cancel both. The result is still the
    endpoint's estimate, which is one reason the figure it feeds is reported
    as attribution rather than as spend."""
    def shaped(doc: str) -> list[dict]:
        return [
            {"role": "user", "content": [{"type": "text", "text": "task"}]},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "doc", "name": "read_docs", "input": {}}]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "doc", "content": doc}]},
        ]
    carried = count_tokens(key, shaped(USAGE_DOC), RIVET_TOOLS)
    empty = count_tokens(key, shaped(""), RIVET_TOOLS)
    if carried is None or empty is None:
        return None
    return carried - empty


# Blocks a breakpoint may not be placed on. Thinking is on by default on this
# model, so an assistant turn can begin with one, and a marker there is
# rejected outright: the call fails, its reservation stands, and the pilot
# stops. Tool results, tool uses, text and documents are all eligible.
UNCACHEABLE_BLOCKS = frozenset({"thinking", "redacted_thinking"})


def last_cacheable(message: dict) -> int | None:
    """The last block of a message a breakpoint may be placed on."""
    content = message["content"]
    # Every message this harness builds is in block form, including the first:
    # a string and a one-text-block list render differently, and alternating
    # between them mid-conversation would rewrite the prefix every round.
    assert isinstance(content, list), f"message content must be blocks, got {type(content).__name__}"
    for bi in range(len(content) - 1, -1, -1):
        if content[bi].get("type") not in UNCACHEABLE_BLOCKS:
            return bi
    return None


def cached_messages(messages: list[dict], anchor: int | None = None) -> list[dict]:
    """The conversation with ephemeral cache breakpoints, applied to a copy.

    The copy is the point: a marker left behind on every round's tail would
    pass the four-breakpoint limit by round five. Nothing is lost by removing
    an old one, because a breakpoint marks where an entry is *written* and a
    later request can look that entry up without a marker still being there.

    `anchor` is how many messages the previous request carried, so the block
    it names is exactly where that request wrote. Marking it is what makes
    this request read rather than rewrite, and it is why nothing here depends
    on how wide the turn since then was: a lookup walks back a bounded number
    of positions, and one assistant turn interleaving text with tool calls can
    exceed that on its own, but a marker sitting *on* the previous endpoint
    has no distance to walk. Without it a wide turn silently rewrites the
    whole conversation at 1.25x and reads none of it back, which costs more
    than not caching at all and shows up only as writes in the usage figures.

    The tail is marked too, so this request writes an entry the next one
    anchors to."""
    if not messages:
        return messages
    targets = []
    if anchor is not None and 0 < anchor <= len(messages):
        targets.append(anchor - 1)
    targets.append(len(messages) - 1)
    marked = list(messages)
    for mi in dict.fromkeys(targets):
        bi = last_cacheable(marked[mi])
        if bi is None:  # a turn of nothing but thinking: nowhere to put one
            continue
        message = dict(marked[mi])
        blocks = list(message["content"])
        blocks[bi] = {**blocks[bi], "cache_control": {"type": "ephemeral"}}
        message["content"] = blocks
        marked[mi] = message
    return marked


# --------------------------------------------------------------------------
# Tool schemas and dispatch per condition. Execution reuses trials.py's
# Naive/Patch/Rivet classes; only the caller (a model, not a script) differs.

NAIVE_TOOLS = [
    {
        "name": "read_file",
        "description": "Read the full contents of a file relative to the project root.",
        "input_schema": {"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]},
    },
    {
        "name": "write_file",
        "description": "Overwrite a file with new full contents.",
        "input_schema": {
            "type": "object",
            "properties": {"path": {"type": "string"}, "content": {"type": "string"}},
            "required": ["path", "content"],
        },
    },
]

PATCH_TOOLS = [
    {
        "name": "grep",
        "description": "Search a regular expression across the given files; returns matching lines with line numbers.",
        "input_schema": {
            "type": "object",
            "properties": {"pattern": {"type": "string"}, "paths": {"type": "array", "items": {"type": "string"}}},
            "required": ["pattern", "paths"],
        },
    },
    {
        "name": "read_range",
        "description": "Read lines [start, end] (1-indexed, inclusive) of a file.",
        "input_schema": {
            "type": "object",
            "properties": {"path": {"type": "string"}, "start": {"type": "integer"}, "end": {"type": "integer"}},
            "required": ["path", "start", "end"],
        },
    },
    {
        "name": "patch",
        "description": (
            "Replace `old` with `new` in a file. `old` must match exactly once in the file's current "
            "content or the patch is rejected unapplied; this is the only guard against a stale edit."
        ),
        "input_schema": {
            "type": "object",
            "properties": {"path": {"type": "string"}, "old": {"type": "string"}, "new": {"type": "string"}},
            "required": ["path", "old", "new"],
        },
    },
]

WRITE_COMMANDS = {"set", "insert", "delete", "stamp", "update-item", "insert-item", "write"}

def rivet_tools(find: bool = False) -> list[dict]:
    """The rivet condition's tools. The tool description names `find` only
    when the guide the run hands out documents it, so a run under an older cut
    is described exactly as earlier runs were. The CLI still answers `find`
    under every cut: a model that guesses the command gets it."""
    reads = "map, find, get, query" if find else "map, get, query"
    return [
        {
            "name": "rivet",
            "description": (
                f"Call the rivet CLI. `command` is one of: {reads}, set, insert, delete, stamp, "
                "update-item, insert-item, write (a single-file batch). `request` is the JSON request body "
                "documented in the rivet usage guide (call read_docs first if you have not read it). "
                "Returns the JSON response and the process exit code (0 ok, 1 unusable request, 2 refused "
                "with nothing changed, 3 a write that may have been applied)."
            ),
            "input_schema": {
                "type": "object",
                "properties": {"command": {"type": "string"}, "request": {"type": "object"}},
                "required": ["command", "request"],
            },
        },
        {
            "name": "read_docs",
            "description": "Return the rivet usage guide (CLI commands, selectors, request/response shapes, errors).",
            "input_schema": {"type": "object", "properties": {}},
        },
    ]


def advertises_find(guide: str) -> bool:
    return FIND_HEADING + "\n" in guide


RIVET_TOOLS = rivet_tools()


def confine(root: Path, given: str) -> str:
    """Resolve a model-supplied path inside the project, or refuse.

    The baseline tools join what they are given onto the project root, and
    joining an absolute path discards the root entirely. `markrivet` confines its
    own paths; without this the baselines would reach the whole filesystem,
    including this process's environment."""
    base = root.resolve()
    target = (base / given).resolve()
    if target != base and base not in target.parents:
        raise ValueError(f"path escapes the project: {given!r}")
    return str(target.relative_to(base))


class ToolError(str):
    """A tool call that failed in the harness rather than returning content.
    Still the plain message the model is sent; the type is what the trace
    records, since a successful read's content can begin `error:` too."""


def dispatch(cond, name: str, inp: dict) -> str:
    try:
        if isinstance(cond, trials.Naive):
            if name == "read_file":
                return cond.read(confine(cond.root, inp["path"]))
            if name == "write_file":
                cond.write(confine(cond.root, inp["path"]), inp["content"])
                return "ok"
        elif isinstance(cond, trials.Patch):
            if name == "grep":
                hits = cond.grep(inp["pattern"], [confine(cond.root, x) for x in inp["paths"]])
                return "\n".join(f"{r}:{n}:{l}" for r, n, l in hits) or "(no matches)"
            if name == "read_range":
                return cond.read_range(confine(cond.root, inp["path"]), inp["start"], inp["end"])
            if name == "patch":
                ok = cond.patch(confine(cond.root, inp["path"]), inp["old"], inp["new"])
                return "ok" if ok else "error: old text not found exactly once; re-read and retry"
        elif isinstance(cond, trials.Rivet):
            if name == "read_docs":
                return USAGE_DOC
            if name == "rivet":
                code, out = cond(inp["command"], inp["request"])
                return json.dumps({"exit_code": code, "response": out})
        return ToolError(f"error: unknown tool {name!r} for condition {cond.name!r}")
    except Exception as e:  # a tool error is a message back to the model, not a crash
        return ToolError(f"error: {type(e).__name__}: {e}")


TOOLS = {"naive": NAIVE_TOOLS, "patch": PATCH_TOOLS, "rivet": RIVET_TOOLS}

SYSTEM = """You are editing Markdown tracker files in a small project using only the tools provided.
Make exactly the requested change and nothing else: do not reformat, reword, or touch any file or
region not named in the task. When the task is a write, stop calling tools once the change is made and
reply with one short plain-text sentence confirming what you did. When the task is a read/report, call
tools as needed to gather the answer, then reply with the requested information as plain text and make
no further tool calls. If a tool reports a conflict or refusal, do not blindly retry with the same
arguments; either report the conflict in your final reply or make one considered retry with fresh
information."""


# --------------------------------------------------------------------------
# The agent loop: real API calls, real tool execution, no scripted decisions.


@dataclass
class Call:
    request: dict
    response: dict
    latency_s: float


@dataclass
class Trace:
    calls: list[Call] = field(default_factory=list)
    tool_calls: list[dict] = field(default_factory=list)
    final_text: str = ""
    rounds: int = 0
    aborted: str | None = None


def run_agent(key: str, budget: Budget, cond, task_prompt: str, max_tokens: int,
              first_call_barrier: threading.Barrier | None = None,
              cancel: threading.Event | None = None,
              attribution: Attribution | None = None) -> Trace:
    tools = TOOLS[cond.name]
    messages = [{"role": "user", "content": [{"type": "text", "text": task_prompt}]}]
    trace = Trace()
    # Where the last served request ended, and so where its cache entry sits.
    anchor: int | None = None
    for round_n in range(MAX_ROUNDS):
        # Checked at the top of every round and again before dispatching tools:
        # a trial already recorded as failed must stop spending and must stop
        # writing into a project that is about to be removed.
        if cancel is not None and cancel.is_set():
            trace.aborted = "cancelled: the trial was already recorded as failed"
            break
        if first_call_barrier is not None and round_n == 0:
            try:
                first_call_barrier.wait(timeout=30)
            except threading.BrokenBarrierError:
                trace.aborted = "the peer writer failed before its first call"
                break
        # The request's own byte length, as an admission estimate. A
        # byte-level tokeniser cannot emit more tokens than the text has
        # bytes, but this is the harness's serialisation rather than the
        # prompt the provider renders and tokenises, so it is not a proven
        # bound on the call: over-reserving is returned at settlement, and
        # under-reserving is what the settlement re-check exists to catch.
        sent = len(messages)
        # What this request carries, counted before it is sent. A guide the
        # model asked for and did not get (a baseline has no `read_docs`) is
        # not carried, so the returned text has to match.
        carrying = sum(1 for tc in trace.tool_calls
                       if tc["name"] == "read_docs" and tc["output"] == USAGE_DOC)
        payload = cached_messages(messages, anchor)
        body_bytes = len(json.dumps({"system": SYSTEM, "messages": payload, "tools": tools}).encode())
        resp = latency = None
        for attempt in range(MAX_ATTEMPTS):
            # Reserved per attempt, not per round: a retry is a second request
            # and may be billed like the first.
            res = budget.reserve(body_bytes, max_tokens)
            if res is None:
                trace.aborted = budget.stopped_reason
                break
            try:
                resp, latency = call_messages(key, SYSTEM, payload, tools, max_tokens)
            except TransportError as e:
                if e.billable:
                    # The request may already have been served. Its worst case
                    # stands and the pilot stops rather than retrying blind.
                    budget.settle(res, None)
                    if attribution is not None:
                        attribution.charged(carrying)
                    trace.aborted = f"call failed, billing unknown: {e}"
                    break
                budget.refund(res)
                if attempt == MAX_ATTEMPTS - 1:
                    trace.aborted = f"call failed after {MAX_ATTEMPTS} unbilled attempts: {e}"
                    break
                time.sleep(2**attempt * 2)
                continue
            budget.settle(res, resp.get("usage"))
            if attribution is not None:
                attribution.charged(carrying)
            break
        if resp is None or trace.aborted:
            break
        trace.calls.append(Call(
            {"messages": copy.deepcopy(messages), "tools": [t["name"] for t in tools]}, resp, latency))
        trace.rounds = round_n + 1
        # The request was served, so its entry exists and the next round can
        # anchor to it. A call that failed wrote nothing to anchor to.
        anchor = sent
        if resp.get("stop_reason") == "error" or "content" not in resp:
            trace.aborted = f"api error: {resp}"
            break
        if resp.get("stop_reason") == "max_tokens":
            # Truncated: the text is not a final answer and any tool_use block
            # may carry incomplete arguments. Dispatching it would grade a
            # cut-off reply as a wrong one.
            trace.aborted = f"reply hit max_tokens={max_tokens}; not a usable answer"
            break
        content = resp["content"]
        messages.append({"role": "assistant", "content": content})
        tool_uses = [b for b in content if b.get("type") == "tool_use"]
        if not tool_uses:
            trace.final_text = "".join(b.get("text", "") for b in content if b.get("type") == "text")
            break
        if cancel is not None and cancel.is_set():
            trace.aborted = "cancelled before dispatching tools"
            break
        results = []
        for tu in tool_uses:
            probes = getattr(cond, "probes", None)
            seen = len(probes) if probes is not None else 0
            started = time.time()
            out = dispatch(cond, tu["name"], tu.get("input", {}))
            # Wall clock, shared between the two writer threads: the ordering
            # of one agent's reads against the other's writes is the only
            # thing that separates a stale overwrite from a serial update.
            record = {"name": tu["name"], "input": tu.get("input", {}), "output": out,
                      "started": started, "ended": time.time(), "round": round_n,
                      "tool_error": isinstance(out, ToolError)}
            if probes is not None:
                # What the host did to each rivet process this call spawned:
                # duration, rusage, and on a timeout its state, wait channel
                # and reap time. Empty when the call spawned none.
                record["processes"] = probes[seen:]
            trace.tool_calls.append(record)
            results.append({"type": "tool_result", "tool_use_id": tu["id"], "content": out})
        messages.append({"role": "user", "content": results})
    else:
        trace.aborted = trace.aborted or f"hit MAX_ROUNDS={MAX_ROUNDS} without a final reply"
    return trace


def output_allowance(task: str, condname: str, before: dict[str, str]) -> int:
    """Reply room for one call.

    `naive` writes by emitting the entire file, so a ceiling below the file's
    own size does not make that condition expensive, it makes the task
    impossible; the 100-section document is ~13 KB against a 1,536-token
    default. `whole_document` asks every condition to reproduce a file. The
    tokens are still measured and still charged to the condition that spent
    them: this removes a harness artefact, not the cost being compared."""
    if condname == "naive" or task == "whole_document":
        largest = max((len(t) for t in before.values()), default=0)
        return max(DEFAULT_MAX_TOKENS, largest // 3 + 512)
    return DEFAULT_MAX_TOKENS


def condition_for(name: str, root: Path, binary: str):
    log = trials.Log()
    if name == "naive":
        return trials.Naive(root, log)
    if name == "patch":
        return trials.Patch(root, log)
    return trials.Rivet(root, log, binary)


# --------------------------------------------------------------------------
# Trials: task prompts and grading, mirroring benchmark/trials/trials.py's
# task shapes. Grading reuses the same oracle; nothing here trusts the model's
# own account of what it did.


@dataclass
class RunResult:
    task: str
    param: str
    condition: str
    correct: bool
    damage_bytes: int
    lost_updates: int
    conflicts_detected: int
    rounds: int
    tool_calls: int
    api_calls: int
    input_tokens: int
    output_tokens: int
    cache_read_tokens: int
    cache_creation_tokens: int
    latency_s: float   # summed API durations; for a concurrent trial, both agents
    wall_s: float      # elapsed time for the trial, tool execution included
    aborted: str | None
    notes: str
    cost_usd: float = 0.0
    # Calls whose prompt carried the usage guide, and the guide's share of
    # this trial's input tokens. Attribution, not billing: a carried copy that
    # was served from cache cost a tenth of what it counts here. Zero for
    # every condition but rivet, and for a rivet trial that never read it.
    doc_calls: int = 0
    # None when the guide could not be measured. Null rather than zero,
    # because a reader of the results file cannot tell those apart otherwise
    # and one of them puts the guide's tokens into the remainder.
    orientation_input_tokens: int | None = 0
    # False when the collector covered fewer calls than the budget charged for
    # this trial, so what the uncovered calls carried is not known. Coverage
    # is compared, not inferred from whether an agent returned an object.
    attribution_known: bool = True
    # True when a writer outlived the trial, so its calls could still settle
    # after this result's spend was read. The meta totals are authoritative.
    accounting_provisional: bool = False
    # Whether the host stalled during the trial (see `classify_env`): True,
    # False, or None when it could not be measured. Reported beside
    # `correct` and `lost_updates`, never folded into them.
    env_stall: bool | None = None
    env_stall_reason: str | None = None
    # Host IO / memory pressure `some` seconds over the trial; None unmeasured.
    io_stall_s: float | None = None
    memory_stall_s: float | None = None


def _psi_delta(start: dict, end: dict, resource: str) -> float | None:
    a, b = (start or {}).get(resource), (end or {}).get(resource)
    if not a or not b or "some" not in a or "some" not in b:
        return None
    delta = b["some"] - a["some"]
    # A counter that went backwards was not the same counter; not a reading.
    return round(delta / 1e6, 6) if delta >= 0 else None


def classify_env(psi_start: dict, psi_end: dict, tool_calls: list[dict]) -> dict:
    """Tell a host stall from the tool's own behaviour, for one trial.

    `env_stall` is True when host IO pressure rose by more than
    ENV_STALL_IO_S over the trial, or when a tool process timed out while in
    uninterruptible sleep (state D: the kernel, not the process, was holding
    it). It is False when IO pressure was measured and stayed under the
    threshold with no such timeout, and None when pressure could not be read
    and nothing else showed a stall: unmeasured is not the same as calm, and
    is never counted as a stall either. Applied identically to every
    condition, since a baseline's write can be held by the same host."""
    io = _psi_delta(psi_start, psi_end, "io")
    mem = _psi_delta(psi_start, psi_end, "memory")
    reasons = []
    if io is not None and io > ENV_STALL_IO_S:
        reasons.append(f"io_pressure_some={io:.1f}s>{ENV_STALL_IO_S:g}s")
    for c in tool_calls:
        for p in c.get("processes") or ():
            if p.get("timed_out") and p.get("state") == "D":
                reasons.append(f"{p.get('command')} timed out in state D wchan={p.get('wchan')}")
    if reasons:
        stall = True
    elif io is not None:
        stall = False
    else:
        stall = None
    return {"env_stall": stall, "env_stall_reason": "; ".join(reasons) or None,
            "io_stall_s": io, "memory_stall_s": mem}


def failure_env(psi_start: dict, conds) -> tuple[dict, dict]:
    """Classify a trial that raised, from what its conditions recorded.

    A trial that raised has no trace to read tool calls from, so the probes
    come from the conditions themselves. A writer still running may append to
    its list or still be filling its last probe; a copy of each probe taken now
    is what was known when the trial was recorded as failed, so the published
    evidence always reclassifies to the published flag. `dict()` of a dict is
    one C call under the GIL, so the copy is never torn mid-insert, though it may
    hold a probe the writer has only partly filled; that copy is what is classified
    and what is published."""
    psi_end = trials.psi_snapshot()
    procs = [dict(p) for c in conds for p in list(getattr(c, "probes", None) or ())]
    env = classify_env(psi_start, psi_end, [{"processes": procs}])
    return env, {"psi": {"start": psi_start, "end": psi_end}, "processes": procs}


def fresh_project(files: int, filler: int, binary: str) -> tuple[Path, dict[str, str]]:
    tmp = Path(tempfile.mkdtemp())
    try:
        root = tmp / "project"
        root.mkdir()
        generated = trials.corpus(root, files, filler)
        trials.stamp_all(root, list(generated), binary)
        for rel, text in generated.items():
            assert trials.unstamp(exact_text(root / rel)) == text, f"stamping changed {rel}"
        return root, {rel: exact_text(root / rel) for rel in generated}
    except BaseException:
        # The caller's `finally` only runs once this has returned a root.
        shutil.rmtree(tmp, ignore_errors=True)
        raise


def usage_from(trace: Trace) -> tuple[int, int, int, int, float]:
    # `.get("usage", {})` is not enough: a response may carry `"usage": null`,
    # which the budget already treats as an unknown, and crashing here would
    # cost the trial the trace and timings it did pay for.
    use = [c.response.get("usage") or {} for c in trace.calls]
    inp = sum(u.get("input_tokens", 0) for u in use)
    out = sum(u.get("output_tokens", 0) for u in use)
    cache_read = sum(u.get("cache_read_input_tokens", 0) for u in use)
    cache_create = sum(u.get("cache_creation_input_tokens", 0) for u in use)
    lat = sum(c.latency_s for c in trace.calls)
    return inp, out, cache_read, cache_create, lat


def trace_record(trace: Trace) -> dict:
    """The per-call requests/results/usage/latency the plan's acceptance gate
    asks a pilot to capture, kept compact: tool schemas are fixed (in meta),
    so a call record is its usage, latency, stop reason and tool exchanges."""
    return {
        "aborted": trace.aborted,
        "final_text": trace.final_text,
        "calls": [
            {
                "stop_reason": c.response.get("stop_reason"),
                "usage": c.response.get("usage"),
                "latency_s": round(c.latency_s, 3),
                # The assistant turn as it was returned. Without it a trace
                # cannot be replayed: the tool calls alone do not show what
                # the model said between them, and `request` holds a snapshot
                # taken before the conversation was appended to.
                "content": c.response.get("content"),
                "prompt_messages": len(c.request["messages"]),
            }
            for c in trace.calls
        ],
        "tool_calls": trace.tool_calls,
    }


def exact_text(path: Path) -> str:
    """The file as written. `Path.read_text` translates CRLF to LF, which
    hides a wholesale line-ending rewrite from every comparison downstream of
    it, including the one for the file a task was allowed to edit."""
    return path.read_bytes().decode("utf-8")


def untouched_damage(root: Path, before: dict[str, str], edited: set[str]) -> int:
    """Bytes changed in files the task never declared it would edit, plus the
    size of any file that was not there to begin with. Mirrors the check in
    `benchmark/trials/trials.py`: a read-only task declares no edits, so every
    file is held to byte-identity. A file the corpus never generated counts
    whole, because a task that leaves scratch behind has not left the project
    as it found it."""
    total = 0
    for rel, original in before.items():
        if rel in edited:
            continue
        # Raw bytes, no digest masking: `read_text` folds CRLF into LF and
        # `trials.damage` deliberately ignores stay digests, so a wholesale
        # line-ending rewrite or a changed digest both read as untouched.
        current = (root / rel).read_bytes()
        if current != original.encode():
            total += max(len(current), len(original.encode()))
    for path in root.rglob("*"):
        rel = str(path.relative_to(root))
        # `.rivet/` is engine state, not the user's content: corpus setup
        # stamps every condition's project, so it exists under all three, and
        # its lock and revisions are meant to change. Holding it to
        # byte-identity would mark every trial damaged.
        if path.is_file() and rel not in before and not rel.startswith(".rivet/"):
            total += path.stat().st_size
    return total


def budget_mark(budget: Budget) -> dict:
    """One reading of the whole ledger, to measure a trial against.

    The whole ledger and not three counters: a trial's spend and the number of
    calls it was charged for are checked against each other, and reading them
    at different moments lets a surviving writer refund between them. The
    result then certifies that its attribution covered every charged call
    while counting one fewer call than the attribution was measured against."""
    return budget.snapshot()


def spent_since(mark: dict, now: dict) -> tuple[int, int, float]:
    """What this trial cost the budget, reservations included.

    Both readings are passed in rather than taken here, so the caller can use
    the same `now` for the charged-call count it checks coverage against.

    Taken from the ledger rather than from the responses, so a call that was
    reserved and then lost to a transport failure still appears against the
    trial that made it. Summing this over the results reconciles with the meta
    totals, unless a writer outlived its trial and settled a call after its
    delta was read; that result says so, and the meta totals are the ones to
    quote. Summing reported usage reconciles with neither."""
    return (now["input_tokens"] - mark["input_tokens"],
            now["output_tokens"] - mark["output_tokens"],
            now["cost_usd"] - mark["cost_usd"])


def single_writer(task: str, param: str, key: str, budget: Budget, condname: str, binary: str, prompt_fn, grade_fn, files=1, filler=0) -> tuple[RunResult, dict]:
    started = time.perf_counter()
    mark = budget_mark(budget)
    tally = Attribution()
    root, before = fresh_project(files, filler, binary)
    try:
        cond = condition_for(condname, root, binary)
        psi_start = trials.psi_snapshot()
        try:
            trace = run_agent(key, budget, cond, prompt_fn(list(before)),
                              output_allowance(task, condname, before), attribution=tally)
        except Exception as e:
            # The calls it made were charged for, so what they carried has to
            # leave with the failure rather than being lost to it.
            env, evidence = failure_env(psi_start, [cond])
            raise TrialFailed(f"{type(e).__name__}: {e}", tally, env=env, evidence=evidence) from e
        psi_end = trials.psi_snapshot()
        try:
            correct, dmg, notes = grade_fn(root, before, trace)
            stray = untouched_damage(root, before, set() if task in trials.READ_ONLY else {REL0})
            if stray:
                dmg += stray
                correct = False
                notes = f"{notes} stray_damage={stray}".strip()
        except Exception as e:
            # The calls were made and paid for; their usage, latency and trace
            # are still valid measurements. Only the verdict is unavailable.
            correct, dmg, notes = False, 0, f"grading raised: {type(e).__name__}: {e}"
        _, _, cache_read, cache_create, lat = usage_from(trace)
        ending = budget.snapshot()
        inp, out, cost = spent_since(mark, ending)
        covered, doc_calls, doc_tokens = tally.freeze()
        result = RunResult(task, param, condname, correct, dmg, 0, 0, trace.rounds, len(trace.tool_calls),
                            len(trace.calls), inp, out, cache_read, cache_create, lat,
                            time.perf_counter() - started, trace.aborted, notes,
                            cost, doc_calls, doc_tokens,
                            attribution_known=covered == ending["calls"] - mark["calls"],
                            **classify_env(psi_start, psi_end, trace.tool_calls))
        return result, {"a": trace_record(trace), "psi": {"start": psi_start, "end": psi_end}}
    finally:
        shutil.rmtree(root.parent, ignore_errors=True)


def two_writers(task: str, param: str, key: str, budget: Budget, condname: str, binary: str, prompt_a, prompt_b, grade_fn) -> tuple[RunResult, dict]:
    started = time.perf_counter()
    mark = budget_mark(budget)
    # One tally for both writers: it is the trial that is reported, and a
    # writer that raises still leaves what it was charged for in here.
    tally = Attribution()
    root, before = fresh_project(1, 0, binary)
    try:
        barrier = threading.Barrier(2)
        cond_a, cond_b = condition_for(condname, root, binary), condition_for(condname, root, binary)
        traces = {}

        failures: dict[str, BaseException] = {}
        cancel = threading.Event()

        allowance = output_allowance(task, condname, before)

        def go(name, cond, prompt):
            try:
                traces[name] = run_agent(key, budget, cond, prompt, allowance,
                                         first_call_barrier=barrier, cancel=cancel,
                                         attribution=tally)
            except BaseException as e:  # joined threads do not re-raise
                failures[name] = e
                barrier.abort()  # release a peer still waiting on a first call

        ta = threading.Thread(target=go, args=("a", cond_a, prompt_a(list(before))))
        tb = threading.Thread(target=go, args=("b", cond_b, prompt_b(list(before))))
        psi_start = trials.psi_snapshot()
        ta.start(); tb.start()
        # Bounded: a writer wedged in a tool call must not hold the project
        # directory open past the point where the trial can still be graded.
        deadline = MAX_ROUNDS * (CALL_TIMEOUT_S + TOOL_TIMEOUT_S)
        ta.join(deadline); tb.join(deadline)
        if ta.is_alive() or tb.is_alive():
            # Stop them making further calls, give the current one time to
            # return, and refuse to delete a project a live thread still
            # writes to. Leaking a temp directory beats corrupting a trial or
            # spending past a run already recorded as failed.
            cancel.set()
            # Stop the shared budget too: a thread that outlives its trial
            # must not keep spending against a run already recorded as failed,
            # and the trials after it would be accounted against a total it is
            # still moving.
            budget.stop("a writer outlived its trial; stopping rather than spending alongside it")
            ta.join(CALL_TIMEOUT_S); tb.join(CALL_TIMEOUT_S)
            env, evidence = failure_env(psi_start, [cond_a, cond_b])
            if ta.is_alive() or tb.is_alive():
                # A live writer can still settle a call, so this trial's spend
                # is not final at the moment it is read.
                raise TrialFailed(
                    f"a writer did not stop within {deadline}s and is still running; "
                    f"leaving {root.parent} in place rather than deleting it underneath",
                    tally, provisional=True, env=env, evidence=evidence)
            raise TrialFailed(f"a writer did not finish within {deadline}s; the trial is not gradable",
                              tally, env=env, evidence=evidence)
        if failures:
            name, exc = next(iter(failures.items()))
            env, evidence = failure_env(psi_start, [cond_a, cond_b])
            raise TrialFailed(f"writer {name} failed: {type(exc).__name__}: {exc}", tally,
                              env=env, evidence=evidence) from exc
        psi_end = trials.psi_snapshot()
        try:
            correct, dmg, lost, conflicts, notes = grade_fn(root, before, traces["a"], traces["b"])
            stray = untouched_damage(root, before, {REL0})
            if stray:
                dmg += stray
                correct = False
                notes = f"{notes} stray_damage={stray}"
        except Exception as e:
            correct, dmg, lost, conflicts = False, 0, 0, 0
            notes = f"grading raised: {type(e).__name__}: {e}"
        _, _, cr_a, cc_a, lat_a = usage_from(traces["a"])
        _, _, cr_b, cc_b, lat_b = usage_from(traces["b"])
        ending = budget.snapshot()
        inp, out, cost = spent_since(mark, ending)
        covered, doc_calls, doc_tokens = tally.freeze()
        aborted = traces["a"].aborted or traces["b"].aborted
        result = RunResult(task, param, condname, correct, dmg, lost, conflicts,
                            traces["a"].rounds + traces["b"].rounds,
                            len(traces["a"].tool_calls) + len(traces["b"].tool_calls),
                            len(traces["a"].calls) + len(traces["b"].calls),
                            inp, out, cr_a + cr_b, cc_a + cc_b,
                            lat_a + lat_b, time.perf_counter() - started, aborted, notes,
                            cost, doc_calls, doc_tokens,
                            attribution_known=covered == ending["calls"] - mark["calls"],
                            **classify_env(psi_start, psi_end,
                                           traces["a"].tool_calls + traces["b"].tool_calls))
        return result, {"a": trace_record(traces["a"]), "b": trace_record(traces["b"]),
                        "psi": {"start": psi_start, "end": psi_end}}
    finally:
        if not (ta.is_alive() or tb.is_alive()):
            shutil.rmtree(root.parent, ignore_errors=True)


REL0 = "tracking/TRACKING_000.md"


def prompt_complete(rels: list[str]) -> str:
    return (
        f"In `{REL0}`, find the checkbox action titled exactly "
        f'"Action 1 of tracker 0." in the `## Action` section and mark it done '
        f"(the completed form of the checkbox). Change nothing else in the file."
    )


def grade_complete(root: Path, before: dict[str, str], trace: Trace):
    text = exact_text(root / REL0)
    line = trials.item_line(before[REL0], "Action 1 of tracker 0.")
    expected = before[REL0].replace(line, line.replace("- [ ]", "- [x]", 1), 1)
    dmg = trials.damage(text, expected)
    return dmg == 0, dmg, ""


def prompt_gate(rel: str, k: int, text: str) -> str:
    return (
        f'In `{rel}`, find the action titled "Action {k} of tracker 0." and change the text of its '
        f'**Gate** line to exactly: {text}\nChange nothing else in the file, including other actions\' gates.'
    )


def ended_refused(t: Trace) -> bool:
    """Did this writer's last attempt end in a refusal?

    An earlier refusal that the agent then retried successfully does not
    excuse that successful write later disappearing: it was acknowledged, and
    losing it is as silent as losing any other."""
    last_refusal = max((i for i, c in enumerate(t.tool_calls) if _refused(c)), default=None)
    if last_refusal is None:
        return False
    last_applied = max((i for i, c in enumerate(t.tool_calls) if applied_write(c)), default=-1)
    return last_refusal > last_applied


def _refused(c: dict) -> bool:
    if c["name"] == "patch":
        return c["output"] == "error: old text not found exactly once; re-read and retry"
    if c["name"] != "rivet":
        return False
    try:
        return json.loads(c["output"]).get("response", {}).get("error", {}).get("code") == "guard_conflict"
    except (TypeError, json.JSONDecodeError):
        return False


def _write_conflict_signaled(t: Trace) -> bool:
    """True if a write tool call was refused for a guard or preimage mismatch.

    This counts *refusals*, which is not the same as detected concurrency and
    must not be published as if it were: a mistyped `old` string, a nonunique
    one, a malformed guard or the wrong revision kind all land here with no
    second writer involved. It is a lower bound on silent-overwrite avoidance,
    not a measurement of conflict detection. What it does establish is the
    other direction: `naive` cannot produce one at all, because its tool has
    no guard to refuse on.

    Must not scan `read_docs` output: the usage guide it returns mentions
    `guard_conflict` as documentation, which is a false positive for any naive
    substring search across all tool outputs."""
    for c in t.tool_calls:
        if c["name"] == "patch" and c["output"] == "error: old text not found exactly once; re-read and retry":
            return True
        if c["name"] == "rivet":
            try:
                out = json.loads(c["output"])
            except (TypeError, json.JSONDecodeError):
                continue
            if out.get("response", {}).get("error", {}).get("code") == "guard_conflict":
                return True
    return False


READ_COMMANDS = {"map", "find", "get", "query"}


def is_read(c: dict) -> bool:
    if c["name"] in ("read_file", "read_range", "grep"):
        return True
    return c["name"] == "rivet" and c["input"].get("command") in READ_COMMANDS


def read_content(c: dict) -> str:
    """What a read returned from the documents: nothing when it failed. A
    refusal or tool error echoes the request (a path, a selector, find's
    text), so a value found there was sent by the model, not shown to it.
    Failure is what the call recorded, never the text's prefix: a file can
    begin `error:`."""
    out = str(c["output"])
    if c.get("tool_error"):
        return ""
    if c["name"] == "rivet":
        try:
            parsed = json.loads(out)
        except (TypeError, json.JSONDecodeError):
            return ""
        if parsed.get("exit_code") != 0:
            return ""
    return out


def accepted_writes(trace: Trace) -> list[dict]:
    """Write tool calls that changed the file, with their timings."""
    return [c for c in trace.tool_calls if applied_write(c)]


def overwrite_kind(winner: Trace, loser: Trace, lost_value: str) -> str:
    """Why the loser's value is not in the file.

    `stale`: the winner's surviving write was planned without ever seeing the
    value it destroyed. Nobody was told, which is the failure these trials
    exist to detect. `serial`: the winner read the lost value and then
    deliberately superseded it, which is ordinary sequential editing.

    Decided on what the winner's reads actually returned, not on timing.
    Timing alone says only that some tool call happened in between, and a read
    of an unrelated path (`.rivet/project.json`, another file) satisfies that
    while telling the agent nothing about the value it is about to destroy.
    Where the evidence does not settle it, say so rather than guess:
    `ambiguous` is not a pass."""
    loser_writes, winner_writes = accepted_writes(loser), accepted_writes(winner)
    if not loser_writes:
        return "loser-never-wrote"
    if not winner_writes:
        return "winner-never-wrote"
    last_loser_write = max(c["ended"] for c in loser_writes)
    surviving = max(winner_writes, key=lambda c: c["started"])

    # A read only informed the write if the model had its result before
    # planning it. Tools requested in the same assistant turn run in sequence,
    # so a read beside an already-planned write returns the newer value to the
    # harness and never to the model.
    saw_it = any(
        is_read(c) and c["round"] < surviving["round"] and lost_value in read_content(c)
        for c in winner.tool_calls
    )
    if saw_it:
        return "serial"

    read_after = any(
        is_read(c) and c["round"] < surviving["round"] and c["started"] > last_loser_write
        for c in winner.tool_calls
    )
    if read_after:
        # It read after the lost write landed, in a turn whose result it saw,
        # but that read did not return the value: another path, a narrower
        # range. Not evidence either way.
        return "ambiguous"

    # Nothing it saw carried the lost value. Ordering decides, and only when
    # the invocations do not overlap: a tool call's start and end bracket the
    # commit, they do not pin it, so overlapping intervals settle nothing.
    if surviving["started"] >= last_loser_write:
        return "stale"  # every read it had predates the write it destroyed
    if surviving["ended"] <= min(c["started"] for c in loser_writes):
        return "stale"  # the surviving value was fully written before the lost one
    return "ambiguous"


def grade_two(same: bool):
    def grade(root: Path, before: dict[str, str], ta: Trace, tb: Trace):
        rel = REL0
        kb = 1 if same else 2
        result = exact_text(root / rel)
        gates = {a["title"]: a["gate"] for a in trials.actions(result)}
        detected = _write_conflict_signaled(ta) or _write_conflict_signaled(tb)
        # A trial where one writer never wrote is not a concurrency
        # observation at all. Without this it grades exactly like a clean
        # serial update and reports zero lost updates.
        attempted = [any(is_write_attempt(c) for c in t.tool_calls) for t in (ta, tb)]
        if not all(attempted):
            idle = "A" if not attempted[0] else "B"
            return False, 0, 0, int(detected), f"writer {idle} never attempted a write; not a concurrency observation"
        if same:
            # Both agents target the SAME field with different values, so only
            # one can remain and "who won" is not itself a defect. What the
            # loser's disappearance means is: refused (the writer was told),
            # superseded by an agent that had read it (ordinary serial
            # editing), or destroyed by a write planned before it landed (a
            # silent lost update). Only the last is a failure, and the final
            # file cannot tell the three apart.
            final = gates.get("Action 1 of tracker 0.")
            a_kept, b_kept = final == "A was here.", final == "B was here."
            landed_cleanly = a_kept or b_kept
            expected = trials.set_gate(before[rel], 0, 1, final if landed_cleanly else "A was here.")
            dmg = trials.damage(result, expected)
            if not landed_cleanly:
                lost, kind, winner = 1, "neither-value-survived", "?"
            else:
                winner, wt, lt = ("A", ta, tb) if a_kept else ("B", tb, ta)
                lost_value = "B was here." if a_kept else "A was here."
                if ended_refused(lt):
                    lost, kind = 0, "refused"
                else:
                    kind = overwrite_kind(wt, lt, lost_value)
                    lost = int(kind == "stale")
            # `ambiguous` is not a pass: the evidence did not settle whether an
            # update was destroyed, and a trial that cannot answer its own
            # question is not a result.
            correct = landed_cleanly and dmg == 0 and lost == 0 and kind != "ambiguous"
            notes = f"winner={winner} outcome={kind} lost_silently={lost} detected={detected}"
        else:
            a_kept = gates.get("Action 1 of tracker 0.") == "A was here."
            b_kept = gates.get(f"Action {kb} of tracker 0.") == "B was here."
            expected = trials.set_gate(trials.set_gate(before[rel], 0, 1, "A was here."), 0, kb, "B was here.")
            dmg = trials.damage(result, expected)
            # A missing update that the tool refused out loud is not a lost
            # update: the writer was told. `lost` counts silent losses only,
            # which is the claim these trials exist to test.
            refused = [ended_refused(ta), ended_refused(tb)]
            missing = [not a_kept, not b_kept]
            lost = sum(1 for m, r in zip(missing, refused) if m and not r)
            announced = sum(1 for m, r in zip(missing, refused) if m and r)
            correct = not any(missing) and dmg == 0
            notes = f"a_kept={a_kept} b_kept={b_kept} lost_silently={lost} refused={announced}"
        return correct, dmg, lost, int(detected), notes
    return grade


def prompt_warm(rels: list[str]) -> str:
    steps = "\n".join(f"{i + 1}. Set it to: revision {i} holds." for i in range(5))
    return (
        f'In `{REL0}`, update the **Gate** line of the action titled "Action 1 of tracker 0." '
        f"five times in order, one tool call per step:\n{steps}\nOnly the final value should remain; "
        f"change nothing else in the file."
    )


def is_write_attempt(c: dict) -> bool:
    """Did this call try to change the file, whatever became of it?

    Separate from `applied_write`: a request the tool rejected for a bad shape
    or a stale guard was still an attempt, and a trial where it happened is a
    concurrency observation even though nothing landed."""
    if c["name"] in ("write_file", "patch"):
        return True
    return c["name"] == "rivet" and c["input"].get("command") in WRITE_COMMANDS


def applied_write(c: dict) -> bool:
    """Did this tool call change the file?

    For rivet, exit code 0 is not the question: `preview` plans without
    writing and `unchanged` wrote nothing, and both exit 0. Only `applied`
    changed bytes. Reading the exit code alone lets four previews and one
    write pass as five edits."""
    if c["name"] in ("write_file", "patch"):
        return c["output"] == "ok"
    if c["name"] != "rivet" or c["input"].get("command") not in WRITE_COMMANDS:
        return False
    try:
        parsed = json.loads(c["output"])
    except (TypeError, json.JSONDecodeError):
        return False
    if parsed.get("exit_code") != 0 or c["input"].get("request", {}).get("preview"):
        return False
    return parsed.get("response", {}).get("status") == "applied"


def successful_writes(trace: Trace) -> list[str]:
    """The payload of each write that actually changed the file, in order."""
    return [json.dumps(c["input"]) for c in trace.tool_calls if applied_write(c)]


def written_gate(c: dict) -> str | None:
    """The gate value a write left behind, read from what it wrote.

    Not a substring search over the request: `patch` carries the text it
    replaced as well as the replacement, so searching the whole payload
    credits a state the file never held. Each tool is asked for the value it
    actually put there."""
    inp = c["input"]
    if c["name"] == "write_file":
        found = [a["gate"] for a in trials.actions(inp.get("content", ""))
                 if a["title"] == "Action 1 of tracker 0."]
        return found[0] if found else None
    if c["name"] == "patch":
        if inp.get("old") == inp.get("new"):
            return None  # replaced text with itself: the file did not change
        return inp.get("new")
    if c["name"] == "rivet":
        request = inp.get("request", {})
        for key in ("gate", "value", "text", "content"):
            if isinstance(request.get(key), str):
                return request[key]
    return None


def grade_warm(root: Path, before: dict[str, str], trace: Trace):
    """Five successive edits, not one.

    Checking only the final value credits an agent that skipped straight to
    `revision 4 holds.` with a five-edit workload, and publishes its tokens,
    calls and latency as the cost of five edits. Each of the five states has
    to have been left in the file, in order, by a write the tool applied."""
    text = exact_text(root / REL0)
    expected = trials.set_gate(before[REL0], 0, 1, "revision 4 holds.")
    dmg = trials.damage(text, expected)
    step, applied = 0, accepted_writes(trace)
    for c in applied:
        value = written_gate(c)
        if value is not None and f"revision {step} holds." in value:
            step += 1
            if step == 5:
                break
    notes = f"steps_observed={step}/5 writes={len(applied)}"
    return dmg == 0 and step == 5, dmg, notes


def prompt_section(k: int) -> str:
    return f"In `{REL0}`, replace the body paragraph of `## Section {k}` with exactly: Replaced body.\nChange nothing else."


def grade_section(k: int):
    def grade(root: Path, before: dict[str, str], trace: Trace):
        import re

        text = exact_text(root / REL0)
        paragraph = re.search(rf"^Notes on .*, paragraph {k}\.$", before[REL0], re.M).group(0)
        expected = before[REL0].replace(paragraph, "Replaced body.", 1)
        dmg = trials.damage(text, expected)
        return dmg == 0, dmg, ""
    return grade


def prompt_query(rels: list[str]) -> str:
    listed = "\n".join(f"- {r}" for r in rels)
    return (
        "These tracker files share a project:\n"
        f"{listed}\n\n"
        'List every action in each file\'s `## Action` section that is NOT done and whose actor tag is '
        '`agent` (an actor tag looks like `` `agent` ``, `` `human:sam` `` or `` `process:nightly` `` '
        "right after the checkbox; an action with no tag is not owned by `agent`). "
        "Reply with one line per match, exactly: `path: title` (title without its trailing date), and "
        "nothing else."
    )


def grade_query(rels: list[str]):
    def grade(root: Path, before: dict[str, str], trace: Trace):
        import re

        truth = {
            (rel, re.sub(r" \(\d{4}-\d{2}-\d{2}\)$", "", a["title"]))
            for rel in rels
            for a in trials.actions(before[rel])
            if not a["done"] and a["actor"] == "agent"
        }
        lines, found = [], set()
        for line in trace.final_text.splitlines():
            if ":" in line:
                path, title = line.split(":", 1)
                lines.append((path.strip(), title.strip()))
                found.add((path.strip(), title.strip()))
        stray = [l for l in trace.final_text.splitlines() if l.strip() and ":" not in l]
        duplicates = len(lines) - len(found)
        correct = found == truth and not stray and not duplicates
        notes = f"{len(found)} found, {len(truth)} expected"
        if stray:
            notes += f", {len(stray)} non-answer lines"
        if duplicates:
            notes += f", {duplicates} duplicate lines"
        return correct, 0, notes
    return grade


def prompt_wholedoc(rels: list[str]) -> str:
    return f"Report the complete contents of `{REL0}`, verbatim."


def grade_wholedoc(root: Path, before: dict[str, str], trace: Trace):
    """Verbatim, with exactly two wrappings allowed, both spelled out here
    rather than left to a `.strip()` to decide:

    1. the file inside one fenced code block, optionally after a preamble; or
    2. the file as the tail of the reply, after at most one preamble line.

    Trailing newlines are the one thing forgiven, because a chat reply cannot
    carry a final newline reliably. Leading whitespace is not."""
    import re

    want = before[REL0].rstrip("\n")
    text = trace.final_text
    fenced = re.search(r"```[^\n]*\n(.*?)\n?```", text, re.S)
    if fenced:
        return fenced.group(1).rstrip("\n") == want, 0, "fenced"
    body = text.rstrip("\n")
    if body == want:
        return True, 0, "bare"
    head, sep, tail = body.partition("\n")
    if sep and tail == want:
        return True, 0, "one-line preamble"
    return False, 0, "not verbatim"


# --------------------------------------------------------------------------
# Plan: which runs to attempt, in priority order (coverage before repeats),
# so a budget stop truncates the tail rather than skewing toward one task.


def disjoint_entry(cname: str, binary: str):
    return ("two", dict(task="disjoint_nodes", param="A then B, concurrent", condname=cname, binary=binary,
                        prompt_a=lambda rels: prompt_gate(REL0, 1, "A was here."),
                        prompt_b=lambda rels: prompt_gate(REL0, 2, "B was here."),
                        grade_fn=grade_two(False)))


def build_plan(binary: str, safety: int = 0):
    """The trial schedule. `safety` > 0 replaces the mixed schedule with that
    many `disjoint_nodes` trials per condition, the one trial that can lose a
    disjoint edit silently (`same_node` cannot, under this harness's loose
    synchronisation), with the conditions interleaved so a cap that stops the
    run early leaves them evenly covered."""
    if safety > 0:
        return [disjoint_entry(cname, binary) for _ in range(safety) for cname in ("naive", "patch", "rivet")]
    plan = []  # list of (kind, kwargs) consumed lazily by main()
    for cname in ("naive", "patch", "rivet"):
        plan.append(("single", dict(task="complete_one", param="1 file", condname=cname, binary=binary,
                                     prompt_fn=prompt_complete, grade_fn=grade_complete)))
    for cname in ("naive", "patch", "rivet"):
        plan.append(("single", dict(task="warm_revisits", param="5 edits, one item", condname=cname, binary=binary,
                                     prompt_fn=prompt_warm, grade_fn=grade_warm)))
    for cname in ("naive", "patch", "rivet"):
        plan.append(("single", dict(task="section_body", param="100 sections", condname=cname, binary=binary,
                                     prompt_fn=lambda rels: prompt_section(50), grade_fn=grade_section(50),
                                     filler=100)))
    for cname in ("naive", "patch", "rivet"):
        plan.append(("single", dict(task="whole_document", param="1 file", condname=cname, binary=binary,
                                     prompt_fn=prompt_wholedoc, grade_fn=grade_wholedoc)))
    for cname in ("naive", "patch", "rivet"):
        plan.append(("single", dict(task="query", param="10 files", condname=cname, binary=binary,
                                     prompt_fn=prompt_query, grade_fn=None, files=10)))
    for cname in ("naive", "patch", "rivet"):
        plan.append(("two", dict(task="same_node", param="A then B, concurrent", condname=cname, binary=binary,
                                  prompt_a=lambda rels: prompt_gate(REL0, 1, "A was here."),
                                  prompt_b=lambda rels: prompt_gate(REL0, 1, "B was here."),
                                  grade_fn=grade_two(True))))
    for cname in ("naive", "patch", "rivet"):
        plan.append(disjoint_entry(cname, binary))
    # Repeats for variance on the cheapest, most frequent real task.
    for cname in ("naive", "patch", "rivet"):
        for _ in range(3):
            plan.append(("single", dict(task="complete_one", param="1 file (repeat)", condname=cname, binary=binary,
                                         prompt_fn=prompt_complete, grade_fn=grade_complete)))
    return plan


def run_one(kind: str, kw: dict, key: str, budget: Budget) -> tuple[RunResult, dict]:
    """Execute one plan entry. `kw` is consumed here and not reused."""
    if kind == "two":
        return two_writers(**kw, key=key, budget=budget)
    files, filler = kw.pop("files", 1), kw.pop("filler", 0)
    grade_fn = kw.pop("grade_fn")
    if grade_fn is None:  # query: the truth set needs the generated file list
        return single_writer(**kw, key=key, budget=budget, files=files, filler=filler,
                             grade_fn=lambda root, before, trace: grade_query(list(before))(root, before, trace))
    return single_writer(**kw, key=key, budget=budget, files=files, filler=filler, grade_fn=grade_fn)


def failed_run(task: str, param: str, condname: str, reason: str, spent: tuple[int, int, float],
               attribution: Attribution | None = None, charged_calls: int = 0,
               provisional: bool = False, env: dict | None = None) -> RunResult:
    """A trial that raised. Recorded rather than dropped: the run was attempted
    and may have spent tokens, and a silently shorter results list is the kind
    of gap that makes a published number unreproducible. `spent` is the
    budget's own delta across the trial.

    Attribution comes from the tally the failure carried out, and is known
    only when that tally covers every call the trial was charged for. A tally
    from one of two writers is not the trial's attribution: reporting it as
    though it were puts the other writer's guide copies into the remainder,
    and reporting the whole trial as unknown throws away what the first writer
    did record. Coverage is the thing that decides, so it is compared.

    `env` is the failure's host-stall classification (`classify_env`), which
    sits beside the verdict and changes nothing else in the row."""
    # An absent tally covers nothing, which is complete only for a trial that
    # was charged for nothing: a setup failure before the first call knows its
    # attribution is empty, and saying "unknown" there throws that away.
    covered, doc_calls, doc_tokens = (attribution or Attribution()).freeze()
    return RunResult(task, param, condname, False, 0, 0, 0, 0, 0, 0, spent[0], spent[1], 0, 0, 0.0, 0.0,
                     reason, "trial raised", spent[2], doc_calls, doc_tokens,
                     attribution_known=covered == charged_calls,
                     accounting_provisional=provisional, **(env or {}))


def cache_note(ledger: dict) -> str:
    """What the run's cache figures say, including when they say nothing.

    Reads one snapshot rather than a live budget, so this describes the same
    moment the totals beside it do. A run that made no calls has neither read
    nor written anything, and reporting that as a landing breakpoint would be
    a claim about a mechanism that never ran."""
    if not ledger["calls"]:
        return "no calls were made, so the run says nothing about caching either way"
    # Every statement below describes the calls that reported usage. Calls
    # still outstanding reported none, and silence is not evidence that they
    # cached nothing or that nothing read an entry back, so the qualification
    # is appended to whichever description is chosen rather than replacing
    # one of them.
    pending = ledger["unsettled_calls"] + ledger["in_flight"]
    aside = (f" {pending} call(s) reported no usage or have not settled; nothing here covers them."
             if pending else "")
    if ledger["cache_read_tokens"]:
        return "cache reads were reported, so the breakpoints are landing." + aside
    if ledger["cache_creation_tokens"]:
        return ("entries were written at 1.25x and nothing read them back at 0.1x. Across a run "
                "of multi-round trials that costs more than not caching at all, and a prefix "
                "that changed between rounds or entries that expired are the causes to look for; "
                "a run of single-request trials writes without reading quite legitimately. "
                "Diagnose before quoting these costs." + aside)
    if pending:
        return ("the calls that settled cached nothing either way, and what the outstanding ones "
                f"did is unknown rather than nothing: {pending} of them reported no usage.")
    return ("nothing was cached either way, so every token was billed at the ordinary input "
            "rate. A prompt under the model's minimum cacheable size does this, and so does a "
            "marker the API never accepted. A single-round trial is not an explanation on its "
            "own: its request still writes, it simply has no later round to read the entry.")


def summarise_conditions(results: list[RunResult]) -> str:
    """Input tokens per condition, with the usage guide's share pulled out.

    Printed together because they are not the same claim and the difference is
    large: a rivet trial reads a guide no baseline is given, so the raw
    multiple between conditions is mostly documentation. What is left is
    remaining input, the closer comparison and still not the engine's
    intrinsic cost. A condition whose attribution did not cover its spend, or
    whose guide could not be measured, reports no split at all."""
    rows = ["", "input tokens by condition (mean per run):",
            f"  {'condition':10} {'runs':>4} {'total':>9} {'orientation':>12} {'remaining':>10} {'cost $':>8}"]
    raised = unmeasurable = provisional = False
    for name in ("naive", "patch", "rivet"):
        rs = [r for r in results if r.condition == name]
        if not rs:
            continue
        n = len(rs)
        total = sum(r.input_tokens for r in rs) / n
        cost = sum(r.cost_usd for r in rs) / n
        # Three different unknowns, none of which may be printed as a number.
        # A split reported where one of these holds states the opposite of
        # what is known: that the guide cost nothing.
        lost = [r for r in rs if not r.attribution_known]
        unmeasured = [r for r in rs if r.orientation_input_tokens is None]
        moving = [r for r in rs if r.accounting_provisional]
        raised = raised or bool(lost)
        unmeasurable = unmeasurable or bool(unmeasured)
        provisional = provisional or bool(moving)
        flag = "~" if moving else " "
        if lost or unmeasured:
            rows.append(f" {flag}{name:10} {n:4d} {total:9.0f} {'?':>12} {'?':>10} {cost:8.4f}")
        else:
            orient = sum(r.orientation_input_tokens for r in rs) / n
            rows.append(f" {flag}{name:10} {n:4d} {total:9.0f} {orient:12.0f} {total - orient:10.0f} {cost:8.4f}")
    if unmeasurable:
        rows.append("  ? the guide could not be measured, so neither its share nor the remainder")
        rows.append("    is known for a condition that carried it. The totals stand.")
    if raised:
        rows.append("  ? a trial's attribution did not cover what it was charged for: its spend is")
        rows.append("    in the total and what those calls carried is unknown, so no split is shown")
    if provisional:
        rows.append("  ~ a writer outlived its trial and could still settle a call, so this row is")
        rows.append("    a reading taken while its totals were still moving")
    rows.append("  remaining is input minus the guide's own tokens, not the engine's intrinsic")
    rows.append("  cost: the guide is still in the context the model reasoned from")
    return "\n".join(rows)


def summarise_env(results: list[RunResult]) -> str:
    """Host stalls per condition, beside the outcome counts and not inside
    them: `correct` and `lost` are what the grader saw, and a trial flagged
    here is one where the host, not necessarily the tool, may explain it.
    Whether to restate a published figure is a human call, so nothing here
    subtracts anything."""
    rows = ["", f"host stalls by condition (io pressure > {ENV_STALL_IO_S:g}s, or a tool timeout in state D):",
            f"  {'condition':10} {'runs':>4} {'correct':>7} {'lost':>4} {'env_stall':>9} "
            f"{'stalled&wrong':>13} {'unmeasured':>10}"]
    for name in ("naive", "patch", "rivet"):
        rs = [r for r in results if r.condition == name]
        if not rs:
            continue
        stalled = [r for r in rs if r.env_stall]
        rows.append(f"  {name:10} {len(rs):4d} {sum(r.correct for r in rs):7d} "
                    f"{sum(r.lost_updates for r in rs):4d} {len(stalled):9d} "
                    f"{sum(1 for r in stalled if not r.correct):13d} "
                    f"{sum(1 for r in rs if r.env_stall is None):10d}")
    rows.append("  correct and lost are unchanged by this table; a stalled trial is counted in both")
    return "\n".join(rows)


def parser() -> argparse.ArgumentParser:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--output", required=True, type=Path)
    ap.add_argument("--max-runs", type=int, default=APPROVED_MAX_RUNS)
    ap.add_argument("--max-input-tokens", type=int, default=APPROVED_MAX_INPUT_TOKENS)
    ap.add_argument("--max-output-tokens", type=int, default=APPROVED_MAX_OUTPUT_TOKENS)
    ap.add_argument("--safety", type=int, default=0, metavar="N",
                    help="run only disjoint_nodes, N trials per condition, instead of the mixed "
                         "schedule: the silent-loss bound. 3*N must fit --max-runs.")
    ap.add_argument("--max-cost-usd", type=float, default=APPROVED_MAX_COST_USD,
                    help="dollar ceiling for the whole run, reserved and settled like the token "
                         f"caps (default: the approved {APPROVED_MAX_COST_USD:.2f}). Caching moves "
                         "this figure and not the token counts, so it is the one that prices a "
                         "re-run.")
    ap.add_argument("--max-rounds", type=int, default=MAX_ROUNDS,
                    help="tool-call round trips allowed per agent before it is cut off "
                         f"(default: {MAX_ROUNDS}). Recorded in the output meta: it is a "
                         "ceiling that can bind one condition sooner than another.")
    ap.add_argument("--orientation", choices=sorted(ORIENTATIONS), default="full",
                    help="what the rivet condition's read_docs returns: the whole usage guide "
                         "(full, the default and every run before 2026-10-16), the sections "
                         "documenting the tool's commands (engine), only those for map, "
                         "query and update-item (minimal), or those plus find (minimal-find). "
                         "The rivet tool names find only when the guide documents it. "
                         "Recorded in the output meta.")
    return ap


def main() -> int:
    global MAX_ROUNDS, USAGE_DOC, USAGE_DOC_TOKENS, RIVET_TOOLS
    args = parser().parse_args()
    try:
        # Before anything is billed: a guide that no longer has a named
        # section stops here, not after the spend.
        USAGE_DOC = orientation_text(args.orientation)
    except ValueError as e:
        print(f"--orientation {args.orientation}: {e}", file=sys.stderr)
        return 2
    RIVET_TOOLS = rivet_tools(advertises_find(USAGE_DOC))
    TOOLS["rivet"] = RIVET_TOOLS
    # NaN loses every comparison, so an unchecked `--max-cost-usd nan` passes
    # the approval check below and then fails to bind admission or settlement:
    # a cap that is never exceeded because nothing can exceed it.
    if not (args.max_cost_usd >= 0) or args.max_cost_usd == float("inf"):
        print(f"--max-cost-usd must be a finite, non-negative number, got {args.max_cost_usd}",
              file=sys.stderr)
        return 2
    if args.safety < 0 or 3 * args.safety > args.max_runs:
        print(f"--safety {args.safety} needs 0 <= 3*N <= --max-runs ({args.max_runs})", file=sys.stderr)
        return 2
    over = [
        (name, got, cap)
        for name, got, cap in (
            ("--max-runs", args.max_runs, APPROVED_MAX_RUNS),
            ("--max-input-tokens", args.max_input_tokens, APPROVED_MAX_INPUT_TOKENS),
            ("--max-output-tokens", args.max_output_tokens, APPROVED_MAX_OUTPUT_TOKENS),
            ("--max-cost-usd", args.max_cost_usd, APPROVED_MAX_COST_USD),
        )
        if got > cap
    ]
    if over:
        # The defaults are the approval. Lowering them is a smaller run;
        # raising them is a different run than the one that was agreed.
        for name, got, cap in over:
            print(f"{name}={got} exceeds the approved ceiling of {cap}", file=sys.stderr)
        return 2

    traces_path = args.output.with_name(args.output.stem + "-traces.json")
    for path in (args.output, traces_path):
        if path.exists():
            print(f"{path} exists; choose a fresh --output path", file=sys.stderr)
            return 2
    try:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        probe = args.output.with_suffix(".probe")
        probe.write_text("")
        probe.unlink()
    except OSError as e:
        # Discovering this after the spend is how a paid run ends with no data.
        print(f"cannot write to {args.output.parent}: {e}", file=sys.stderr)
        return 2

    binary = shutil.which(os.environ.get("RIVET", "markrivet"))
    binary = os.path.abspath(binary) if binary else None
    if not binary:
        print("markrivet not found; set RIVET", file=sys.stderr)
        return 2

    MAX_ROUNDS = args.max_rounds

    key = api_key()
    # Free, and before anything is billed: the guide's share of a rivet trial
    # is quoted from the model's own tokeniser or not quoted at all.
    USAGE_DOC_TOKENS = usage_doc_tokens(key)
    if USAGE_DOC_TOKENS is None:
        print("count_tokens unavailable; orientation will be reported as unmeasured", file=sys.stderr)
    budget = Budget(args.max_runs, args.max_input_tokens, args.max_output_tokens, args.max_cost_usd)
    plan = build_plan(binary, args.safety)
    results: list[RunResult] = []
    traces: list[dict] = []

    published: dict = {}

    def write_output() -> None:
        # One reading of the ledger, under its lock, kept for the console to
        # render as well: a writer that outlived a trial can settle between
        # two separate reads, and a JSON file and a summary built from
        # different readings describe two different runs.
        ledger = budget.snapshot()
        published.update(ledger)
        meta = {
            "model": MODEL,
            "max_runs": args.max_runs,
            "safety_per_condition": args.safety,
            "max_input_tokens": args.max_input_tokens,
            "max_output_tokens": args.max_output_tokens,
            "max_cost_usd": args.max_cost_usd,
            "max_rounds": args.max_rounds,
            "runs_attempted": ledger["runs"],
            "runs_recorded": len(results),
            "api_calls": ledger["calls"],
            "input_tokens": ledger["input_tokens"],
            "output_tokens": ledger["output_tokens"],
            "cost_usd": round(ledger["cost_usd"], 4),
            "uncached_input_tokens": ledger["uncached_input_tokens"],
            "cache_creation_tokens": ledger["cache_creation_tokens"],
            "cache_read_tokens": ledger["cache_read_tokens"],
            "prices_usd_per_mtok": {
                "input": PRICE_INPUT_PER_MTOK,
                "output": PRICE_OUTPUT_PER_MTOK,
                "cache_write_multiplier": CACHE_WRITE_MULTIPLIER,
                "cache_read_multiplier": CACHE_READ_MULTIPLIER,
                "source": f"{MODEL} list prices, read 2026-09-13; no discount applied",
            },
            "orientation": args.orientation,
            "orientation_sections": [t for t, _ in ORIENTATIONS[args.orientation] or []] or None,
            "rivet_find": advertises_find(USAGE_DOC),
            "usage_doc_bytes": len(USAGE_DOC.encode()),
            "usage_doc_tokens": USAGE_DOC_TOKENS,
            "usage_doc_tokens_method": "count_tokens difference between a request carrying the "
                                       "guide in a tool result and the same request without it; "
                                       "the endpoint reports the model's estimate, not a bill",
            "stopped_reason": ledger["stopped_reason"],
            "unsettled_calls": ledger["unsettled_calls"],
            "calls_in_flight": ledger["in_flight"],
            "latency_note": "latency_s sums this trial's API durations and excludes tool time; "
                            "for a concurrent trial it sums both agents and is not elapsed time. "
                            "wall_s is the trial's elapsed time.",
            "token_note": "a result's input_tokens/output_tokens are the budget's own delta across "
                          "that trial, so they include a reserved call whose billing outcome was "
                          "never reported. They sum to these meta totals unless a result carries "
                          "accounting_provisional, which says a writer outlived its trial and could "
                          "still settle a call after that result's spend was read; these totals are "
                          "the authoritative ones. cache_read_tokens and cache_creation_tokens are a "
                          "reported breakdown of part of input_tokens, not additional spend.",
            "cost_note": "cost_usd prices settled usage at the table above and carries the worst "
                         "case of any call whose billing was never reported, so it is a figure to "
                         "argue from rather than an invoice. It rests on the same request-size "
                         "estimate the token caps do, and on 5xx retries being unbilled. Where "
                         "entries are read back, caching lowers cost_usd while leaving every "
                         "token count unchanged, so a token cap and a dollar cap are not "
                         "interchangeable; writes nothing reads back raise it instead.",
            "cache_note": cache_note(ledger),
            "orientation_note": "orientation_input_tokens attributes usage_doc_tokens to every "
                                "charged call that carried the guide, counted as each request was "
                                "sent so it covers the same calls the spend does. It is "
                                "attribution at full price, not billed cost. In the runs recorded "
                                "so far a rivet input figure quoted without it describes the guide "
                                "more than the engine, which is an observed result rather than a "
                                "property of the harness; and what is left over is remaining "
                                "input, not the engine's own cost, since the guide stays in the "
                                "context either way. Three states: 0 means complete coverage "
                                "carrying nothing, null means something carried that could not be "
                                "measured, and attribution_known false means the coverage fell "
                                "short of the calls charged, so no split is reported at all.",
            "env_stall_io_threshold_s": ENV_STALL_IO_S,
            "env_stall_note": "env_stall is True when host IO pressure (/proc/pressure/io some) "
                              "rose by more than env_stall_io_threshold_s over the trial, or a "
                              "tool process timed out in uninterruptible sleep (state D); False "
                              "when pressure was measured and stayed under it; null when it could "
                              "not be read. It is applied to every condition alike and does not "
                              "change correct or lost_updates: a stalled trial is still counted. "
                              "Per-trial PSI snapshots are in the traces file under psi, and each "
                              "rivet tool call carries its processes' duration, rusage and, on a "
                              "timeout, state, wchan and reap_s.",
            "commit": subprocess.run(["git", "-C", str(ROOT), "rev-parse", "--short", "HEAD"],
                                     capture_output=True, text=True).stdout.strip() or None,
            "tool_schemas": TOOLS,
            "note": f"Real {MODEL} tool-use trials. The model chose every tool call; grading is by an "
                    "oracle independent of the model's own account.",
        }
        args.output.write_text(json.dumps({"meta": meta, "results": [asdict(r) for r in results]}, indent=2) + "\n")
        traces_path.write_text(json.dumps(traces, indent=2) + "\n")
        print(f"traces: {traces_path}", file=sys.stderr)

    try:
        for kind, kw in plan:
            if not budget.can_start_run():
                print(f"stopping: {budget.stopped_reason or 'max runs reached'}", file=sys.stderr)
                break
            task, param, condname = kw["task"], kw["param"], kw["condname"]
            run_n = budget.start_run()
            print(f"[{run_n}/{args.max_runs}] {task} / {condname} "
                  f"(in={budget.input_tokens} out={budget.output_tokens})", file=sys.stderr)
            mark = budget_mark(budget)
            try:
                r, tr = run_one(kind, kw, key, budget)
            except Exception as e:
                # One bad trial truncates the pilot; it does not discard the
                # trials already paid for.
                reason = f"{type(e).__name__}: {e}"
                print(f"  FAILED: {reason}", file=sys.stderr)
                tally = e.attribution if isinstance(e, TrialFailed) else None
                failure = e if isinstance(e, TrialFailed) else None
                # One reading: spend and the charged-call count the coverage
                # is checked against have to describe the same moment.
                ending = budget.snapshot()
                results.append(failed_run(task, param, condname, reason, spent_since(mark, ending),
                                          tally, ending["calls"] - mark["calls"],
                                          bool(failure and failure.provisional),
                                          failure.env if failure else None))
                traces.append({"run": run_n, "task": task, "condition": condname, "error": reason,
                               **((failure.evidence or {}) if failure else {})})
                continue
            results.append(r)
            traces.append({"run": run_n, "task": r.task, "condition": r.condition, **tr})
            if r.aborted:
                print(f"  aborted: {r.aborted}", file=sys.stderr)
    finally:
        write_output()

    # The same reading the results file was written from, not a fresh one.
    ledger = published or budget.snapshot()
    print(f"\n{ledger['runs']} runs, {ledger['calls']} API calls, "
          f"{ledger['input_tokens']} input / {ledger['output_tokens']} output tokens")
    print(f"cost: ${ledger['cost_usd']:.2f} of ${args.max_cost_usd:.2f} "
          f"(cache: {ledger['cache_read_tokens']} read, {ledger['cache_creation_tokens']} written)")
    if ledger["in_flight"]:
        print(f"  {ledger['in_flight']} call(s) still in flight: these totals are a reading taken "
              "while they were moving")
    if ledger["calls"] and not ledger["cache_read_tokens"]:
        print(f"  no cache reads: {cache_note(ledger)}")
    print(f"correct: {sum(1 for r in results if r.correct)}/{len(results)}")
    print(summarise_conditions(results))
    print(summarise_env(results))
    return 0


if __name__ == "__main__":
    sys.exit(main())

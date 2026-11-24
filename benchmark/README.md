# Benchmark harness

`benchmark/harness/` is the canonical executable path. Older copies kept
with the design research are historical provenance, not maintained
entrypoints.
Saved documents and estimate CSVs are research artifacts, not engine results.

```sh
python3 -m venv .venv
.venv/bin/pip install -r benchmark/requirements.txt
.venv/bin/python benchmark/harness/gen_docs.py --output-dir /tmp/rivet-corpus --seed 1337
.venv/bin/python benchmark/harness/gen_docs.py --output-dir /tmp/rivet-query --count 100 --target-tokens 2000 --seed 42
.venv/bin/python benchmark/harness/test_generator.py
.venv/bin/python benchmark/harness/estimate_tokens.py --output /tmp/rivet-estimates.csv
```

Generator v2 derives each document seed with SHA-256 of version, root seed and
document ID. All content randomness comes from that document's RNG, including
the captured RNG state used for worklog sizing. Each table has unique labels,
exactly one `the caching layer` target and all five statuses. IDs and target
sizes are independent. The manifest records seeds, tokenizer, generator version
and measured content/row sizes. Token sizes are approximate targets; fixed
content sets a floor. The tokenizer is `cl100k_base`, not measured model usage.

Explicit output paths protect the historical corpus/results from incidental
regeneration. Use a fresh directory for each run; existing unrelated files in
an output directory are not removed. Byte reproducibility is tested in fresh
Python processes with different `PYTHONHASHSEED` settings.

The estimator's formulas remain a historical model with known limitations:
fixed tool/map overhead, incomplete call accounting, and an overly expensive
smart-baseline conflict recovery assumption. Its default input is the saved
four-size manifest. A regenerated four-size manifest may be supplied using
`--manifest`. It is not a live benchmark, acceptance gate, or evidence of savings.
Deterministic tool-only trials of the engine against two baselines, with an
independent oracle, are in `benchmark/trials/`. They remain the authority for
stale-write protection, because they control the ordering that makes a write
stale.

`benchmark/latency/bench.py` times commands on stamped synthetic trackers of
1,000 to 30,000 lines, single writers and concurrent ones, against prebuilt
binaries; every row carries the host's load. The limits in `docs/usage.md`
come from its results in `benchmark/results/latency-*.json`. Its README has
the method. `crates/markrivet-cli/tests/scaling.rs` guards the same growth in CI
as a 3,000-to-30,000-line ratio rather than a time.

`benchmark/pilot/pilot.py` runs the same three conditions with a real model
choosing every tool call, graded by the same oracle. It makes billed API
calls, so it is bounded four ways: `--max-runs` caps attempted trials, and
`--max-input-tokens`, `--max-output-tokens` and `--max-cost-usd` cap
cumulative measured usage, each reserved at its worst case before a call and
settled to what the API reported afterwards. It reads its key from
`RIVET_PILOT_API_KEY` only, never from `ANTHROPIC_API_KEY` and never from
argv. Run it against a built binary:

```sh
cargo build
RIVET=target/debug/markrivet python3 benchmark/pilot/pilot.py --output /tmp/pilot.json
```

It writes results to `--output` and per-call traces (tool calls with
timings, assistant turns, usage, latency, stop reasons) alongside it. Neither
is committed: a pilot run is evidence for a specific commit, recorded in the
working plan, not a fixture.

Each trace also says what the host was doing. A trial carries `psi`, the
`/proc/pressure/io` and `/proc/pressure/memory` `some`/`full` totals at its
start and end (`null` where the kernel does not expose them), and each rivet
tool call carries `processes`: duration, the child's rusage, and on the
tool timeout its scheduler state, wait channel and how long the kill took to
reap. A result is `env_stall: true` when IO pressure rose by more than
`env_stall_io_threshold_s` (in the `meta`) over the trial or a tool process
timed out in uninterruptible sleep, `false` when pressure was measured and
stayed under it, and `null` when it could not be read. A trial that raised
is classified the same way, from the probes its writers recorded, and its
trace entry keeps `psi` and `processes`. The flag applies to every condition
alike and changes neither `correct` nor `lost_updates`; the
console prints the stalled trials per condition in a table of their own.

Two things to know before quoting a token or dollar figure from a run.

**Caching moves the money, not the tokens.** Each request carries two
prompt-cache breakpoints: one on the block where the previous request wrote,
and one on its own tail for the next request to anchor to. The first is what
makes a round read rather than rewrite. A lookup walks back a bounded number
of positions and a single assistant turn can exceed that on its own, so a
marker only on the tail silently stops finding the previous entry once a turn
gets wide enough: the prefix is unchanged, every round rewrites the whole
conversation at the write premium, and the usage figures show writes with no
reads. A marker sitting on the previous endpoint has no distance to walk.
Neither is placed on a thinking block, which cannot carry one. The token counts are unchanged by that (a cached token is still a
token the model read) and the cost falls, which is why a run reports both and
why a token cap and a dollar cap answer different questions. `cost_usd` prices
settled usage at the list prices named in the results `meta`, counts a cache
write at 1.25x and a read at 0.1x, and carries the worst case of any call
whose billing was never reported. It is a figure to argue from rather than an
invoice: like the token caps, it admits a call against the serialised
request's byte length, which is an admission heuristic and not a proven bound
on what the provider renders, and it assumes a retried 5xx was never billed.
Settlement re-checks every cap against measured usage for that reason.

A run that reports writes and no reads is paying the write premium for entries
nothing comes back for; across multi-round trials that costs more than not
caching at all, though a single-request trial writes without reading quite
legitimately. A run that reports neither cached nothing across the calls that
settled. Calls still outstanding are covered by neither statement, and the
note says so rather than counting silence as nothing.

**The rivet condition reads a usage guide the baselines are not given**, and
then carries it in every later round of that trial. In the runs recorded so
far it is most of what separates rivet's input count from a baseline's, which
is an observed result rather than a property of the harness, so a run measures
the guide
with the model's own tokeniser and reports its share separately:
`orientation_input_tokens` per trial, an `orientation` column in the summary,
and what is left as `remaining`. A single input figure for rivet against a
baseline compares the documentation, not the engine. The remainder is
remaining input and not the engine's intrinsic cost: subtracting the guide's
tokens does not remove the guide from the context the model reasoned in.
Attribution is counted over exactly the calls the budget was charged for,
including one whose response was lost, so the two figures cover the same
calls and their difference means something.

`benchmark/pilot/test_pilot.py` checks the harness itself against scripted
models and hand-built traces. It makes no API calls and costs nothing:

```sh
RIVET=target/debug/markrivet python3 benchmark/pilot/test_pilot.py
```

Two properties it pins down are worth knowing before reading any pilot result.
A condition is allowed the reply length its own tool design needs, so a
whole-file writer can actually emit a whole file and is then charged for the
tokens, rather than being capped below the task and scored as incorrect. And a
value that did not survive a concurrent trial is classified from tool-call
timings, not from the final file: refused, superseded by a writer that had
read it, or destroyed by a write planned before it landed. Only the last is a
lost update, and the final file alone cannot tell the three apart.

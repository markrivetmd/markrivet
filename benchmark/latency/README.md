# Latency suite

`bench.py` times rivet commands on synthetic stamped trackers, so the limits
in [`docs/usage.md`](../../docs/usage.md) § Supported syntax and limits come
from a measurement anyone can repeat. It runs binaries you have already
built; it never builds or installs anything.

```sh
cargo build --release -p markrivet-cli
cp target/release/markrivet /tmp/rivet-new
python3 benchmark/latency/bench.py --bin new=/tmp/rivet-new \
  --work /tmp/rivet-latency --out /tmp/latency.json --concurrent-bin new
```

Pass `--bin LABEL=PATH` more than once to compare builds in one run. Copy
each binary out of `target/` first, since the next build overwrites it.
`--help` lists the other knobs: sizes, repetitions, writer counts and
writes per writer.

## The document

The same shape as the CI ratio test (`crates/markrivet-cli/tests/scaling.rs`):
frontmatter, an `## Action` list of three-line checkbox items (one per eight
lines), and a `## Log` of `###` sections (one per twelve lines). Every item
and section is stamped before timing starts. At 30,000 lines that is one list
of 3,750 items and about 1.2 MB with 6,250 stays, so the stamped list is the
part that grows.

## Single-writer rows

For each binary and size the suite builds one stamped project and times, per
repetition: `map`, a `query` page, `get` by id, an `update-item` preview, a
real `update-item` that ticks or unticks the file's last action with the guard
the previous write returned, and `stamp` of a freshly written unstamped copy.
Repetitions are interleaved across binaries and sizes, so a change in host
load lands on every cell alike. Each row reports n, min, median, p90 and max
in milliseconds.

`ratios` divides each binary's time at the largest size by its time at each
smaller size, for `map`, `update_item` and `stamp`, by median and by min. The median is the figure the 3k-to-30k
scaling gate reads; the min, an estimate of the uncontended
cost, is what the CI ratio test asserts.

## Concurrent rows

With `--concurrent-bin LABEL`, that binary also runs K writer processes
(`--writers`, default 1, 4 and 8) on K disjoint items of one stamped file
(`--writer-sizes`, default 3,000 and 30,000 lines), each making `--writes`
real ticks (default 10) with fresh guards. Writers to one project serialise
on its lock, so these rows show what queueing costs: wall time, per-write
latency, and the outcome of every write. A write that waited past
`lock_timeout_ms` (10 seconds by default) is counted under its error code
rather than retried.

## Load

Every row carries `load_1m` (the 1-minute load average, lowest and highest
seen while the row was taken) and `io_some_avg10` (`/proc/pressure/io`, `null`
where the kernel lacks it). Read a number beside its load: on a busy shared
host the same binary can run several times slower.

## Results

Runs cited by the docs are kept in `benchmark/results/` as
`latency-<date>-<commit>.json`. A result names its binaries, host CPU count
and kernel; the docs add the build and the load range.

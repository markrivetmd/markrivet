# markrivet

[![ci](https://github.com/markrivetmd/markrivet/actions/workflows/ci.yml/badge.svg?branch=master)](https://github.com/markrivetmd/markrivet/actions/workflows/ci.yml)
[![license: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![MSRV 1.89](https://img.shields.io/badge/MSRV-1.89-orange.svg)](Cargo.toml)

Site: [markrivet.dev](https://markrivet.dev)

Concurrent, guarded updates to the Markdown files agents share, by agents that
all use markrivet (Rivet for short).

Rivet reads ordinary Markdown files, gives headings and list items stable ids,
and applies small targeted writes that are checked against a revision of
exactly what they replace. Two agents can change different parts of the same
file at the same moment and both changes land; an agent working from a stale
read is refused instead of overwriting, with the current revision (except for
an item named by its place: the revision now at that place may be another
item's, so the agent re-reads). Writes are
meant to go through Rivet; the files stay plain, diffable Markdown that any
editor opens, and a hand edit made anyway is never overwritten.

Any document works: a plan, a handover, a changelog, a runbook, a task list.
Checkbox lists can also be typed, so their items are queried and updated as
records. The examples here use a task tracker, the busiest case.

## What works today

- **Addressing**: `map` outlines a file with ids and revisions; `get` returns
  one section, item or block with its guard. Ids are
  [markstay](https://markstay.org) v1.10 markers, so other markstay tools read
  them too, and they are optional for list items: on an unstamped file an
  item is addressed by its place (a heading path and an index), and `set` and
  `update-item` write it under its guard, refusing rather than landing on
  another item when something above it moved. Stamping gives an address that
  outlasts inserts and moves, and is needed for `move`, archiving and history.
- **Guarded writes**: `set`, `insert`, `delete`, `stamp`, `append` (to a
  section's own body), `replace_in_section` (one exact, unique, single-line
  substring inside a paragraph of a section's own body, including lists and quotes),
  `remove_paragraph_in_section` (an exact whole top-level paragraph without stays),
  `add_key` (a new frontmatter key) and `add_key_yaml`/`set_key_yaml` (a key's
  exact single-line YAML token), alone or as a single-file batch. Each write
  takes a project lock, re-reads the file,
  re-checks every guard, re-parses the complete result and verifies that
  nothing outside its declared change moved, then replaces the file durably.
  `create` writes a new document and never replaces one.
- **Archiving**: `archive-copy` copies a guarded item or stayed section subtree
  with its identities and exact content bytes. An archive precondition on the
  subsequent delete proves the committed copy still preserves them; section
  copies and deletes also require the original direct parent's heading stay.
- **Repairs**: `rejoin` moves a line break out of a wrapped code span and
  `adopt_stay` turns a hand-typed stay into the item's own, each refusing
  any shape but the one it fixes.
- **Typed lists**: a flat schema bound to the checkbox actions of a named
  section, and optionally of its subsections less an exclude list, so a new
  dated `###` batch joins the queue as soon as it is written. `query` lists
  matching actions across files; `update-item` and `insert-item` change
  fields and re-read them before committing. A title is optional, a label
  reads one line or, declared `multiline`, its continuation lines too, and
  ticking a checkbox changes one byte.
- **Frontmatter**: single-line scalars and calendar dates are read and
  written in place; empty values, lists and mappings are read as JSON. `set`
  refuses lists and mappings as targets. `add_key_yaml` and `set_key_yaml`
  copy a caller's exact single-line scalar or flat flow-list token, keeping
  or removing the line's trailing comment. `append_key_item` appends one flat
  mapping to a flow or block list of them (a `verified` list of `{by, at}`)
  in that list's own form. Replacing a whole block list or mapping is refused.
- **Refusals that group**: an item stamp refused behind a `<` or backslash
  (markstay §3.4) carries a `blocker` naming that character's line, column
  and offset, so one character behind many refused items reads as one
  problem. A closed inline code span is masked before the check.
- **Evidence**: markstay digests of changed units are refreshed; digests that
  were already stale are reported, not silently cleared.
- **Per-write commits** (opt-in): with `"git": {"commit": "per-write"}`, a
  write to a tracker that matched HEAD and passes the git preflight
  (attributes, identity, no lingering lock and the rest) becomes exactly one
  git commit, made under the project lock, so it never carries another
  session's uncommitted edits. A dirty tracker is skipped, adopted or
  refused by policy. Once a write has published its pending record, a crash
  leaves either its receipt or that record, which `commit-pending` settles;
  `status` reports records, the last write to each tracker and any git lock
  file left behind. `create` commits a new tracker the same way, and `adopt`
  stamps a hand-edited tracker and commits it whole.

What it does not do: protect against editors that ignore the lock (outside
edits are detected best effort), change several files atomically, create a
general directory tree, commit a write on top of someone else's uncommitted edit in the
same file (it skips, or adopts the whole file), or offer an MCP server yet.
Syntax outside the supported profile is refused rather than guessed, so a
real tracker can still be unreadable or partly unstampable. [docs/usage.md](docs/usage.md) lists every limit.

The native [`doctor` command](docs/usage.md#doctor-collection-health-and-coverage)
checks collection coverage, record validity, stable addressing and structural
write prerequisites without changing project bytes.

## When to use something else

- **A team that needs to be told.** Assignment, notifications, permissions
  and a board: use an issue tracker, and link the ticket from the Markdown.
- **Writers on different machines.** The guard holds inside one checkout;
  agents in separate clones still meet at a git merge. A tracker with its
  own sync fits better, such as [Beads](https://github.com/gastownhall/beads),
  which keeps its issues in Dolt and moves them between machines with Dolt's
  push and pull.
- **A queue nobody reads as prose.** A task queue that agents churn through,
  with little reasoning beside each item, fits an agent issue tracker such
  as Beads better than a file people open.
- **A personal knowledge base.** Notes you search by meaning and follow
  through their links, served to agents over MCP: [Basic
  Memory](https://github.com/basicmachines-co/basic-memory) fits better. It
  edits a note's sections by heading and, when asked, refuses an edit if the
  note changed since it was read.
- **High write rates or big data.** Thousands of records, or writes every
  second: use a database. Rivet is sized for files people still read.

## Platforms

Linux is supported. CI builds and tests on Linux x86_64 with the pinned
toolchain in `.github/workflows/ci.yml`; release builds are also tested on
Linux aarch64. The release binaries need glibc 2.34 or later. musl-based
distributions such as Alpine do not build today. The crates declare Rust 1.89
as their minimum (`rust-version`), which CI does not build with; per-write
commits and history need git 2.36 or later.

macOS is not supported yet: Rivet does not build for it, per-write commits and
recovery read Linux `/proc`, and its durability and path-identity guarantees
have not been checked on APFS (case-insensitive names, extended attributes
and ACLs, directory sync). Windows is not supported: the code imports
`std::os::unix` unconditionally. WSL2 on its Linux filesystem may work but is
untested.

## Install

Install from `github.com/markrivetmd/markrivet` with Cargo:

```sh
cargo install --locked --git https://github.com/markrivetmd/markrivet markrivet-cli
```

Or download a release binary:

```sh
# Linux x86_64 with glibc 2.34 or later (aarch64: change x86_64 in asset=)
set -e
asset=markrivet-x86_64-unknown-linux-gnu.tar.gz
base=https://github.com/markrivetmd/markrivet/releases/latest/download
curl -fLO "$base/$asset"
curl -fLO "$base/SHA256SUMS"
grep "  $asset\$" SHA256SUMS | sha256sum -c
tar -xzf "$asset"
"${asset%.tar.gz}/markrivet" --version
```

The release workflow builds and tests the binaries on x86_64 and aarch64
Linux and runs each one through the walkthrough on glibc 2.34.
`gh attestation verify <archive> -R markrivetmd/markrivet` checks an
archive's build provenance.

The crates are not on crates.io yet. To build from a checkout instead, see
below.

## Quickstart

```sh
cargo install --path crates/markrivet-cli
bash examples/tracker/walkthrough.sh
```

The walkthrough copies `examples/tracker/` to a scratch directory, diagnoses
the collection, queries open actions, completes one and logs it in a single batch,
then shows a stale write being refused and recovered: first on the unstamped
tracker by position, then again after stamping it, by id.

```sh
cd examples/tracker   # or any directory after `markrivet init`
echo '{"collection": "actions", "where": [{"field": "done", "op": "eq", "value": false}]}' \
  | markrivet --pretty query
```

## Documentation

- [docs/usage.md](docs/usage.md): install, protocol, selectors, revisions,
  writes, typed collections, write safety, limits. The maintained entry point.
- [docs/parser-contract.md](docs/parser-contract.md): the implementation
  contract: source spans, markstay attachment profile, identity, guards.
- [AGENTS.md](AGENTS.md): working on the code.

## Tests

Run `cargo test --workspace` in the debug profile. `.cargo/config.toml` sets
four test workers, including when the shell exports `RUST_TEST_THREADS`.
Each worker can spawn several Rivet and Git processes; running one worker per
CPU on a busy host can exhaust Git or lock deadlines in otherwise independent
fixtures. The multi-process tests still run their competing writers concurrently,
with the production deadlines and explicit fault-test deadlines unchanged.
For a deliberate worker-count experiment, pass `-- --test-threads=N` to Cargo.

## Layout

- `crates/markrivet-core`: document model, markstay adapter, planner, filesystem
  protocol, typed collections, JSON contract.
- `crates/markrivet-cli`: the `markrivet` binary and its integration and
  multi-process concurrency tests.
- `examples/tracker`: a synthetic project used by the walkthrough and tests.
- `benchmark/`: reproducible synthetic corpus generation, a historical
  token-cost model labelled as an estimate, and `pilot/`, a capped harness
  in which a real model drives the tool conditions. No published measured
  results yet.

## Background

The original framing was "SQLite semantics over Markdown, without SQLite":
addressable reads, targeted mutations, typed queries and concurrency control
without a database. The design research behind it, which is not published
with the code, pointed at concurrency safety, typed cross-file queries and
stable addressing as the places Rivet might help. Its token-savings figures
were estimates from a model, not measurements, and they depend heavily on the
baseline.

## License

MIT; see [LICENSE](LICENSE).

# Working on Rivet

Rivet's job is to never lose or misplace a byte of someone's Markdown while
agents write to it concurrently. Changes are judged against that first.

## What is authoritative

1. The executable tests under `crates/*/tests` and `examples/tracker/walkthrough.sh`.
2. [docs/usage.md](docs/usage.md) (the user contract) and
   [docs/parser-contract.md](docs/parser-contract.md) (the implementation
   contract). Keep both true in the present tense as the code changes.
3. `research/`: historical design research, kept in the private working
   repository and not in the published tree. It motivates, it does not
   specify. Its proposed features, API sketches and token estimates are not
   commitments or results. Do not cite it as evidence that something works.

When code and docs disagree, one of them is a bug; fix it in the same change.

## Rules for changes

- **Fix a missed rule once.** Before patching a review finding that is a missed
  case, grep for the rule's other copies. With two or more copies, unify them
  first and fix the case once.
- **Write-safety claims need evidence in the same change.** A new or changed
  write path needs a test showing bytes outside its declared edit are
  preserved, a refused write leaves the file byte-identical, and stays keep
  their attachment. Anything touching locking, rereading or replacement needs
  a multi-process test in `crates/markrivet-cli/tests/concurrency.rs` using the
  debug-build pause and fault hooks (`RIVET_TEST_PAUSE`, `RIVET_TEST_FAULT`).
- **Unsupported input is refused explicitly.** Add a diagnostic and a test
  for the refusal rather than guessing a target, falling back to whole-file or
  whole-frontmatter serialisation, or coercing a value.
- **Every documented command runs.** Examples in `docs/usage.md` come from, or
  are checked by, the walkthrough script or a CLI test. Label anything not
  implemented as such.
- **markstay behaviour is checked against markstay.** Attachment, child
  ownership and digests are tested against vectors in
  `crates/markrivet-core/tests/fixtures/markstay/`. `generate.py` there regenerates
  the reference vectors from the markstay Python linter; regenerate only when
  the pinned spec changes, and update the pinned hash in the test with it.
- **Fixtures are synthetic.** No real tracker content, names or private paths
  in tests, examples or docs.
- **Measurements are labelled.** Keep hypotheses, estimates and observed
  results apart; a token or latency number names its tokenizer, model and
  harness, or it is marked as an estimate.
- **`benchmark/pilot/pilot.py` spends real money.** It calls a paid API on
  every run. Do not run it to check a change; the deterministic trials in
  `benchmark/trials/` cover that and cost nothing, and
  `benchmark/pilot/test_pilot.py` exercises the pilot's own budget and grading
  offline. A pilot run needs an explicit budget agreed beforehand, and its
  caps are passed on the command line, not edited out: the defaults are the
  approval, and the tool refuses a value above them.
- **A grader change needs a case in `test_pilot.py`.** These graders decide
  what gets published about rivet against its baselines, and the failures that
  matter are the ones where a condition looks correct: an edit silently lost,
  a five-edit workload credited for one, a read-only task that damaged the
  corpus. Add the case that would have caught it.

## Checks

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
# benchmark generator tests, in the venv from benchmark/README.md
.venv/bin/python benchmark/harness/test_generator.py
# pilot harness: budget arithmetic and graders, offline and free
RIVET=target/debug/markrivet python3 benchmark/pilot/test_pilot.py
```

The published tree is checked for private paths, account names and copied
third-party pages before every push.

The concurrency tests spawn real `markrivet` processes and rely on debug-build
hooks, so run them in the default (debug) test profile.

## Layout

- `crates/markrivet-core/src/document.rs`: owned document model (comrak), sections,
  items, stay attachment, revisions, evidence digests.
- `crates/markrivet-core/src/markers.rs`: markstay marker spans on top of the
  pinned `markstay` crate.
- `crates/markrivet-core/src/markdown.rs`: the comrak queries planning and
  verification ask of a page (code extents, code spans wrapped across a line,
  code span texts, the rendered page, a rendered item parsed on its own), and
  the `inspect` spike.
- `crates/markrivet-core/src/edit.rs`: operation planning, evidence refresh and
  whole-candidate verification.
- `crates/markrivet-core/src/relocate.rs`: item and section move planning,
  two-path receipt schema, replay and recovery preconditions.
- `crates/markrivet-core/src/fs.rs`: project confinement, the lock, durable
  replacement, test hooks.
- `crates/markrivet-core/src/editor_lock.rs`: Emacs's `.#<name>` lock, parsed and
  checked for a live owner before a write publishes.
- `crates/markrivet-core/src/schema.rs`: schemas, bindings, typed extraction and
  rendering, query.
- `crates/markrivet-core/src/api.rs`: the JSON contract; `crates/markrivet-cli`: the binary.

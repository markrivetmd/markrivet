# Tool-only trials

`trials.py` runs scripted agents through the same tracker tasks with three
tool conditions and records what each would put into and take out of an
agent's context. No model is called. These are observed results for scripted
tool use, not model usage, and not evidence about how a model would behave.

```sh
cargo build -p markrivet-cli                  # a debug build: seeded ids
RIVET=target/debug/markrivet .venv/bin/python benchmark/trials/trials.py --output /tmp/trials.json
```

The run is deterministic: the corpus comes from a fixed seed and a debug
build of `markrivet` mints ids from `RIVET_TEST_ID_SEED`. Tokens are counted with
`tiktoken` `cl100k_base` when it is installed and omitted otherwise.
`--quick` runs the small sizes only; the CLI test suite runs it that way.

## Conditions

| Condition | Tools | Guard | Lock | Write |
|---|---|---|---|---|
| naive | read a whole file, write a whole file | none | none | in place |
| patch | grep across files, read a line range, replace exact unique old text | the old text | none | temp file and rename |
| rivet | the `markrivet` CLI | revisions | one project lock | temp file, rename, directory sync |

The `patch` baseline is deliberately strong: it greps many files in one call,
reads bounded ranges, remembers what it wrote so warm edits need no re-read,
and reconstructs section membership from heading lines in the same grep. Every
condition starts from the same stamped corpus and must keep stay markers
intact.

## Oracle

`actions()`, `normalise()` and `unstamp()` read the tracker shape line by
line without any Rivet code.

- Stamping is checked first: removing the markers Rivet added must give back
  the generated corpus exactly.
- A result is correct when it equals the expected file with stay-marker
  digests masked at their stored length (a correct Rivet write refreshes
  them; any other change, including to a digest's length, still counts).
- Every file a task should not touch must come back byte-identical, read-only
  tasks included.
- `damage_bytes` counts the bytes on lines that differ from the expected text,
  on both sides of a line diff.
- A lost update is an intended write that was neither kept nor explicitly
  refused. A same-node trial is correct only if the stale write is refused;
  a disjoint trial only if nothing is refused.

## Tasks

| Task | What the agent does |
|---|---|
| `complete_one` | tick one action |
| `same_node` | writers A and B read, then A and B both change the same action's gate |
| `disjoint_nodes` | as above, but B changes a different action |
| `racing_writers` | two disjoint edits whose read and write steps interleave inside the tool (Rivet: two real concurrent processes) |
| `warm_revisits` | five successive edits to one action by one agent |
| `section_body` | replace one section's body in a document of 10, 100 or 1000 sections |
| `query` | list open agent-owned actions across 100 trackers, excluding a distractor section |
| `whole_document` | read one whole tracker, the task a structural tool does not help with |

## Results

From `benchmark/results/trials.json` (commit 7e3d9d8, seed 20260911, tokens
by `tiktoken` `cl100k_base`). Bytes are the request (sent) and response
(received) payloads summed over the task.

| task | param | condition | correct | lost | conflicts | calls | sent B | received B | received tok |
|---|---|---|---|---|---|---|---|---|---|
| complete_one | 1 file | naive | yes | 0 | 0 | 2 | 2457 | 2411 | 839 |
| complete_one | 1 file | patch | yes | 0 | 0 | 2 | 250 | 117 | 40 |
| complete_one | 1 file | rivet | yes | 0 | 0 | 2 | 357 | 1098 | 449 |
| same_node | A then B, both from one read | naive | NO | 1 | 0 | 4 | 4884 | 4822 | 1678 |
| same_node | A then B, both from one read | patch | yes | 0 | 1 | 4 | 500 | 282 | 103 |
| same_node | A then B, both from one read | rivet | yes | 0 | 1 | 4 | 720 | 1693 | 658 |
| disjoint_nodes | A then B, both from one read | naive | NO | 1 | 0 | 4 | 4884 | 4822 | 1678 |
| disjoint_nodes | A then B, both from one read | patch | yes | 0 | 0 | 4 | 500 | 246 | 96 |
| disjoint_nodes | A then B, both from one read | rivet | yes | 0 | 0 | 4 | 720 | 2172 | 874 |
| racing_writers | tool steps interleaved | patch | NO | 1 | 0 | 2 | 234 | 4 | 2 |
| racing_writers | tool steps interleaved | rivet | yes | 0 | 0 | 3 | 486 | 2656 | 1114 |
| warm_revisits | 5 edits, one item | naive | yes | 0 | 0 | 6 | 12144 | 2419 | 843 |
| warm_revisits | 5 edits, one item | patch | yes | 0 | 0 | 6 | 1024 | 131 | 52 |
| warm_revisits | 5 edits, one item | rivet | yes | 0 | 0 | 6 | 1206 | 4006 | 1691 |
| section_body | 10 sections | naive | yes | 0 | 0 | 2 | 3506 | 3492 | 1215 |
| section_body | 10 sections | patch | yes | 0 | 0 | 3 | 153 | 137 | 48 |
| section_body | 10 sections | rivet | yes | 0 | 0 | 2 | 335 | 884 | 350 |
| section_body | 100 sections | naive | yes | 0 | 0 | 2 | 13229 | 13215 | 4517 |
| section_body | 100 sections | patch | yes | 0 | 0 | 3 | 156 | 139 | 43 |
| section_body | 100 sections | rivet | yes | 0 | 0 | 2 | 337 | 888 | 354 |
| section_body | 1000 sections | naive | yes | 0 | 0 | 2 | 112060 | 112039 | 37569 |
| section_body | 1000 sections | patch | yes | 0 | 0 | 3 | 152 | 134 | 48 |
| section_body | 1000 sections | rivet | yes | 0 | 0 | 2 | 339 | 878 | 349 |
| query | 100 files | naive | yes | 0 | 0 | 100 | 2400 | 241865 | 83908 |
| query | 100 files | patch | yes | 0 | 0 | 1 | 2522 | 38536 | 13172 |
| query | 100 files | rivet | yes | 0 | 0 | 1 | 169 | 24302 | 9934 |
| whole_document | 1 file | naive | yes | 0 | 0 | 1 | 24 | 2409 | 838 |
| whole_document | 1 file | patch | yes | 0 | 0 | 1 | 29 | 2409 | 838 |
| whole_document | 1 file | rivet | yes | 0 | 0 | 1 | 90 | 2736 | 1002 |

## Findings

Observed in these runs:

- **Correctness under concurrency is where Rivet differs.** `naive` loses an
  update in both two-writer cases. `patch` detects the same-node conflict
  through its old-text guard and keeps disjoint edits when writes happen one
  after another, but loses an update when two patch operations interleave
  their read and rename. Rivet refuses the stale same-node write and keeps
  every disjoint write, including from genuinely concurrent processes.
- **The strong baseline is cheaper per edit.** Ticking one action received
  117 bytes through `patch` and 1,098 through Rivet; five warm edits received
  131 against 4,006. Rivet's responses carry revisions (80 bytes and 37 to 50
  tokens each, measured over 50 samples), the new document revision and
  refreshed-evidence reports.
- **Rivet's response size does not grow with the document.** Replacing one
  section's body received about 880 bytes through Rivet at 10, 100 and 1000
  sections, and about 135 through `patch`; `naive` grew from 3.5 KB to 112 KB
  (37,569 tokens). Time does grow with the document, roughly in proportion
  to it; `benchmark/latency/` measures that.
- **Typed queries were the smallest way to answer a cross-file question.**
  Listing open agent-owned actions across 100 trackers took one Rivet query
  receiving 24.3 KB (9,934 tokens), one structured grep receiving 38.5 KB
  (13,172 tokens), or 100 whole-file reads receiving 242 KB (83,908 tokens).
  Rivet's answer also carries typed fields and a guard per item.
- **Whole-document reads cost slightly more through Rivet** (2,736 against
  2,409 bytes) because JSON escapes the text.

Not measured here: model behaviour, retries after a refusal, wall-clock time,
and real editors racing a write.

## Possible reductions

Suggested by the numbers, not yet made: shorter revision strings (128 bits
would halve their tokens), and a compact write response that omits document
revisions and evidence unless asked.

## Paid pilot (proposal, not run)

A pilot with a real model needs an agreed budget and a spend cap before any
call. Proposed shape, to be fixed before launch:

- Model: one current Claude model, named with its exact id in the results.
- Tasks: `complete_one`, `same_node` with a scripted second writer,
  `warm_revisits`, `query`, on the same seeded corpus.
- Conditions: `patch` and `markrivet`, each exposed as tool definitions whose
  schemas are counted in the overhead.
- Cap: 5 trials per task and condition (40 runs), stopping at the agreed cap.
- Budget estimate: the payloads above plus tool schemas and instructions
  suggest at most 50k input and 5k output tokens per run, so at most 2M input
  and 200k output tokens for the pilot. Convert with the chosen model's
  current prices at approval time; this file deliberately quotes no price.
- Capture: tool schemas, every request and result, reported usage including
  cached tokens, latency, correctness by the oracle above, and how each
  refusal or uncertain commit was handled.
- Sample size for a larger run comes from the pilot's variance and the effect
  size worth detecting, not from a fixed N.

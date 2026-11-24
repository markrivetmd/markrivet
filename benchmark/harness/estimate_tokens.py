"""
rivet benchmark — first-pass token estimation model.

Produces a reasoned (not measured, not fabricated-precise) estimate of
input/output tokens for each of the 8 tasks x 4 doc sizes, under two
conditions:
  - baseline   : read_file / search_files / apply_patch (fs tools)
  - structural : map(path) / get(path, selector) / set|append(path, selector, value)

All formulas are driven off harness/gen_docs.py's measured manifest.json
constants (frontmatter_tokens, one_paragraph_tokens, one_table_row_tokens,
one_worklog_entry_tokens, doc actual_tokens) so the numbers trace back to
an actual generated corpus, not vibes. Where a number is a genuine modeling
assumption (e.g. "apply_patch old_string needs N lines of unique context"),
it is named as a constant at the top of the file so it can be challenged
and tuned once the harness is run for real.

Two baseline sub-variants are computed:
  - baseline_naive : agent reads the ENTIRE file before editing (common
    behavior for simple ReAct-style ranging agents, and the necessary
    behavior for any task needing whole-doc context regardless of engine).
  - baseline_smart : agent uses search_files (grep) to locate the target
    region first (cheap output), reads a bounded excerpt around it, then
    patches. This is the strong/optimized baseline and is the fairer
    comparison; baseline_naive is reported as an upper bound.

Run: python estimate_tokens.py --output /tmp/estimate_grid.csv
Writes the selected output and prints a markdown table.
These historical formulas are hypotheses, not measured tool or conversation usage.
"""
import argparse
import csv
import json
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
by_id = {}

# ---- named modeling assumptions (tune these once real runs exist) --------
APPLY_PATCH_CONTEXT_LINES_TOKENS = 40   # old_string/new_string unique-match context
                                         # a patch tool typically needs a few lines of
                                         # surrounding text to disambiguate a match in
                                         # a doc with repetitive worklog phrasing
GREP_HIT_OUTPUT_TOKENS = 60             # search_files content-mode hit: path + line no + 2-3 ctx lines
EXCERPT_WINDOW_TOKENS = 250             # baseline_smart reads ~N tokens around the match,
                                         # not the whole doc (read_file with offset/limit)
TOOL_CALL_OVERHEAD_TOKENS = 15          # JSON call envelope (name+args wrapper) per call,
                                         # excluding the once-per-turn schema (see schema_tokens.py)
MODEL_REASONING_TOKENS_SMALL = 40       # short natural-language reasoning/confirmation the
                                         # model emits alongside a tool call (per call)
# map() DESIGN DECISION: a map() that enumerates every child node (e.g. one line
# per worklog entry) defeats the entire point of a structural engine -- it would
# make map() scale O(n) with document size, same as reading the whole file, just
# with a terser per-line format. A map() worth building instead returns a
# TABLE OF CONTENTS: frontmatter key list, section headings with a hash per
# section, table name(s) with row COUNT (not rows) + hash, worklog node with
# entry COUNT + first/last date + hash (not per-entry lines). This is the
# design that actually delivers "return only the changed subtree" -- map is a
# constant-ish-cost index, not a second copy of the document. We model it as
# base tokens per doc SHAPE (num sections/tables/worklog-as-one-node), which
# is fixed by our doc generator regardless of doc size, plus a small log-ish
# term for very large child counts (a real index may paginate/summarize further).
MAP_OUTPUT_BASE_TOKENS = 130            # ~6 section headings+hashes, 1 table w/ row count+hash,
                                         # 1 worklog node w/ count+range+hash, ~8 frontmatter keys
GET_OUTPUT_OVERHEAD_TOKENS = 10         # selector echo + hash wrapper around returned node content
SET_OUTPUT_TOKENS = 25                  # {new_hash, changed_selector, bytes_changed} envelope
CONFLICT_CHECK_TOKENS = 15              # expected_hash mismatch response, no doc content
GET_WORKLOG_TAIL_TOKENS = 8             # get(path, "worklog:last") overhead to fetch just 1 entry
                                         # for style-matching before an append (entry content billed
                                         # separately via one_wl(doc_id))

def naive_read_tokens(doc_id):
    return by_id[doc_id]["actual_tokens"]

def one_para(doc_id):
    return by_id[doc_id]["one_paragraph_tokens"]

def one_row(doc_id):
    return by_id[doc_id]["one_table_row_tokens"]

def one_wl(doc_id):
    return by_id[doc_id]["one_worklog_entry_tokens"]

def fm_tokens(doc_id):
    return by_id[doc_id]["frontmatter_tokens"]

def n_wl_entries(doc_id):
    return by_id[doc_id]["worklog_entries"]

def n_table_rows(doc_id):
    return by_id[doc_id]["table_rows"]


# ---------------------------------------------------------------------
# Each task function returns a dict of:
#   baseline_naive: (input_tokens, output_tokens, tool_calls)
#   baseline_smart: (input_tokens, output_tokens, tool_calls)
#   structural:      (input_tokens, output_tokens, tool_calls)
# "input_tokens" = tokens fed TO the model as tool results (doc content etc,
#   excludes the system prompt / tool-schema, which is measured separately
#   and is roughly constant across conditions -- see schema_tokens.py).
# "output_tokens" = tokens the model must GENERATE (tool call args + any
#   verification reasoning).
# ---------------------------------------------------------------------

def task_frontmatter_field(doc_id):
    # change one frontmatter field, e.g. status: in-progress -> status: blocked
    fm = fm_tokens(doc_id)
    naive_in = naive_read_tokens(doc_id)
    naive_out = APPLY_PATCH_CONTEXT_LINES_TOKENS + TOOL_CALL_OVERHEAD_TOKENS
    smart_in = GREP_HIT_OUTPUT_TOKENS + fm  # grep for "status:" hits frontmatter, tiny excerpt
    smart_out = 20 + TOOL_CALL_OVERHEAD_TOKENS  # old_string="status: in-progress", new_string=...
    struct_in = MAP_OUTPUT_BASE_TOKENS + GET_OUTPUT_OVERHEAD_TOKENS + 8  # map + get(frontmatter.status)->1 value
    struct_out = 15 + TOOL_CALL_OVERHEAD_TOKENS * 2  # map call + set call args (selector+value, tiny)
    return dict(
        baseline_naive=(naive_in, naive_out, 2),  # read, patch
        baseline_smart=(smart_in, smart_out, 2),  # grep, patch
        structural=(struct_in, struct_out, 2),    # map, set
    )

def task_worklog_append(doc_id):
    # append a new worklog entry at the end of the file
    naive_in = naive_read_tokens(doc_id)  # must read to find end-of-file / last entry style
    entry_out = one_wl(doc_id)
    naive_out = entry_out + APPLY_PATCH_CONTEXT_LINES_TOKENS + TOOL_CALL_OVERHEAD_TOKENS
    # smart: read last ~1-2 entries as a style example via tail-oriented excerpt (grep "## Worklog" + read tail)
    smart_in = GREP_HIT_OUTPUT_TOKENS + one_wl(doc_id) * 2
    smart_out = entry_out + TOOL_CALL_OVERHEAD_TOKENS
    # structural: map (constant-ish ToC) shows worklog as an appendable node with
    # entry count/hash; optionally get(worklog:last) for style (1 entry only, not all)
    struct_in = MAP_OUTPUT_BASE_TOKENS + GET_WORKLOG_TAIL_TOKENS + one_wl(doc_id)
    struct_out = entry_out + TOOL_CALL_OVERHEAD_TOKENS * 2
    return dict(
        baseline_naive=(naive_in, naive_out, 2),
        baseline_smart=(smart_in, smart_out, 2),
        structural=(struct_in, struct_out, 2),  # map (or get worklog tail), append
    )

def task_update_paragraph(doc_id):
    naive_in = naive_read_tokens(doc_id)
    para = one_para(doc_id)
    naive_out = para + APPLY_PATCH_CONTEXT_LINES_TOKENS + TOOL_CALL_OVERHEAD_TOKENS
    smart_in = GREP_HIT_OUTPUT_TOKENS + EXCERPT_WINDOW_TOKENS
    smart_out = para + TOOL_CALL_OVERHEAD_TOKENS
    struct_in = MAP_OUTPUT_BASE_TOKENS + GET_OUTPUT_OVERHEAD_TOKENS + para  # get current paragraph to ground the edit
    struct_out = para + TOOL_CALL_OVERHEAD_TOKENS * 2
    return dict(
        baseline_naive=(naive_in, naive_out, 2),
        baseline_smart=(smart_in, smart_out, 2),
        structural=(struct_in, struct_out, 2),  # map, set
    )

def task_table_row(doc_id):
    naive_in = naive_read_tokens(doc_id)
    row = one_row(doc_id)
    naive_out = row + APPLY_PATCH_CONTEXT_LINES_TOKENS + TOOL_CALL_OVERHEAD_TOKENS
    smart_in = GREP_HIT_OUTPUT_TOKENS + row * 3  # need header+row context to keep pipe alignment
    smart_out = row + TOOL_CALL_OVERHEAD_TOKENS
    struct_in = MAP_OUTPUT_BASE_TOKENS + GET_OUTPUT_OVERHEAD_TOKENS + row
    struct_out = row + TOOL_CALL_OVERHEAD_TOKENS * 2
    return dict(
        baseline_naive=(naive_in, naive_out, 2),
        baseline_smart=(smart_in, smart_out, 2),
        structural=(struct_in, struct_out, 2),  # map, set table:row
    )

def task_rename_section(doc_id):
    # rename a heading, e.g. "## Open Risks" -> "## Risks & Mitigations"
    naive_in = naive_read_tokens(doc_id)
    naive_out = APPLY_PATCH_CONTEXT_LINES_TOKENS + TOOL_CALL_OVERHEAD_TOKENS
    smart_in = GREP_HIT_OUTPUT_TOKENS + 20
    smart_out = 15 + TOOL_CALL_OVERHEAD_TOKENS
    struct_in = MAP_OUTPUT_BASE_TOKENS + GET_OUTPUT_OVERHEAD_TOKENS
    struct_out = 10 + TOOL_CALL_OVERHEAD_TOKENS * 2  # map, set heading title only
    return dict(
        baseline_naive=(naive_in, naive_out, 2),
        baseline_smart=(smart_in, smart_out, 2),
        structural=(struct_in, struct_out, 2),
    )

def task_two_distant_sections(doc_id):
    # e.g. update Overview paragraph AND append worklog entry in one logical op
    naive_in = naive_read_tokens(doc_id)  # one read still covers both (already have whole doc)
    para = one_para(doc_id)
    wl = one_wl(doc_id)
    naive_out = (para + APPLY_PATCH_CONTEXT_LINES_TOKENS + TOOL_CALL_OVERHEAD_TOKENS) + \
                (wl + APPLY_PATCH_CONTEXT_LINES_TOKENS + TOOL_CALL_OVERHEAD_TOKENS)
    # smart baseline needs TWO separate grep+read round trips since sections are far apart
    smart_in = 2 * (GREP_HIT_OUTPUT_TOKENS + EXCERPT_WINDOW_TOKENS)
    smart_out = (para + TOOL_CALL_OVERHEAD_TOKENS) + (wl + TOOL_CALL_OVERHEAD_TOKENS)
    # structural: one map (already have coordinates for both selectors -- ToC hashes
    # for Overview section and Worklog node), then 2 targeted writes
    struct_in = MAP_OUTPUT_BASE_TOKENS + GET_OUTPUT_OVERHEAD_TOKENS * 2 + para + wl
    struct_out = (para + TOOL_CALL_OVERHEAD_TOKENS) + (wl + TOOL_CALL_OVERHEAD_TOKENS) + TOOL_CALL_OVERHEAD_TOKENS
    return dict(
        baseline_naive=(naive_in, naive_out, 3),   # 1 read, 2 patches
        baseline_smart=(smart_in, smart_out, 4),   # 2x(grep+read), 2 patches -> 4 calls min (grep+patch each, read merged)
        structural=(struct_in, struct_out, 3),     # map, set, set
    )

def task_concurrent_conflict(doc_id):
    # Two writers race on the SAME node. Writer A succeeds, writer B must
    # detect the conflict rather than silently clobbering A's write.
    # Baseline: fs tools have NO structural conflict detection -- mtime/etag
    # checks at the whole-file level are the best available. Model:
    # both writers read-full, both patch; second patch either (a) succeeds
    # and silently overwrites A's change (data loss -- the realistic default
    # for plain write_file/apply_patch without a locking convention), or
    # (b) if the harness adds a naive mtime-check wrapper, B's read is stale
    # and the patch fails outright forcing a full re-read+retry.
    naive_in = 2 * naive_read_tokens(doc_id)          # both writers read whole doc
    naive_out = 2 * (APPLY_PATCH_CONTEXT_LINES_TOKENS + TOOL_CALL_OVERHEAD_TOKENS)
    # smart baseline: still whole-file granularity for conflict detection (mtime/hash
    # of full file), so a retry after conflict means a full re-read, not a targeted one
    smart_in = (GREP_HIT_OUTPUT_TOKENS + EXCERPT_WINDOW_TOKENS) + naive_read_tokens(doc_id)  # A cheap, B retries w/ full read
    smart_out = 2 * (30 + TOOL_CALL_OVERHEAD_TOKENS)
    # structural: expected_hash is scoped to the NODE, not the doc. Writer A's set
    # succeeds and returns new node hash. Writer B's set (stale node hash) is
    # rejected with a small conflict response; B re-fetches only that node (get)
    # and retries -- no full-doc re-read needed regardless of doc size.
    struct_in = (MAP_OUTPUT_BASE_TOKENS + GET_OUTPUT_OVERHEAD_TOKENS) + \
                (CONFLICT_CHECK_TOKENS + GET_OUTPUT_OVERHEAD_TOKENS + 20)  # A's map/get + B's conflict + B's re-get of node only
    struct_out = 2 * (20 + TOOL_CALL_OVERHEAD_TOKENS * 2)
    return dict(
        baseline_naive=(naive_in, naive_out, 4),      # 2x(read,patch)
        baseline_smart=(smart_in, smart_out, 4),
        structural=(struct_in, struct_out, 5),        # A: map,set  B: get,set(fail),set(retry)
    )

def task_query_100_files(doc_id):
    # status query across 100 files of THIS size. Not a single-doc task --
    # returns per-file-size totals; the grid divides by 100 for a "per file"
    # figure but the real deliverable is the 100-file total, reported separately.
    naive_in = naive_read_tokens(doc_id)   # per file: full read (or grep, see smart)
    naive_out = 8  # negligible -- no write, just tool call args (path)
    smart_in = GREP_HIT_OUTPUT_TOKENS  # grep "^status:" per file, one line hit
    smart_out = 8
    struct_in = fm_tokens(doc_id) if False else (GET_OUTPUT_OVERHEAD_TOKENS + 6)  # get(path,'frontmatter.status') -> 1 value
    struct_out = 8
    return dict(
        baseline_naive=(naive_in, naive_out, 1),
        baseline_smart=(smart_in, smart_out, 1),
        structural=(struct_in, struct_out, 1),
    )


TASKS = {
    "1_frontmatter_field": task_frontmatter_field,
    "2_worklog_append": task_worklog_append,
    "3_update_paragraph": task_update_paragraph,
    "4_table_row": task_table_row,
    "5_rename_section": task_rename_section,
    "6_two_distant_sections": task_two_distant_sections,
    "7_concurrent_conflict": task_concurrent_conflict,
    "8_query_100_files_PER_FILE": task_query_100_files,
}

DOC_IDS = ["doc_2k", "doc_10k", "doc_25k", "doc_100k"]

def main():
    parser = argparse.ArgumentParser(description="Historical token model; not engine measurements")
    parser.add_argument("--manifest", type=Path, default=ROOT / "docs" / "manifest.json")
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    global by_id
    by_id = {m["doc_id"]: m for m in json.loads(args.manifest.read_text(encoding="utf-8"))}
    missing = set(DOC_IDS) - by_id.keys()
    if missing:
        parser.error(f"manifest lacks the four-size set: {sorted(missing)}")
    rows = []
    for task_name, fn in TASKS.items():
        for doc_id in DOC_IDS:
            r = fn(doc_id)
            bn_in, bn_out, bn_calls = r["baseline_naive"]
            bs_in, bs_out, bs_calls = r["baseline_smart"]
            st_in, st_out, st_calls = r["structural"]
            bn_total = bn_in + bn_out
            bs_total = bs_in + bs_out
            st_total = st_in + st_out
            rows.append(dict(
                task=task_name, doc=doc_id, doc_tokens=by_id[doc_id]["actual_tokens"],
                baseline_naive_in=bn_in, baseline_naive_out=bn_out, baseline_naive_calls=bn_calls, baseline_naive_total=bn_total,
                baseline_smart_in=bs_in, baseline_smart_out=bs_out, baseline_smart_calls=bs_calls, baseline_smart_total=bs_total,
                structural_in=st_in, structural_out=st_out, structural_calls=st_calls, structural_total=st_total,
                reduction_vs_naive=f"{(1 - st_total/bn_total)*100:.1f}%",
                reduction_vs_smart=f"{(1 - st_total/bs_total)*100:.1f}%" if bs_total else "n/a",
            ))

    out_csv = args.output
    out_csv.parent.mkdir(parents=True, exist_ok=True)
    with out_csv.open("w", newline="") as f:
        w = csv.DictWriter(f, fieldnames=list(rows[0].keys()))
        w.writeheader()
        w.writerows(rows)

    # print a compact markdown table (totals only) for the report
    print("| Task | Doc | DocTok | Baseline-naive tot | Baseline-smart tot | Structural tot | Reduction vs naive | Reduction vs smart |")
    print("|---|---|---|---|---|---|---|---|")
    for row in rows:
        print(f"| {row['task']} | {row['doc']} | {row['doc_tokens']} | {row['baseline_naive_total']} | "
              f"{row['baseline_smart_total']} | {row['structural_total']} | {row['reduction_vs_naive']} | {row['reduction_vs_smart']} |")

if __name__ == "__main__":
    main()

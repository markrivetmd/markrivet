"""
rivet benchmark — synthetic document generator.

Generates realistic "maintained working doc" Markdown files at target
token sizes (~2k, ~10k, ~25k, ~100k) using cl100k_base token counting
(tiktoken) as a stable, reproducible proxy tokenizer. Real LLM tokenizers
(Claude/GPT) will differ by single-digit percent on English prose/Markdown,
which is acceptable for order-of-magnitude benchmark sizing.

Each doc has:
  - YAML frontmatter (title, id, owner, status, tags, created, updated, priority)
  - ## Overview
  - ## Architecture (several paragraphs)
  - ## Design Decisions (several paragraphs)
  - ## Open Risks
  - ## Component Status (a Markdown table, rows scale with size)
  - ## Worklog (dated entries, grows over time — this section absorbs most
    of the size increase between doc sizes, matching how real working docs
    actually grow: the worklog is append-heavy, everything else is stable)

Usage:
    python gen_docs.py --output-dir /tmp/rivet-corpus
Writes four sizes and manifest.json to the explicit output directory.
Use --count 100 --target-tokens 2000 for a multi-file corpus.
"""
import argparse
import hashlib
import json
import random
import textwrap
from pathlib import Path

import tiktoken

ENC = tiktoken.get_encoding("cl100k_base")
ROOT = Path(__file__).resolve().parent.parent
DOCS_DIR = ROOT / "docs"

def ntok(s: str) -> int:
    return len(ENC.encode(s))

TOPICS = [
    "the ingestion pipeline", "the auth service", "the billing reconciler",
    "the search index", "the notification fanout", "the schema migration tool",
    "the rate limiter", "the caching layer", "the export job",
    "the webhook dispatcher", "the audit log", "the retry policy",
]

PARA_TEMPLATES = [
    "The {topic} currently handles roughly {n} requests per minute at peak, "
    "and the current implementation relies on {mech} to keep latency bounded. "
    "We've observed {issue} under sustained load, which is why {owner} proposed "
    "revisiting the {component} before the next release cut.",
    "A key open question is whether {topic} should own its own retry budget or "
    "inherit one from the caller. {owner} leans toward the former because it "
    "keeps failure domains isolated, but that adds {n} lines of near-duplicate "
    "config across services, which {mech} was supposed to prevent in the first place.",
    "Since the last review, {topic} moved from {component} to {mech}, which cut "
    "{issue} substantially but introduced a new class of {n}-ms tail latencies "
    "during cold starts. {owner} is tracking whether that regresses the SLO.",
]

MECHS = ["a token-bucket limiter", "connection pooling", "an in-memory LRU cache",
         "batched writes", "a circuit breaker", "eventual-consistency reads",
         "a write-ahead log", "idempotency keys", "a leader-election lock"]
ISSUES = ["p99 latency spikes", "duplicate event delivery", "connection exhaustion",
          "stale cache reads", "partial writes on crash", "thundering-herd retries"]
COMPONENTS = ["the queue consumer", "the shard router", "the config loader",
              "the health checker", "the deploy pipeline", "the metrics exporter"]
OWNERS = ["Priya", "Marcus", "Dee", "Sanjay", "Wren", "Oksana", "Tobi", "Lena"]

def gen_paragraph(rng):
    t = rng.choice(PARA_TEMPLATES)
    return t.format(
        topic=rng.choice(TOPICS), n=rng.randint(3, 800), mech=rng.choice(MECHS),
        issue=rng.choice(ISSUES), owner=rng.choice(OWNERS), component=rng.choice(COMPONENTS),
    )

def gen_frontmatter(doc_id, rng):
    return textwrap.dedent(f"""\
    ---
    title: "{doc_id.replace('_', ' ').title()} — Working Doc"
    id: {doc_id}
    owner: {rng.choice(OWNERS)}
    status: in-progress
    priority: p2
    tags: [platform, infra, reliability]
    created: 2025-11-03
    updated: 2026-09-05
    ---

    """)

def gen_section(title, n_paragraphs, rng):
    body = "\n\n".join(gen_paragraph(rng) for _ in range(n_paragraphs))
    return f"## {title}\n\n{body}\n\n"

def gen_table(n_rows, rng):
    header = "| Component | Owner | Status | Notes |\n|---|---|---|---|\n"
    rows = []
    statuses = ["done", "in-progress", "blocked", "not-started", "needs-review"]
    for i in range(n_rows):
        comp = "the caching layer" if i == 0 else f"{rng.choice(COMPONENTS)} {i:03d}"
        owner = rng.choice(OWNERS)
        status = statuses[i % len(statuses)]
        note = f"see worklog {2025 + (i % 2)}-{(i % 12) + 1:02d}-{(i % 27) + 1:02d}"
        rows.append(f"| {comp} | {owner} | {status} | {note} |")
    return "## Component Status\n\n" + header + "\n".join(rows) + "\n\n"

def gen_worklog_entry(date, rng):
    author = rng.choice(OWNERS)
    bullets = "\n".join(
        f"- {gen_paragraph(rng).split('.')[0]}."
        for _ in range(rng.randint(1, 3))
    )
    return f"### {date} — {author}\n\n{bullets}\n\n"

def gen_worklog(n_entries, rng):
    out = "## Worklog\n\n"
    year, month, day = 2025, 11, 3
    for i in range(n_entries):
        day += rng.randint(1, 4)
        if day > 28:
            day -= 28
            month += 1
            if month > 12:
                month = 1
                year += 1
        out += gen_worklog_entry(f"{year:04d}-{month:02d}-{day:02d}", rng)
    return out

def build_doc(doc_id, target_tokens, rng):
    """Build doc by growing worklog entries until we hit target size.
    Fixed-cost sections (frontmatter, overview, architecture, decisions, risks,
    table) are held roughly constant in *shape* but table rows also scale a
    little with size, matching a real doc where both content and log grow."""
    fm = gen_frontmatter(doc_id, rng)
    overview = gen_section("Overview", 2, rng)
    architecture = gen_section("Architecture", 3, rng)
    decisions = gen_section("Design Decisions", 3, rng)
    risks = gen_section("Open Risks", 2, rng)

    table_rows = max(5, min(40, target_tokens // 2500 + 5))
    table = gen_table(table_rows, rng)

    fixed = fm + overview + architecture + decisions + risks + table
    fixed_tokens = ntok(fixed)
    budget = max(target_tokens - fixed_tokens, 200)

    # binary-search-ish growth of worklog entry count to hit budget
    lo, hi = 1, 4000
    best = None
    worklog_state = rng.getstate()
    while lo <= hi:
        mid = (lo + hi) // 2
        rng_copy = random.Random()
        rng_copy.setstate(worklog_state)
        wl = gen_worklog(mid, rng_copy)
        t = ntok(wl)
        if best is None or abs(t - budget) < abs(best[1] - budget):
            best = (mid, t, wl)
        if t < budget:
            lo = mid + 1
        else:
            hi = mid - 1
    n_entries, wl_tokens, worklog = best

    full = fixed + worklog
    return full, {
        "doc_id": doc_id,
        "target_tokens": target_tokens,
        "actual_tokens": ntok(full),
        "fixed_section_tokens": fixed_tokens,
        "worklog_tokens": wl_tokens,
        "worklog_entries": n_entries,
        "table_rows": table_rows,
        "frontmatter_tokens": ntok(fm),
        "one_paragraph_tokens": ntok(gen_paragraph(random.Random(42))),
        "one_table_row_tokens": ntok(table.splitlines()[4]),
        "row_target": "the caching layer",
        "tokenizer": "cl100k_base",
        "generator_version": 2,
        "one_worklog_entry_tokens": ntok(gen_worklog_entry("2026-01-01", random.Random(7))),
        "one_section_heading_line_tokens": ntok("## Architecture\n"),
    }

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--seed", type=int, default=1337)
    parser.add_argument("--count", type=int, help="Generate N files at --target-tokens instead of the four-size set")
    parser.add_argument("--target-tokens", type=int, default=2000)
    args = parser.parse_args()
    if args.count is not None and not 1 <= args.count <= 10000:
        parser.error("--count must be between 1 and 10000")
    if not 1 <= args.target_tokens <= 100000:
        parser.error("--target-tokens must be between 1 and 100000")
    targets = ({f"doc_{i:04d}": args.target_tokens for i in range(args.count)}
               if args.count is not None else
               {"doc_2k": 2000, "doc_10k": 10000, "doc_25k": 25000, "doc_100k": 100000})
    args.output_dir.mkdir(parents=True, exist_ok=True)
    manifest = []
    for doc_id, target in targets.items():
        # Stable across Python processes, hash randomization and iteration order.
        seed = int.from_bytes(hashlib.sha256(f"rivet-corpus-v2:{args.seed}:{doc_id}".encode()).digest(), "big")
        rng = random.Random(seed)
        text, stats = build_doc(doc_id, target, rng)
        stats["seed"] = str(seed)
        stats["root_seed"] = args.seed
        path = args.output_dir / f"{doc_id}.md"
        path.write_text(text, encoding="utf-8", newline="")
        manifest.append(stats)
        print(f"{doc_id}: target={target} actual={stats['actual_tokens']} "
              f"worklog_entries={stats['worklog_entries']} table_rows={stats['table_rows']}")
    (args.output_dir / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")

if __name__ == "__main__":
    main()

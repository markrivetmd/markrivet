#!/usr/bin/env python3
"""Regenerate reference vectors for Rivet's markstay adapter.

The markstay Python linter in CommonMark mode with child blocks enabled is the
reference for §5.2 block attachment, §5.5 list-item ownership, child bodies and
§8 digests. Rivet's tests read `reference-children.json`, `carrier-state.json`
and `profile-divergence.json`; they never run Python.

    MARKSTAY_REPO=/path/to/markstay python3 generate.py

MARKSTAY_REPO is a markstay checkout (its `linter/`, `impl/py/src` and `SPEC.md`).

Requires markdown-it-py. Table-row children (§5.6) are emitted for reference
but Rivet declines that optional profile; its test checks only that row
markers are never reported as block stays.
"""
import hashlib
import json
import os
import subprocess
import sys
from pathlib import Path

if not os.environ.get("MARKSTAY_REPO"):
    sys.exit("generate.py: set MARKSTAY_REPO to a markstay checkout")
REPO = Path(os.environ["MARKSTAY_REPO"])
sys.path.insert(0, str(REPO / "linter"))
import markstay_lint as M  # noqa: E402

sys.path.insert(0, str(REPO / "impl" / "py" / "src"))
import markstay.stamp  # noqa: E402  (`markstay.stamp` is also a function name in __init__)

M_STAMP = sys.modules["markstay.stamp"]

TRACKER = """---
status: active
owner: sam
---

# Widget tracker
<!-- stay:hTrk hash=sha256:000000000000 -->

> One paragraph of scope. Links go here.

## Action
<!-- stay:hAct -->

- [ ] **Ship the widget. (2026-09-01)** Context a session needs,
      wrapped onto a second line. <!-- stay:aOne subhash=sha256:111111111111 -->
      **Gate**: `make check` passes.

- [ ] `agent` **Refresh the fixtures.** <!-- stay:aTwo subhash=sha256:222222222222 -->
      **Gate**: fixture diff is empty.
  - a nested note <!-- stay:nNest subhash=sha256:333333333333 -->

- [x] `human:sam` **Closed item.**
      **Gate**: reply on the thread. <!-- stay:aThree subhash=sha256:444444444444 -->
<!-- stay:lAct hash=sha256:555555555555 -->

## Log
<!-- stay:hLog -->

Newest first.

### 2026-09-02: fixtures refreshed
<!-- stay:e2 -->

Ran the generator.

### 2026-09-01: tracker opened

First entry.
"""

DOCS = [
    ("tracker", TRACKER),
    (
        "tight-list-with-nested-child",
        "- one <!-- stay:c1 subhash=sha256:aaaa -->\n"
        "- two <!-- stay:c2 subhash=sha256:bbbb -->\n"
        "  - nested <!-- stay:c3 subhash=sha256:cccc -->\n"
        "<!-- stay:L hash=sha256:dddd -->\n",
    ),
    (
        "ordered-lists",
        "1. first <!-- stay:o1 subhash=sha256:1 -->\n"
        "2. second\n"
        "   continued <!-- stay:o2 subhash=sha256:2 -->\n\n"
        "3) paren list <!-- stay:o3 subhash=sha256:3 -->\n",
    ),
    (
        "crlf",
        "# Head\r\n<!-- stay:h -->\r\n\r\n- a <!-- stay:x subhash=sha256:ab -->\r\n"
        "  more a\r\n- b <!-- stay:y subhash=sha256:cd -->\r\n<!-- stay:l -->\r\n",
    ),
    (
        "tabs",
        "-\ttab gap <!-- stay:t1 subhash=sha256:1 -->\n"
        "\tcontinued with tab\n"
        "- spaces\n"
        "      six-space continuation <!-- stay:t2 subhash=sha256:2 -->\n",
    ),
    (
        "item-with-fence",
        "- [ ] run this:\n\n"
        "  ```sh\n  echo '<!-- stay:fake subhash=sha256:9 -->'\n  ```\n\n"
        "  then check. <!-- stay:f1 subhash=sha256:1 -->\n"
        "- [ ] second <!-- stay:f2 subhash=sha256:2 -->\n",
    ),
    (
        "unaddressed-child-markers",
        "A paragraph <!-- stay:u1 subhash=sha256:1 --> with a child marker.\n\n"
        "- item <!-- stay:u2 subhash=sha256:2 -->\n"
        "<!-- stay:u3 subhash=sha256:3 -->\n\n"
        "> - quoted <!-- stay:u4 subhash=sha256:4 -->\n",
    ),
    (
        "heading-marker-after-blank",
        "## Section\n\n<!-- stay:s1 hash=sha256:1 -->\n\nBody text.\n<!-- stay:b1 -->\n",
    ),
    (
        "marker-mid-item-and-second-paragraph",
        "- first para <!-- stay:m1 subhash=sha256:1 --> continues\n\n"
        "  second paragraph\n\n"
        "- another\n",
    ),
    (
        "unicode-and-indent",
        "  Indented paragraph naïve 世界 <!-- stay:p1 hash=sha256:1 -->\n\n"
        "- émoji 😀 item <!-- stay:e1 subhash=sha256:1 -->\n",
    ),
    (
        "duplicates-and-orphan",
        "<!-- stay:orph -->\n\nPara one.\n<!-- stay:dup -->\n\nPara two.\n<!-- stay:dup -->\n",
    ),
    (
        "table-rows-declined",
        "| k | v |\n| --- | --- |\n| a | b<!-- stay:r1 subhash=sha256:1 --> |\n"
        "<!-- stay:tbl hash=sha256:2 -->\n",
    ),
    (
        "loose-task-list-empty-item",
        "- [ ]\n\n- [ ] has text <!-- stay:k1 subhash=sha256:1 -->\n\n- [x] done\n",
    ),
]


# §5.4 says the two segmenters agree only on the agreement subset. Rivet digests
# under §5.2, while markstay's CLI (`lint`, `stamp`, `restamp`) and its
# pre-commit hook default to §5.1 blank-line segmentation. On a document outside
# the subset the same stay therefore has two correct digests, one per profile,
# and each tool reports the other's as drift. These vectors pin both values on
# the two shapes a tracker produces most, plus a control inside the subset where
# they coincide, so Rivet's digest is checked against the profile it claims and
# the divergence is a recorded fact rather than a suspected bug.
PROFILE_DOCS = [
    (
        # §5.4 cause 1: an ATX heading with no blank line above it. The
        # whole-list marker and the heading's own marker share one blank-line
        # run, so §5.1 binds both to the list-plus-heading block.
        "heading-glued-under-list-marker",
        "## Action\n<!-- stay:hAct -->\n\n"
        "- [ ] **Ship it.** <!-- stay:aOne subhash=sha256:000000000000 -->\n"
        "- [ ] **Test it.** <!-- stay:aTwo subhash=sha256:000000000000 -->\n"
        "<!-- stay:lAct hash=sha256:000000000000 -->\n"
        "## Baseline\n<!-- stay:hBase hash=sha256:000000000000 -->\n\n"
        "Baseline body.\n",
    ),
    (
        # §5.4 cause 2: a loose list. §5.1 binds the trailing marker to the
        # last item's run, §5.2 to the whole list.
        "loose-list-trailing-marker",
        "## Log\n<!-- stay:hLog -->\n\n"
        "- **2026-01-02** first entry. <!-- stay:eOne subhash=sha256:000000000000 -->\n\n"
        "- **2026-01-01** second entry. <!-- stay:eTwo subhash=sha256:000000000000 -->\n"
        "<!-- stay:lLog hash=sha256:000000000000 -->\n\n"
        "Trailing paragraph.\n",
    ),
    (
        # The same content written to §5.4's advice: a blank line before the
        # heading and a tight list. Both profiles agree on every digest.
        "agreement-subset-control",
        "## Action\n<!-- stay:hAct -->\n\n"
        "- [ ] **Ship it.** <!-- stay:aOne subhash=sha256:000000000000 -->\n"
        "- [ ] **Test it.** <!-- stay:aTwo subhash=sha256:000000000000 -->\n"
        "<!-- stay:lAct hash=sha256:000000000000 -->\n\n"
        "## Baseline\n<!-- stay:hBase hash=sha256:000000000000 -->\n\n"
        "Baseline body.\n",
    ),
]


def profile_digests(doc, mode):
    """Each block stay's digest under one segmenter, keyed by stay id."""
    out = {}
    for b in M.parse_document(doc, mode=mode, child_blocks=False):
        if b.index < 0:
            continue
        for m in b.markers:
            if m.id and not m.has_subhash and not m.malformed:
                out[m.id] = M.body_hash(b.content)
    return out


def profile_vector(name, doc):
    tree = profile_digests(doc, "commonmark")
    blank = profile_digests(doc, "blank-line")
    assert tree.keys() == blank.keys(), name
    stays = [
        {"id": i, "tree": tree[i], "blank_line": blank[i], "agrees": tree[i] == blank[i]}
        for i in tree
    ]
    return {"name": name, "doc": doc, "stays": stays}


def vector(name, doc):
    blocks = []
    for b in M.parse_document(doc, mode="commonmark", child_blocks=True):
        blocks.append(
            {
                "content": b.content,
                "line": b.line,
                "orphan": b.index < 0,
                "ids": [m.id for m in b.markers if m.id and not m.has_subhash and not m.malformed],
                "hash": M.body_hash(b.content) if b.index >= 0 else None,
                "children": [
                    {
                        "kind": c.kind,
                        "content": c.content,
                        "line": c.line,
                        "ordinal": c.ordinal,
                        "ids": [m.id for m in c.markers if m.id],
                        "subhash": M.body_hash(c.content),
                    }
                    for c in b.children
                ],
            }
        )
    return {"name": name, "doc": doc, "blocks": blocks}


# §3.4 carrier texts, answered by the markstay writer itself. v1.8 masks the
# content of a CLOSED inline code span before the scan, so a container prefix
# quoting `<repo>` in backticks no longer refuses the items after it. Every
# boundary the clause names gets a case, because each one is a way to mask too
# much and permit a carrier that changes what a renderer does.
CARRIERS = [
    ("plain-text", "- [ ] **Ship it.** "),
    ("bare-lt-refuses", "- [ ] Run against <repo> and report. "),
    ("closed-code-span-accepts", "- [ ] Run `markstay stamp` against a `<repo>` path. "),
    ("backslash-refuses", "- [ ] A carrier with a back\\slash. "),
    ("lt-outside-a-span-refuses", "- [ ] < and then `<repo>` after it. "),
    ("unclosed-run-masks-nothing", "- [ ] An unclosed ` span with <repo> after it. "),
    ("uneven-runs-stop-the-scan", "- [ ] a `\n- [ ] b ` and <repo> after. "),
    ("double-backtick-span", "- [ ] A ``span with ` and <repo>`` inside. "),
    ("span-spanning-two-items", "- [ ] first `open\n- [ ] second close` then <repo>. "),
    ("multi-item-prefix-accepts", "- [ ] `<a>` one.\n- [ ] `<b>` two.\n- [ ] three. "),
    ("fence-line-not-paired", "- [ ] item\n\n  ```\n  <html>\n  ```\n\n- [ ] after. "),
    ("crlf-prefix", "- [ ] `<repo>` one.\r\n- [ ] two. "),
    ("empty", ""),
    # Each of the five below fails a specific wrong implementation that every
    # vector above accepts. Without them the corpus cannot tell a faithful port
    # from one that pairs across lines, ignores fences, or keeps the spans it
    # found on a line it could not balance.
    ("cross-line-pairing", "- a `<repo>\n- b ` end "),
    ("continues-past-an-unbalanced-line", "- a `\n  b ` <!-- ` c "),
    ("tentative-spans-on-an-unbalanced-line", "- a `<repo>` and ` "),
    ("tilde-fence-is-not-scanned", "- a\n\n  ~~~\n  `<repo>`\n  ~~~\n\n- b "),
    ("bare-cr-line-endings", "- a\r  ~~~\r  `<repo>`\r  ~~~\r- b "),
    # Multi-byte content inside and around a span: the mask writes ASCII
    # spaces over byte ranges, so a character that straddles nothing still has
    # to come out of it intact enough for the scan to read what follows.
    ("multibyte-inside-a-span", "- [ ] `caf\u00e9 <x>` and caf\u00e9. "),
    ("multibyte-outside-a-span", "- [ ] caf\u00e9 `ok` then <x>. "),
    # A closer must be the SAME length as its opener, not merely as long: an
    # implementation that accepts a longer run pairs these two and masks the
    # `<repo>` between them.
    ("closer-longer-than-opener", "- [ ] a `<repo>`` "),
    ("closer-shorter-than-opener", "- [ ] a ``<repo>` "),
    # §3.3 recognises a fence with up to three leading spaces. At four it is an
    # indented code block instead, so its line is NOT in the fence set and its
    # backticks pair like any others.
    ("fence-indented-four-is-scanned", "- [ ] a\n\n    ~~~\n    `<repo>`\n    ~~~\n\n- [ ] b "),
]


def carrier_vector(name, text):
    """What the writer answers for this carrier, and why it is interesting.

    `accepts` is `plain_text_state` at a §5.5 child carrier (non-flush), which
    is the only position v1.8 changed. `presence_rule` is the same call with
    the mask disabled, so a vector records both the v1.8 answer and the v1.7
    one it replaced and a reader can see which cases actually moved.
    """
    scanned = M_STAMP._outside_markers(text, "html")
    presence = not any(c in scanned for c in M_STAMP.CARRIER_CAPTURING["html"])
    masked = M_STAMP._inert_code_spans(scanned)
    return {
        "name": name,
        "carrier_text": text,
        "accepts": M_STAMP.plain_text_state(text, "", "html", False),
        "presence_rule": presence,
        "masked": masked,
        "masked_bytes": _as_byte_mask(scanned, masked),
    }


def _as_byte_mask(original: str, masked: str) -> str:
    """`masked`, with each masked character widened to its UTF-8 byte length.

    markstay masks one space per code point; a Rust port masking byte ranges
    writes one per byte. Both refuse on the same characters, so the verdicts
    agree, but the strings only match while the carrier is ASCII. Widening here
    lets every vector be compared on its masked text rather than skipping the
    non-ASCII ones, which is where a port that used code-point offsets as byte
    indices would otherwise hide.
    """
    out = []
    for before, after in zip(original, masked):
        out.append(" " * len(before.encode()) if after != before else before)
    return "".join(out)


def main():
    revision = subprocess.run(
        ["git", "-C", str(REPO), "rev-parse", "HEAD"], capture_output=True, text=True, check=True
    ).stdout.strip()
    spec = hashlib.sha256((REPO / "SPEC.md").read_bytes()).hexdigest()
    out = {
        "note": "Generated by generate.py from the markstay Python reference "
        "(parse_document mode=commonmark, child_blocks=True). Do not edit by hand.",
        "markstay_revision": revision,
        "spec_sha256": spec,
        "vectors": [vector(name, doc) for name, doc in DOCS],
    }
    path = Path(__file__).with_name("reference-children.json")
    path.write_text(json.dumps(out, indent=2, ensure_ascii=False) + "\n")
    print(f"wrote {len(out['vectors'])} vectors to {path}")

    carriers = {
        "note": "Generated by generate.py from markstay's own writer "
        "(stamp.plain_text_state at a non-flush §5.5 child carrier). "
        "`presence_rule` is the pre-v1.8 answer, kept so a reader can see "
        "which cases the code-span mask actually moved. Do not edit by hand.",
        "markstay_revision": revision,
        "spec_sha256": spec,
        "vectors": [carrier_vector(name, text) for name, text in CARRIERS],
    }
    path = Path(__file__).with_name("carrier-state.json")
    path.write_text(json.dumps(carriers, indent=2, ensure_ascii=False) + "\n")
    moved = sum(1 for v in carriers["vectors"] if v["accepts"] != v["presence_rule"])
    print(f"wrote {len(carriers['vectors'])} carrier vectors to {path} ({moved} moved at v1.8)")

    profiles = {
        "note": "Generated by generate.py from the markstay Python reference. "
        "`tree` is each block stay's digest under §5.2 (parse_document "
        "mode=commonmark), `blank_line` under §5.1 (mode=blank-line), the "
        "default of markstay's CLI and pre-commit hook. Do not edit by hand.",
        "markstay_revision": revision,
        "spec_sha256": spec,
        "vectors": [profile_vector(name, doc) for name, doc in PROFILE_DOCS],
    }
    path = Path(__file__).with_name("profile-divergence.json")
    path.write_text(json.dumps(profiles, indent=2, ensure_ascii=False) + "\n")
    split = sum(1 for v in profiles["vectors"] for s in v["stays"] if not s["agrees"])
    print(f"wrote {len(profiles['vectors'])} profile vectors to {path} ({split} stays split)")


if __name__ == "__main__":
    main()

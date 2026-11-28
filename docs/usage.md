<!-- markdownlint-configure-file
{"MD013": {"code_blocks": false, "tables": false}}
-->

# Using Rivet

markrivet (Rivet for short) lets several agents update the same Markdown
trackers without losing each other's changes. It addresses headings and list
items by stable ids, checks every write against a revision of exactly what it
replaces, and rewrites only the bytes a write owns. The files stay ordinary
Markdown.

Everything on this page runs against the current CLI. The walkthrough in
[§ Tracker walkthrough](#tracker-walkthrough) is `examples/tracker/walkthrough.sh`,
which the test suite executes. Anything not implemented is listed under
[§ Not implemented](#not-implemented).

## Install

Linux is supported; CI builds and tests on Linux x86_64. macOS is not
supported yet: Rivet does not build for it, per-write commits and recovery
read Linux `/proc`, and its durability and path-identity guarantees are
unchecked on APFS. Windows is not supported (the code imports
`std::os::unix` unconditionally). The crates declare Rust 1.89 as their
minimum; per-write commits and history need git 2.36 or later.

Install from `github.com/markrivetmd/markrivet` with Cargo:

```sh
cargo install --locked --git https://github.com/markrivetmd/markrivet markrivet-cli
```

The crates are not on crates.io yet. From a checkout:

```sh
cargo install --path crates/markrivet-cli   # installs the `markrivet` binary
markrivet --help
markrivet --version                         # markrivet 0.1.0 <commit>[-dirty]
```

`--version` names the commit the binary was built from, with `-dirty` when
the build's sources (`Cargo.toml`, `Cargo.lock`, `crates/`) differed from it,
and `unknown` outside a git checkout.

The walkthrough script also needs `jq`.

## A project

A project is a directory with a `.rivet/` directory. Every path in a request
is relative to that root; commands find it by walking up from the current
directory, or take `--root DIR`. `DIR` is required, and an argument starting
with `-` in its place is refused as a missing value; reach a directory with
such a name as `./-name`.

```sh
markrivet --root . init   # creates .rivet/, .rivet/schemas/, .rivet/.gitignore
```

| Path | Tracked in git | Purpose |
| --- | --- | --- |
| `.rivet/project.json` | yes | collection bindings (typed items) |
| `.rivet/schemas/<id>.json` | yes | typed item schemas |
| `.rivet/lock` | no (`.rivet/.gitignore`) | the writer lock; never delete it while writers run |
| `.rivet/seq`, `.rivet/pending/`, `.rivet/commits.jsonl` | no | per-write commit bookkeeping (see [§ Git](#git)) |

`.rivet/.gitignore` lists those, and `/push.jsonl` for the push log
`save.sh` will keep. Run on an existing project, `init` appends any of the
five lines that is missing and leaves the rest of the file alone; `doctor`
reports a missing line as `rivet_gitignore`: informational while commits
are off, a warning under `per-write`, where the line keeps bookkeeping out
of git.

`init` writes only the engine's own ignore lines. Files under `.rivet/` written
by adopter tooling, such as `journal.jsonl` and `nudge.jsonl`, are the adopter's
responsibility to ignore. Add `/journal.jsonl` and `/nudge.jsonl` to
`.rivet/.gitignore` when your tooling writes them; `init` preserves those additions.

`examples/tracker/` is a complete project with two synthetic trackers.

## Protocol

Every command reads one JSON object on stdin and prints one JSON object on
stdout. The command names the operation; the request never carries `op`
(except inside a batch's `ops`). `--pretty` indents the output.

```sh
echo '{"path": "tracking/TRACKING_WIDGETS.md"}' | markrivet map
```

A document path has one spelling. Empty and `.` components are dropped
before anything binds, reads or reports the path, so
`tracking//TRACKING_WIDGETS.md`, `tracking/./TRACKING_WIDGETS.md` and
`./tracking/TRACKING_WIDGETS.md` all name `tracking/TRACKING_WIDGETS.md`, and
responses carry that form. `..` and absolute paths are refused with
`path_rejected`. This also applies to `query.paths`, `archive-mkdir.path`,
and both `archive-copy.source` and `archive-copy.path`. `doctor` and analytics
document filters are stricter: their `paths` must already be canonical.
Paths in delete receipts are immutable evidence and must already be canonical.

Every response has `ok` and `status`. A refused request carries `error` with
a stable `code` and a `message`. Doctor reports findings and coverage, including
incomplete diagnoses, in its versioned report instead.

| Exit | `status` | Meaning | Retry? |
| --- | --- | --- | --- |
| 0 | `read`, `applied`, `unchanged`, `preview` | done | n/a |
| 1 | `invalid` | the request could not be read (not JSON, unknown command, bad shape) | fix the request |
| 2 | `incomplete` (doctor), `refused` (other commands) | doctor retains partial evidence; other commands change nothing: conflict, bad selector, invalid content, lock timeout, outside edit, editor lock | safe after re-reading |
| 3 | `applied` with `durable: false`, or `uncertain` | the write may have landed | **never blindly**: re-read first |
| 4 | `read` (doctor only) | complete scan with errors, or warnings under `strict`; `ok: true` means the diagnosis ran | correct the reported findings |

Error codes: `invalid_request`, `path_rejected`, `not_found`,
`unsupported_syntax`, `invalid_content`, `missing_selector`,
`ambiguous_selector`, `match_not_found`, `ambiguous_match`, `duplicate_id`,
`guard_conflict`, `collection_conflict`, `invalid_schema`, `invalid_item`, `stamp_refused`,
`stale_cursor`, `lock_timeout`, `external_change`, `editor_lock`, `io`,
`commit_uncertain`, `limit_exceeded`, `already_exists`, `internal`, and in
commit mode `pending_unresolved`, `commit_refused` and `clear_lock_refused` (see
[§ Git](#git)). `query` with `ready` reports `unresolved_wait` in `diagnostics`.
Document diagnostics also use `orphan_marker`, `unaddressed_marker` and
`malformed_marker`.

Delete verification uses `invalid_delete_receipt` for invalid or noncanonical
receipt evidence, and `delete_receipt_too_large` for receipts over their budget.
Archive operations use `archive_copy_refused` when the configured archive or
copy provenance does not authorize the request. A stale prune policy snapshot
uses `policy_conflict`.

## Reading

### `doctor`: collection health and coverage

```sh
echo '{"collection":"actions"}' | markrivet --root examples/tracker --pretty doctor
echo '{"collection":"actions","paths":["tracking/TRACKING_WIDGETS.md"],"strict":true}' \
  | markrivet --root examples/tracker doctor
echo '{}' | markrivet --root examples/tracker doctor
```

`--root` names the project containing `.rivet`, not the document directory.
`collection` is optional; omission checks every configured collection. `paths`
is an optional nonempty array of exact project-relative Markdown paths. Each
path must use plain components and match at least one selected binding. With
`paths`, only bindings matching a requested path are checked; unrelated collections
and their schemas are outside the scope.
Unknown request fields, unknown collections, empty arrays and paths outside the
selected binding are invalid requests (exit 1). Duplicate identical paths count
once. A matching missing file remains a failed requested file (exit 2).

Doctor reads configuration and schema bytes once, then reads and parses each
selected file once, and, when a selected file cites a stay id, every other
collection and archive file once for the ids they carry. It never takes a writer
lock, stamps, repairs or creates files, journals, caches or temporary entries in
the project. Its source-only results have `consistency: "per_file"`; they are
not an atomic project snapshot or permission to write later without a fresh
guard.

The report has these fields:

| Field | Meaning |
| --- | --- |
| `report_version`, `root`, `scope` | Version 1, project root and requested collection/path/strict selectors. |
| `config_fingerprint`, `collections[].binding_fingerprint`, `collections[].schema_fingerprint` | SHA-256 fingerprints of config bytes, normalized binding JSON and schema bytes observed by this invocation; absent source has a null fingerprint. |
| `complete`, `ok`, `status`, `health`, `counts` | Complete diagnosis is `ok: true`, `status: "read"`, `counts: "exact"` and health `healthy`, `warning` or `unhealthy`. Incomplete diagnosis is `ok: false`, `status: "incomplete"`, health `unknown` and `counts: "lower_bound"`. |
| `summary`, `collections[].coverage` | Unique-file coverage globally, per-collection coverage and membership item counts, including valid/invalid, missing/unresolvable ids, excluded/unselected items and diagnostic counts by severity as `errors`, `warnings` and `informational`. |
| `unique_items` | Physical items keyed by path, list index and item index, independent of ids or overlapping membership; a file record counts once per path. |
| `files` | One result per selected physical file, with `status` `parsed` or `failed`, source revision when read, and membership results whose `heading` is `found`, `missing` or `ambiguous`, with member/excluded/unselected sections, or `not_applicable` with empty section lists for a file record. |
| `traversal_events` | Every walker observation for the checked bindings, in the same form as `analyze`'s `events`: pruned prefixes, policy exclusions, non-UTF-8 names with their reversible hex, and enumeration errors with their scope. |
| `items` | One detail per collection membership, with physical coordinates, id, checked state, validity, `addressing` (`resolvable`, `missing_id` or `unresolvable`) and structural readiness. No item body or extracted values. A file record's entry adds `kind: "file"` and has `list_index`, `item_index`, `id` and `checked` null, `line: 1` and `addressing: "resolvable"`; its `stamp`, `current_carrier` and `checkbox` readiness are `not_applicable`, and `rewrite_prerequisites` is `blocked` when the file has no frontmatter or it does not read. A record's only findings are `invalid_item` and its collection's `zero_files`. |
| `diagnostics` | Stable rule code, severity, collection, path, item id where available, source line/column, revision, message, next action and optional original-byte carrier blocker. A `stamp_blocked` finding carries the affected item's line and no column; a `carrier_blocked` finding carries the blocker's line and column and the affected marker's `id`. In both, `blocker` carries the blocker's own line, column and byte offset. |
| `carrier_groups` | Carrier failures grouped by path and original byte offset, with affected membership and physical-item counts. |
| `omitted_detail` | The report limit that stops detail retention and further scanning, or null. |

`files_discovered` counts every file the checked bindings select (with
`paths`, only bindings matching a requested path) before restricting to
requested files; `files_requested` and `files_selected` count unique requested
and selected paths. Successfully read/parsed and failed file counts remain
separate. Global `summary.items`, `valid`, `invalid`, excluded and unselected
counts, and diagnostic counts by severity, are membership counts, so overlapping
collections can increase them (a file-level marker or frontmatter finding
repeats once per collection that selects the file); `unique_items` and global
file counts do not increase with overlap. Citation findings belong to no
collection: they count once, in `summary` only. Invalid items are retained and
`items == valid + invalid`, including checked records. Doctor inspects the full
selected scope, without query paging or filters. Discovery uses the same
descriptor-confined walker as `query`, `find` and `analyze`. Doctor walks once
over every collection and archive binding, then derives selected collection
counts from the matching live files. Its raw traversal events cover that union.
The walker opens only directories a glob can reach and
records the rest as pruned. It discovers only `.md` and `.markdown` files a glob
matches: other files are not documents, since confined reads refuse them, so a
broad glob such as `records/**` passes over them without a finding. The
walker skips hidden entries, `target`, `node_modules`, symlinks and nonregular
entries, and counts nothing beneath an excluded directory or behind an
unfollowed symlink. A literal `files` or `archive` binding into a hidden,
`target` or `node_modules` entry is a `schema` error with incomplete diagnosis,
including an archive binding outside the selected collection because citation
scope includes every collection. A skipped entry that a checked glob would
otherwise match or descend into, such as a symlinked Markdown file or a symlinked directory under
the binding, is an informational `excluded_entry` finding; the project's own
`.rivet` directory and any `.git` entry never are. Explicit symlink paths are
refused by confined reads. Collection discovery for `query`, `find` and `status`
reports relevant exclusions as `path_rejected` diagnostics, and enumeration
failures as their underlying errors. `archive-copy` refuses such diagnostics in
its archive namespace before writing, since an excluded input could carry a
copied stay id.

A directory open, listing or entry-type failure that the walker cannot prove
unrelated to the bindings, an entry error with no known name, and a non-UTF-8
name that could be or contain a matching document make diagnosis incomplete
while retaining evidence from readable files. Non-UTF-8 names are matched on
their native bytes and never become lossy document paths. Strict `analyze`
uses the same gap rule: matching native names refuse a census, while proven
unrelated names and errors do not. Proven unrelated failures and pruned prefixes appear only
in `traversal_events`. Discovery stops at the walker's bounds of 10,000 matching
files, 10,000 pending directories and 160,000 events, which also makes diagnosis
incomplete. Doctor's limits apply to the whole collection/archive union; a
cutoff conservatively makes selected coverage incomplete even if archive files
exhaust the budget. Coverage gaps are decided before any file is read; informational
exclusions and `traversal_events` are retained after file detail, so a large
walk cannot displace file evidence from the response budget. Informational
exclusions still count toward the diagnostic limit, so more than 10,000 of them
leave the report incomplete after its files have been read. With `paths`,
discovery still covers the whole checked binding, so a gap elsewhere in it
bounds `files_discovered` while each requested file is read and reported as
usual.

| Severity (`severity` value) | Codes | Health/exit effect |
| --- | --- | --- |
| Warning (`warning`) | `missing_id` (stampable), `unaddressed_marker`, `unselected_subsection` (checkboxes in unselected descendant sections), `citation_unresolved`, `citation_ambiguous`, `citation_kind` (below), `receipt_log_size` (receipt log at least 128 MiB), `evidence_drift` and `evidence_unrefreshable` (below) | health `warning`, exit 0 by default or 4 under `strict`. |
| Error (`error`) | `invalid_item`, `missing_heading`, `ambiguous_heading`, `zero_files`, `unresolvable_id`, `orphan_marker`, `malformed_marker`, `duplicate_id`, `cross_file_duplicate_id` (below), `unsupported_syntax` (marker or document-wide write blocker), `stamp_blocked`, `carrier_blocked`, `rewrite_blocked` | complete health `unhealthy`, exit 4. Marker diagnostics describe the selected file, including identities outside membership. |
| Informational (`info`) | `excluded_subtree` (explicitly excluded subtrees with checkboxes), `excluded_entry` (a policy-excluded entry a checked glob would otherwise match or descend into), `unsupported_frontmatter` (frontmatter key reads and writes, and analytics metadata fields from that file, are unsupported; checkbox items and body writes are unaffected), `rivet_gitignore` (`.rivet/.gitignore` lacks one of the lines `init` writes), `citation_scope` (a collection or archive file outside the selection does not read or list, so a citation of an id only it carries reads as unresolved) | none; counts remain separate from inspected items. |
| Error (`error`), incomplete | `configuration`, `schema`, `enumeration` (in-scope or scope-unknown walker failures and walker limits), `malformed_name` (a non-UTF-8 name that could match), `file_read_or_parse`, `receipt_log_read` (receipt log metadata cannot be read safely), and report limits recorded in `omitted_detail` | health `unknown`, exit 2, retaining observed findings. This exit takes precedence over complete findings. |

**Citations.** `item:<id>` and `section:<id>` in a selected file's text are
citations of a stay id (`<id>` is letters, digits, `-` and `_`), and each must
name exactly one unit of its kind in the project: an item for `item:`, a
section for `section:`. Ids resolve across every collection's files and their
`archive` files, whatever `collection` or `paths` selects, found by the same
walker over every collection's globs. An orphan or unaddressed stay names no
unit but still carries its id. A live copy beside
an archive copy is the live one; two live copies, or a heading whose `{#id}`
and stay disagree, is `citation_ambiguous`. An id no file carries is
`citation_unresolved`, which also covers a citation into another project,
since Rivet cannot see that project from here; one naming the wrong kind is
`citation_kind`. A value followed by `/` or `#` (`item:connect-test#162`, a
Loom note anchor) is not a stay citation, a bare id in prose is not checked,
and stay markers, frontmatter and top-level code blocks are not read (a code
block inside a list item is). Each
finding carries the cited `id` and the citation's line and column.

**Identity across files.** A stay id carried in two or more selected live
files is `cross_file_duplicate_id`, one error per carrier with its line,
naming the other carriers; it belongs to no collection and counts once, in
`summary` only. Only the selected files are compared, so a `paths` request
checks across the requested files. Archive copies give way to a live one and
are never this finding. Doctor reads files one at a time, so a move
finishing during the scan can show its old source and new destination
together: run `commit-pending` if a move is pending, and recheck before
resolving which copy to keep.

**Evidence.** Every live stay in a selected file, not only collection
members, is checked against the evidence rule `archive-copy` and `move` apply,
and each one whose stored evidence would refuse them is a warning:

| Code | Meaning | `evidence` field |
| --- | --- | --- |
| `evidence_drift` | The stored digest is not a prefix of the unit's current digest, and a [`refresh`](#refresh) write can rewrite it. | `key` (`hash` or `subhash`), `stored`, `fresh` (the current digest at the stored precision), `owner` (`section`, `block`, `list` or `item`), `refreshable: true` |
| `evidence_unrefreshable` | The evidence refuses an archive or move, and `refresh` cannot rewrite it: drift on a marker with unsupported attributes, a duplicated id or a stored digest longer than 64 hex digits, drift in a document with a write blocker (an MDX-form marker), or an item stay without supported attributes or a stored `subhash`. The message says which. | the same fields, `stored` and `fresh` `null` when there is no readable stored digest, and `refreshable: false` |

The `evidence` field appears only on these two codes. Each finding carries the
stay's `id`, the marker's line and column, and the file revision; it belongs to
no collection, so it counts once, in `summary` only, like a citation finding.
`evidence_drift` names `refresh` in its next action; `evidence_unrefreshable`
does not. Unaddressed and orphaned stays have no evidence and keep their own
`unaddressed_marker` and `orphan_marker` codes, and a fenced copy of a marker is
content, never a finding. Exit 0 with these warnings does not mean a unit will
archive: other refusals (unaddressed stays, outside duplicates) still apply, and
the archive preview is the proof.

A present empty bound heading can be healthy with zero items. Zero matched files
or no configured collections cannot be healthy. A plain native marker is not
rejected based on who placed it.

Each item's `readiness` reports independent checks:

| Check | Meaning |
| --- | --- |
| `document_prerequisite` | `blocked` when the parser records a document-wide write blocker, including an MDX marker outside membership; otherwise `clear`. |
| `stamp` | Existing stamp placement eligibility, including paragraph carrier and original list prefix; `not_applicable` when the item already has an owned marker, or carries its own hand-written stay that stamp leaves to `adopt_stay` (its `missing_id` and `rewrite_blocked` findings then name `adopt_stay`). No id is minted. |
| `current_carrier` | Original list prefix before owned markers, `clear`/`blocked`, or `not_applicable` without an owned marker. |
| `rewrite_prerequisites` | Current foreign-marker ownership and unsupported own-marker attributes checked by whole-item replacement. |
| `checkbox`, `checkbox_field` | The schema field derived from `from: "checkbox"` and its one-byte target; `not_applicable` without that field, blocked by a document-wide write refusal. Addressing and validity remain independent prerequisites. |
| `typed_update`, `typed_update_reason` | `not_checked`: no replacement fields are supplied, so candidate rendering, provenance, read-back and final carrier verification do not run. This excluded operation does not fail strict. |

`clear` means no known blocker in that check, not a guarantee about arbitrary
replacement content. Foreign body markers and pre-existing carrier blockers do
not themselves prevent the one-byte checkbox path. General typed updates still
need normal guard, schema and candidate verification.

The fixed limits are 10,000 retained diagnostics and an 8 MiB serialized
response, including pretty JSON, root and scope metadata, file/item details and
messages. A streaming byte counter charges detail before retention with a
conservative whitespace allowance and envelope reserve. A limit can stop a scan
before 8 MiB is reached; oversized metadata is explicitly omitted and a final
serialized-size check bounds the envelope. A capped report never claims healthy.
Doctor exit 2 carries the partial report, unlike other commands' exit-2
refusals. Exit 3 is reserved for uncertain writes and is not a doctor outcome.

### `status`: git state against the tree

```json
{"paths": ["tracking/TRACKING_WIDGETS.md"]}
```

`paths` is optional; without it `status` reports every document a
collection binds, taken from HEAD, the index and the working tree together,
so a tracker deleted from the tree still appears. It is read-only: it takes
no Rivet lock, every git call runs with `GIT_OPTIONAL_LOCKS=0`, and it never
writes the index. One report pins one HEAD commit.

`complete` says whether path enumeration finished. With explicit `paths`, the
requested set is complete even if an individual path cannot be read. Without
`paths`, a relevant traversal cutoff, directory or entry failure, or a matching
non-UTF-8 name sets it to false. Policy exclusions such as hidden entries,
build directories and symlinks leave it true. Unavailable git enumeration or
bookkeeping diagnostics also leave the combined path list incomplete. Per-path
read, parse and git-detail errors do not change this field. A partial report
still returns exit 0 and keeps its existing paths and diagnostics.

A literal binding into a traversal-skipped entry is an `invalid_schema`
diagnostic. This configuration error does not change `complete`: it is not an
enumeration gap. The rejected collection does not supply path memberships.

`git` carries `version`, the pinned `head`, the `branch` HEAD names, and
`ineligible` when the repository as a whole blocks a commit (see
[§ Git](#git)). Each entry in `paths` carries:

| Field | Meaning |
| --- | --- |
| `collections` | the bindings that match the path |
| `base`, `ineligible` | the base state and the first ineligibility reason, as a write's preflight would see them |
| `staged` | the index entry differs from HEAD |
| `head`, `index` | the blob id and mode at HEAD, and the index entries (with `assume_unchanged` or `skip_worktree` when set) |
| `tree` | whether the file exists, its git mode and size |
| `read_at` | when the file was read, in milliseconds since the epoch; `git.read_at` stamps the repository facts |
| `units` | present only when HEAD and the tree differ |
| `pending` | the path's pending commit records: `op`, `seq`, `state`, intended `outcome`, `session`, and `repair` when one needs it |
| `last_write` | the receipt event of the path's last write: the highest sequence number among ops whose outcome has `applied: true` |
| `owner` | `pending:<session>` while a record is open, else `receipt:<session>` from `last_write`, else `unknown` |

`units` compares the two sides by stay id: `added`, `deleted`, `changed` (a
different revision) and `moved` (a different enclosing section, or out of
order among the units that share one, with `from` and `to` title paths).
`unstamped` lists collection items without a stay that are on one side only,
by `side` and `line`. `residual` appears when the bytes outside every unit
differ, such as frontmatter. `parse` carries a side that does not parse, and
`duplicate_ids` the ids that address more than one unit. A reorder that
keeps every revision still shows as `moved`.

Paths with a pending record are reported even when no collection binds
them. `git.locks` lists every lock file in `.git/` and under `.git/refs/`
with its `kind`, `identity` and `mtime`, and `record`: the op of the
pending record whose dead git child spawned before it, if any. `status`
never removes one. `receipts` totals the receipt log: `writes` and
`by_outcome` count ops whose outcome has `applied: true`,
`unknown_by_session` counts `applied: "unknown"` outcomes separately, and
`pending` counts records by state.

Outside a git work tree, `git.ineligible.reason` is `unsupported_layout` and
every path's `base` is `unknown`.

### `map`: outline, ids and revisions

```json
{"path": "tracking/TRACKING_WIDGETS.md", "limit": 200, "cursor": null, "max_depth": 6, "items": false}
```

Returns the document `revision`, frontmatter status, keys and writable keys,
and an
`outline` of sections, lists and other stamped blocks in document order.
Each entry has `kind`, `id` (or `null`), `line`, `end_line` and a `revision`.
`id` is the first of the unit's stay ids that selects it, so a duplicated
id is passed over for another the unit carries; a unit with none keeps its
first.
Lists report `items`, `checkbox_items` and `stamped_items`. `collections`
lists every bound collection this file matches, with its `revision`, and,
for a binding that takes subsections, the title path of each member
subsection under `subsections`. A file record's entry is `{name, schema,
item: "file", valid, errors, revision}`, where `errors` is null for a valid
record and `revision` is the record guard a writer needs.
`truncated` and `next_cursor` page the outline; a cursor from an older
revision is refused with `stale_cursor`. `map` never returns content.

With `"items": true` the outline also lists every direct list item, right
after its list, as `kind: "item"` with `id`, `select`, `addressing`,
`revision`, `line`, `end_line` and `checked` (on a task item), paged with
the rest. `addressing` says how the item is named (§ Selectors): `"id"` with
an id `select`, `"position"` with a position `select` and `id: null`, or
`"ambiguous"` with `select: null` and the refusal a position would get in
`diagnostics` beside the item's line. Items are opt-in because a tracker has
hundreds and `map`'s job is the shape; without `items` the outline is
unchanged.

### `find`: units holding a text

```json
{"text": "Refresh the fixtures", "path": "tracking/TRACKING_WIDGETS.md", "match_case": false, "limit": 20}
```

Searches one document, or without `path` every document a collection
binds, for literal text (no patterns). Each entry in `matches` names the
unit holding a match, so a caller writes without a `map` or a `get`: `path`,
`kind` (the unit: `item`, `list`, `block`, `section` or `document`), `id`, `select`
(the selector naming the unit, ready to pass as it is), `revision` (its
guard), `typed` (the typed guard per collection holding an item), the
section's `heading_path` (absent on a document entry), the first match's
`line`, a `count` and a one-line `excerpt`.

An entry's `kind` is not a selector kind; copy `select` whole instead of
building one. To change an item a collection holds, its `typed` guard goes
straight to `update-item`, with no `get` first:

```json
{"path": "tracking/TRACKING_WIDGETS.md", "collection": "actions", "id": "S26FetLP", "guard": "<typed.actions>", "set": {"done": true}}
```

`path`, `id` and the guard come from the entry, the collection from the
`typed` key.

A match inside a direct list item reports that item, stamped or not. A match
in a nested item reports the top-level item that owns it, and several matches
in one unit count once, not once per enclosing unit. An item entry carries
`addressing` as `map` reports it: an item no stay id selects (a duplicated id
does not count) has `id: null` and a position `select`, or, when no position
names it alone, `select: null` with `ambiguous_selector` and the line in
`diagnostics`. Other text reports the innermost block or section a stay id
addresses; with none around it, the innermost section with `id: null`, or
`kind: "document"` before the first heading or across sections. Its `select`
is then `{"kind": "heading_path", ...}` or `{"kind": "document"}` and its
`revision` the guard for that selector. A heading path that selects another
section too leaves `select` null and puts `ambiguous_selector` and the line
in `diagnostics`.

Matching runs on the source, case-insensitive (Unicode lowercase) unless
`match_case` is true. Markdown syntax is literal: `` `make check` `` matches
`make check` and `` `make check` `` but not `make check passes`. A whitespace
run, a soft line break and its indentation included, matches one space; no
match spans a blank line. Stay markers and frontmatter are not searched; a
malformed marker is text. `total` counts matched units and `truncated` says
`limit` cut the list: narrow the text or pass `path`. An unreadable `path` is
refused as `map` refuses it; without `path`, unreadable files are listed in
`diagnostics`.

### `get`: one unit and its guard

```json
{"path": "tracking/TRACKING_WIDGETS.md", "select": {"kind": "id", "id": "S26FetLP"}, "max_bytes": 65536}
```

Returns `target` (`kind`, `id`, lines), `revision` (the guard for writing
this unit), and `text`, the exact source bytes. `target.id` is chosen as
`map` chooses an entry's `id`. An item's `target` also carries `select` and
`addressing`, as `map` with `items` reports them, so a `get` by position on
a stamped item answers with its id. Items also return `content`,
the editable form a `set` accepts, `checked`, and `typed` fields with the
typed guard for every collection holding the item (plus its `subsection`
where the binding takes subsections). Sections return `title`,
`level` and `body`. A target larger than `max_bytes` (default 64 KiB) is
refused with `limit_exceeded`, never truncated.

### `query`: typed items across files

```json
{"collection": "actions",
 "where": [{"field": "done", "op": "eq", "value": false},
           {"field": "actor", "op": "in", "value": ["agent", "human:sam"]}],
 "fields": ["title", "gate"], "paths": null, "limit": 50, "cursor": null}
```

Scans every file the collection's globs match, in path order and then document
order. Each item returns `path`, `id`, `select`, `addressing`, `line`,
`revision` (its typed guard) and projected `fields`. `id`, `select` and
`addressing` follow `map`: an unstamped item has `id: null` and a position
`select`, and one no position can name alone has `select: null` and
`ambiguous_selector` in `diagnostics`. A binding that takes subsections also returns `subsection`:
the titles from below the bound section down to the one holding the item, `[]`
for the bound section's own body. The guard for inserting into a file's
collection comes from `map` (`collections[].revision`) or the previous typed
write (`collection_revision`). Operators: `eq`, `ne`, `in`, `lt`, `lte`, `gt`,
`gte` (strings, dates and numbers; timestamps by instant), `exists`, `missing`,
`contains` (strings, and a list's elements).
The pseudo-field `id` selects by stay id. A clause whose value does not fit the
field's type is refused (a date must be a real calendar date, an enum value one
of its `values`); nothing is coerced. Numbers compare exactly, at any size.

`consistency` is `per_file`: each file is read once, but the scan is not a
snapshot across files. A continuation cursor is refused with `stale_cursor`
if any scanned file, the schema or the collection binding changed; a file
that reads but does not decode or parse guards it by its bytes. Invalid
items, items no selector names, unreadable files and unlistable directories
appear in `diagnostics`; they never vanish silently. An unstamped item with a
position `select` is not a diagnostic.

On a collection of file records (§ File records) each item is `{path,
revision, fields}`, with `revision` the record guard and no `id`, `select`,
`addressing` or `line`, and the response carries `"item": "file"`. `ready`, the
`id` pseudo-field and `id` in `fields` are refused with `invalid_request`, and
so is a `paths` entry the collection does not select, an excluded file
included. An invalid record is a diagnostic, as an invalid item is.

#### Ready work

`ready` names the schema's wait field (a `string` field with no `default`,
which would make an absent label read as written; one with a default is
refused with `invalid_request`) and turns the query into a readiness read of
open items:

```json
{"collection": "actions", "ready": {"field": "waits"}}
```

Each open (unchecked) item that passes `where` gets one readiness, read from
its wait value: one word, optionally followed by one parenthesised note.

| Readiness | Wait value |
| --- | --- |
| `ready` | `none`, or `item:<id>` naming a checked task item |
| `waiting` | `item:<id>` naming an unchecked task item |
| `condition` | anything else (`human:sam`, `2026-11-01`, prose): Rivet cannot see it close |
| `unlabelled` | no wait label, or an empty one; nothing is ready by default |
| `unresolved` | an `item:` wait Rivet cannot follow, with a `reason` below |

`<id>` is a stay id. It is looked up in every file of the collection and of
its `archive` binding (see [§ Typed collections](#typed-collections)), whatever
`paths` lists, so a wait on an item that was pruned into an archive file still
resolves. A live carrier takes precedence over archive copies, including during
the interval between copying an item into an archive and deletion. Multiple live
carriers, or multiple archive carriers with no live carrier, are ambiguous. Carrier selection
precedes task-state checks, so a live non-task carrier cannot fall back to an
archived task. Any task item carrying the id counts, member or not. The reasons an
`item:` wait is `unresolved`: `dangling` (no scanned file carries the id),
`ambiguous` (more than one unit does, or a heading's `{#id}` and stay
disagree over it), `not_an_item` (the id names a section,
a block or a list item that is not a task, such as a `[-]` one, which
CommonMark reads as text), `cycle` (following open blockers leads back to the
item; an item that only leads into another item's loop stays `waiting`; only
collection members' waits are followed, so a loop through a blocker outside
the collection reads as `waiting` on that blocker) and
`malformed` (the value starts `item:` but is not `item:<id>`). Each unresolved
item is also an `unresolved_wait` diagnostic with its path, line and id; none
is guessed, and the read itself succeeds. No clock is read: a passed date is
still a `condition`.

The response lists `ready` items only, each with `wait`: its `readiness`, raw
`value`, and for an `item:` wait that resolved `on` (the blocker's `id`, `path`,
`line`, `done` and `archived`). `include` lists other classes too
(`"include": ["ready", "waiting"]`). `readiness` counts every class over the
open items `where` and `paths` let through, and `resolved_from` is the number of
files read and parsed only to resolve waits. Done items are never listed. A
cursor goes stale when any scanned file changes, archive files included. A file
read only to resolve waits that cannot be read, decoded or parsed is a
diagnostic and resolves nothing. Invalid items and unreadable bound sections are
reported for listed files only, as without `ready`; `doctor` reads archive files
only for the stay ids its citation check resolves.

```json
{"items": [{"path": "tracking/TRACKING_ALPHA.md", "id": "<id>", "fields": {"...": "..."},
            "wait": {"readiness": "ready", "value": "item:<audit id>",
                     "on": {"id": "<audit id>", "path": "archive/2026-09/ALPHA_CLOSED_ITEMS_2026-09.md",
                            "line": 4, "done": true, "archived": true}}}],
 "readiness": {"ready": 3, "waiting": 1, "condition": 1, "unlabelled": 1, "unresolved": 0},
 "resolved_from": 1}
```

### `inspect`: parser view

```json
{"source": "# Title\n\n- [ ] item\n"}
```

Needs no project. Reports blocks, sections and frontmatter scalars with
half-open UTF-8 byte spans into the given source, and exits 0 with any
diagnostics listed, as `map` does. These are **inspection
spans**, not mutation targets: nested extents keep container prefixes, and
GFM tables appear here although identity treats them as paragraphs.

## Selectors

| Selector | Resolves to |
| --- | --- |
| `{"kind": "id", "id": "..."}` | the unit a stay marker names: a heading's stay is its section, a list item's child stay is the item, any other stay is its top-level block |
| `{"kind": "heading_path", "path": ["Widgets", "Log"]}` | the section whose heading titles end with this path; a suffix is enough, more than one match is `ambiguous_selector` |
| `{"kind": "position", "heading_path": ["Action"], "index": 0}` | a direct list item by place: the `index`-th (zero-based) direct item of the top-level lists in the section's own body, counted across those lists as one sequence; `heading_path` matches as above, and absent or `null` names the preamble before the first heading |
| `{"kind": "frontmatter_key", "key": "status"}` | one top-level frontmatter key |
| `{"kind": "collections"}` | stamp only: every section, and list items where a bound collection holds them (its member sections) |
| `{"kind": "document"}` | the whole file (stamp scope) |

Heading paths match source titles exactly, after the ATX markers, a closing
sequence and a trailing `{#id}` are removed. Ids are unique per document; a
duplicated id resolves to `duplicate_id` and a missing one to
`missing_selector`. Rivet never guesses a target.

A position names an item with no stay, or one with, by where it sits. Its
section's own body runs from the heading to the first subsection, so an item
in a subsection counts in the subsection only. Nested items are part of their
top-level item and lists inside a block quote are not counted, as with
stays. `index` is a non-negative integer; a negative or fractional one fails
request parsing. An empty `heading_path` is `invalid_request`, an index past
the section's items `missing_selector`, and a path more than one section ends
with `ambiguous_selector`. An item whose bytes equal another direct item's
anywhere in the document is refused with `ambiguous_selector` naming the
other copy's line: place and bytes cannot tell the two apart, so stamp the
file to address either. Handles Rivet returns carry the section's full
heading path and are copied whole, like `find`'s `select`. `get`, `set` (plain
and with `collection`) and `update-item` take a position; every other field
that takes a selector refuses one with `invalid_request` (§ `delete`).

## Revisions and guards

Every write names a `guard`: the revision of the unit it changes, as returned by
`map`, `get`, `find`, `query` or the previous write. Revisions are SHA-256 over
the unit's current bytes with a versioned domain and length framing, truncated
to 128 bits: `rivet:v1:<kind>:<32 hex digits>`. A revision detects concurrent
change, and every one a caller holds is echoed back by every read, so the width
is set by accidental collision between one unit's versions rather than by
second-preimage resistance.

| Kind | Covers | Returned by |
| --- | --- | --- |
| `document` | the whole file | `map`, `find`, `write` |
| `section` | heading, its marker lines and body, including subsections | `map`, `get`, `find`, `write` |
| `block`, `list` | a top-level block and its marker lines | `map`, `get`, `find`, `write` |
| `item` | one direct list item's lines | `get`, `find`, `write` |
| `frontmatter` | one top-level key's source lines, including its mapping or list | `get`, `write` |
| `action` | one typed item, plus the collection name, canonical binding fingerprint and schema digest | `get` and `find` (`typed`), `query`, typed writes |
| `collection` | a collection's section heading and own body, plus its binding fingerprint and schema digest; with `subsections`, each member section's heading and own body | `map`, typed writes |
| `record` | a file record's frontmatter block, opening fence through the closing fence's line terminator (BOM excluded, empty without frontmatter), identified by the record's path, plus the collection name, binding fingerprint and schema digest | `map`, `query` |

Typed item and collection revisions include one versioned binding fingerprint:
SHA-256 of compact JSON `["rivet-binding-v1", schema, files, section, level,
subsections, exclude, archive]`. The schema key includes its version. Set-valued
`files`, `exclude` and `archive` are sorted and deduplicated; defaults are
explicit. Titles and glob strings retain their exact spelling. Changing a
binding setting conflicts even when the item remains a member. Schema-file
bytes retain their separate digest. Guards from engines predating this
fingerprint refuse `guard_conflict` on an unchanged valid binding; re-read
after upgrading. Native guards retain their byte-identical contract.

A file binding has its own fingerprint, SHA-256 of compact JSON
`["rivet-file-binding-v1", schema, files, exclude_files]` with both lists
sorted and deduplicated, so adding one leaves every checkbox guard as it was. A
`record` guard does not conflict on a body edit, and conflicts on any
frontmatter change: a key outside the schema, a comment line, or the binding or
schema changing.

A section guard covers its descendants, so editing an item conflicts with a
stale guard on any section above it. It never conflicts with a guard on a
sibling item or on a section that does not contain it. A guard of the wrong kind
is refused with a message naming both kinds. Stored markstay `hash=` evidence is
never a guard: a hand edit that did not refresh a marker still changes the
revision.

A conflict response carries the target's `current` revision, except on an item
named by position (below):

```json
{"ok": false, "status": "refused",
 "error": {"code": "guard_conflict", "message": "op 0: target changed since the guard was read",
           "current": "rivet:v1:action:d97b..."}}
```

Recover by reading the unit again, deciding whether the change still makes
sense, and retrying with the fresh guard. A successful `set`, a stamped
`insert` and every typed write return the next guard in `ops[n].revision`
(typed item writes also return `collection_revision`; a record write returns
the record's next `record` revision and no collection revision), so a
follow-up edit needs no re-read. `write` always returns the new document `revision`.

A write through a position handle (§ Selectors) is checked the same way: under
the writer lock, the item at that place must still have the guarded revision
and no other direct item may have its bytes. An insert, delete or reorder
above the item puts another item at its place, whose bytes differ, so the
write is refused rather than applied to that item; an edit to the item itself
(a tick, a stamp) changes its bytes and is refused too. Changes below it or in
another section leave its place and bytes alone, and the write applies. Its
conflict carries no `current`, since the revision now at that place may be
another item's, and copying it into a retry of the same handle would write
that item. When exactly one direct item in the file still has the guarded
revision, the conflict names where it went:

```json
{"ok": false, "status": "refused",
 "error": {"code": "guard_conflict",
           "message": "op 0: the item at this position changed since the guard was read (positional handle; re-read)",
           "relocated": {"select": {"kind": "position", "heading_path": ["Widgets", "Action"], "index": 3}}}}
```

`relocated` is a hint the engine never applies itself. Retrying with its
`select` and the same guard is checked again from scratch under a fresh lock.
A positional write's response adds `ops[n].select` and `ops[n].addressing` beside
the next guard. Both are computed from the written file, not copied from the
request, so another op in the same batch that shifts the item (an
`insert_item` into an earlier list of its section, a new title on its heading)
is reflected: the returned handle names the item where it now is.

The guard covers the item's span and nothing else, for ids and positions
alike. One byte can sit outside every span: a final line's indentation that
the parser consumes when it has no line terminator (`docs/parser-contract.md`
§ Known limitation). Removing it changes the document revision but not the
item's, so the write still applies, and keeps that byte as every write does.

An unstamped item is its bytes at its place. If it is removed and an identical
item is put back at the same place between the read and the write, the write
applies to that item: bytes and place cannot tell the two apart, as a section
deleted and recreated byte-identical under the same heading path accepts its
old guard. Telling two lifetimes of identical text apart is what a stay is
for.

A write answers with the guards a caller needs next and the news it could not
predict. It does not echo the preimage revision, which is the guard the caller
just sent, and it does not list the digests it refreshed as a matter of course:
`evidence_refreshed` counts those. `drift` appears only when a stored digest
was **already** stale before this write, and lists each one.

## Writing

`write` takes a single-file batch:

```json
{"path": "tracking/TRACKING_WIDGETS.md", "preview": false, "lock_timeout_ms": 10000,
 "message": "set XqnPhqCs: ship it", "session": "s1", "ops": [ ... ]}
```

`set`, `insert`, `delete`, `stamp`, `update-item`, `insert-item`, `append`,
`add-key`, `set-key-field`, `delete-key`, `append-key-item`, `rejoin`, `adopt-stay`,
`replace-in-section` and `remove-paragraph-in-section` are the same request
with one op: put the op's fields beside `path`. `message` and `session` matter
only in commit mode ([§ Git](#git)); a message line using the trailer keys
`Rivet-Op` or `Includes-outside-edit` is refused as `invalid_request` either
way.

All ops in a batch resolve and check their guards against one locked read of
the file. Ops that touch the same unit, overlap, or rewrite a list another op
edits are refused. Adding or deleting an item also excludes every other op
on that list. Edits to different items of one list and edits in different
sections combine freely.
`preview: true` plans and verifies without taking the lock or writing, and
reports what would be minted, dropped and refreshed. A batch whose result
equals the file returns `unchanged` and writes nothing.

### `set`

```json
{"op": "set", "select": {...}, "guard": "...", "content": ..., "part": null, "allow_drop": false}
```

| Target | `content` | Replaces |
| --- | --- | --- |
| item | editable item text, as `get` returns it | the whole item; its stay moves to the end of the new last paragraph |
| section, `part: "title"` | one line | the ATX heading title only (setext titles are refused) |
| section, `part: "body"` | Markdown | everything after the heading's marker lines, subsections included |
| other block | Markdown | the block; its own stay stays attached |
| frontmatter key | a JSON string, number, boolean or null | the value, keeping its quoting where it still means the same thing (read-only keys are refused) |

Item text is relative to the item's content column: `get` strips exactly the
item's width from each continuation line, and a new line is written back at
that width. A line the write repeats keeps its own indentation, so a tab, or a
lazy continuation at column 0, survives an edit to the lines around it. The
exception is a lazy line that would no longer read as part of the item, for
instance after a blank line the new content added, which is written at the
width instead. The typed `update-item` writes through this same path.

Content may not contain stay markers: Rivet places and preserves them.
Marker-shaped text wholly inside a code span or code block is content, as it is
when Rivet reads the file, so an item that quotes a marker in backticks can
still be edited, and so is an opening like `<!-- stay:` that no markstay reader
takes for a marker; either one outside code is refused with `invalid_content`.
So is text that reads as no marker on its own but goes live where the write puts
it (a tab stop that shifts under an item's indent, a leading BOM, a title that
would otherwise read as a fence): a marker inside text a write places must be
one Rivet minted or an existing stay it moved, even when its id already exists
elsewhere. Replacing a section body or a block that holds other stays is refused
unless `allow_drop` is true; the response lists them under `dropped`. A marker
Rivet does not address counts as a stay here, a nested item's or a table row's
included, since markstay's drop check follows its id. In a section body, one
whose id another marker outside the region still carries does not leave the
document and is not listed; a block counts every marker it does not own. An item
or a title holding a marker it does not own is refused with `unsupported_syntax`
whatever `allow_drop` says. A batch whose ops between them remove every live
marker carrying an id that none of them declared is refused. A dropped id can
never name new content in the same write. A body may not introduce a heading at
or above its section's level.

A body writer can require typed membership and schema validity as well as the
native body guard:

```json
{"op":"set","select":{"kind":"id","id":"<item stay>"},"guard":"<native item revision>","collection":"tasks","typed_guard":"<typed action revision>","content":"[x] **One.** Changed."}
```

`collection` and `typed_guard` are required together. This mode accepts one
list item, named by its stay id or by a position handle, string content and no
section `part`. Both original guards and
membership are checked against the locked preimage; the resulting item must
extract without diagnostics under that binding's schema before publication.
A lost membership refuses `collection_conflict`; changed bytes, schema or
binding refuse `guard_conflict`. Invalid candidate fields refuse `invalid_item`.
The returned `ops[n].revision` remains native; use `get` for the next typed
guard. Through a position handle the item needs no stay, and a conflict
follows § Revisions and guards (no `current`, an optional `relocated`). Plain `set` keeps its existing behavior. These shapes and refusals are
exercised by `crates/markrivet-cli/tests/bound_body.rs`.

### `insert`

```json
{"op": "insert", "select": {...}, "position": "start", "guard": "...", "content": "...", "stamp": true}
```

Targeting a list's stay inserts one item (editable item text) at the start or
end, copying the neighbouring item's marker, numbering and blank-line
spacing. Targeting a section inserts one child section: `content` must start
with a heading one level deeper. `start` puts it before the first existing
subsection, which is how a newest-first log grows; `end` puts it last.
`stamp: true` mints a stay for the new unit and returns it under `minted`.
The guard is the list's or section's revision, so two agents appending to one
collection conflict rather than interleave.

### `delete`

```json
{"op": "delete", "select": {...}, "guard": "..."}
```

A position handle is refused with `invalid_request`: deletion names its item
by stay id, which a delete receipt can carry and replay. To remove an
unstamped item, `set` its section's body under the section guard, or stamp
the file first. `insert`, `append`, `replace_in_section`,
`remove_paragraph_in_section`, `rejoin` and `stamp` refuse a position the
same way, before any other check.

Removes an item (with its separating blank line), a section with its
subsections, or a block with its marker lines. The response lists every stay
removed under `dropped`. A section or block delete includes the ids of markers
Rivet does not address unless another copy of the id survives elsewhere in the
file; an item holding a marker it does not own is refused with
`unsupported_syntax`. Separating whitespace outside the target's guard may be
removed, but any non-whitespace there (such as a link reference definition
following a block) refuses with `unsupported_syntax` and leaves the file
unchanged. Deleting the only item requires explicit list authority:

```json
{"op":"delete","select":{"kind":"id","id":"<item stay>"},"guard":"<item revision>","list":{"guard":"<list revision>"}}
```

This schema illustration uses placeholder guards; the CLI tests exercise it in
a per-write/refuse project. The item remains the selected target. The writer
checks its guard and foreign markers, requires exactly one item, then checks
the containing list's native revision. It removes the complete list and its
trailing marker-only lines, plus separating whitespace, declaring both item
and list stays under `dropped`. A stale list guard returns `guard_conflict`;
a non-item target or a list with siblings returns `invalid_request`.
A trailer marker that does not attach to that list returns
`list_trailer_refused`, naming its marker and line. Nested foreign markers
retain `unsupported_syntax`.

List authority implies `require_commit` and requires exactly one delete.
Non-preview writes require a clean eligible per-write/refuse commit; otherwise
they refuse `commit_refused` before publication. Previews also work without
Git. Omitting `list` refuses a sole item, including with `archive`,
`policy_guard` or `collection`.

Before deleting a list, the engine refuses `list_citation_refused` when any
list-owned stay is mentioned in any collection or section-archive source or
archive file. The check uses the candidate for the target file, even outside
all configured globs, so a mention removed with the item does not count.
An existing archive copy's mention does count. The walk must be complete:
configuration, relevant traversal, read and parse failures use the same code,
retaining the underlying diagnostic code and path in the detail. A citation
refusal names its path and line. Another marker carrying the list id does not
excuse a citation. Ids owned only by the item keep the ordinary warning behavior; an alias also
owned by the list is checked as a list id.
The check runs at planning, including preview, at publication under the writer
lock, and before recovery of an uncommitted list delete. Portable receipt
replay and archive-copy do not run this live-project check.

An optional `"collection": "actions"` on the delete op requires its target to
be a checkbox item of that bound queue, using the same section, descendant and
exclusion rules as `query`. The writer checks the file binding and membership
under the project lock after rereading, alongside the native `guard`. A target
outside that queue (including a section or block), a missing queue section, or
a file no longer bound to that collection refuses with `collection_conflict`
and leaves the file unchanged. Unknown collection names also refuse with
`collection_conflict`; ambiguous queue headings retain `ambiguous_selector`.
Previews check the same condition against their unlocked snapshot. Omit
`collection` to retain ordinary structural deletion. In a `write` batch, put
`collection` inside each delete op that needs the check. This precondition is
not included in `Rivet-Delete`: the receipt and `verify-delete` prove the
structural byte edit without loading project configuration.

A stay id the write drops that no collection or archive file still carries
(after the write, previews included) leaves its citations dangling. The
response lists each remaining mention under `dangling_citations`, as `id`,
`path` and `line`. Unlike `doctor`, this searches bare ids too, in any form
(`ItemOne1`, `` `ItemOne1` ``, `item:ItemOne1`): a random stay id has almost
no false hits once the id is known. A token running on into `/` or `#` is
still another scheme's. It is a warning, not a refusal: the write
lands as it would have. The same applies to a `set` with `allow_drop`.
The citation walk runs after the writer lock is released. It uses this write's
candidate for the edited file, even if another writer has since changed that
file; other collection and archive files are read during the walk.

### Committed deletion receipts and `verify-delete`

A clean, eligible single-delete commit carries a `delete_receipt`
in its pending Record and outcome Event, and exactly one reserved
`Rivet-Delete: <base64url>` commit trailer. The decoded canonical JSON binds
the transaction op, canonical path, actual parent, base/candidate blob ids
and modes, both SHA-256 byte hashes, the original native selector and guard,
and the exact dropped ids. It is structural evidence, not a signature of a
human's approval. Mixed batches, dirty-source adoption, `set allow_drop`,
previews, unchanged writes and commit mode off carry no delete receipt.

The complete canonical decoded receipt is limited to 65,536 bytes. An
otherwise receipted deletion above that limit refuses with
`delete_receipt_too_large` before replacing the document or changing Git.
Receipts are never truncated or silently downgraded.
Their paths exclude control characters. A clean committed single-delete of
such a filename refuses with `path_rejected`; it does not silently omit its
receipt. Legacy unreceipted modes retain their existing path behavior.

`verify-delete` reads this object on stdin and needs no project:

```json
{"parent":"<original UTF-8 document>","candidate":"<complete resulting UTF-8 document>","receipt":{"version":1,"op":"<transaction op>","path":"tracking/example.md","parent":"<full parent oid>","base":{"oid":"<base blob oid>","mode":"100644"},"candidate":{"oid":"<candidate blob oid>","mode":"100644"},"read_sha256":"<sha256>","candidate_sha256":"<sha256>","operation":{"op":"delete","select":{"kind":"id","id":"<item id>"},"guard":"<native item revision>","dropped":["<item id>"]}}}
```

This schema illustration is not runnable with placeholder identities. The
CLI receipt tests produce, decode and replay real transactions, including
receipts from a fresh clone. Success returns `status: "verified"`,
the receipt's `version` and only the verified `dropped` ids. Replay uses Rivet's guarded
planner and complete-candidate verification, including evidence refreshes.
Changed prose or an additional deletion fails exact byte comparison.
Receipts without list authority retain their version-1 bytes. A delete with
`list` carries version 2 and includes the list guard in the operation,
in this canonical order:

```json
{"op":"delete","select":{"kind":"id","id":"<item stay>"},"guard":"<item revision>","list":{"guard":"<list revision>"},"dropped":["<item stay>","<list stay>"]}
```

This is the operation inside a version-2 receipt, illustrated with placeholders.
Version 1 with `list`, version 2 without it, and unknown versions refuse
`invalid_delete_receipt`. Older verifiers refuse version 2. Replay remains
pure: later citations do not invalidate this structural evidence.
The caller independently checks the enclosing raw Git commit, its parent,
one-path diff, regular file modes, blob identities and path. The verifier
does not open receipt-named paths or run commit-controlled tools.

Only this command accepts up to 100 MiB of raw JSON, allowing two escaped
8 MiB UTF-8 documents. Each decoded document stays limited to 8 MiB and
canonical decoded receipt JSON to 65,536 bytes. The combined non-document
framing and metadata budget, including the receipt's escaped transport
representation, is 1 MiB. Unknown fields, duplicate fields,
duplicate dropped ids and unsupported versions refuse. Other commands keep
their 16 MiB request limit. `Rivet-Delete` and `Rivet-Archive`, like
`Rivet-Op`, are forbidden in caller messages, including case, whitespace,
equals-delimited and multiline injection variants.

### `move`

`move` transfers one native, stayed checkbox item or section subtree between two existing live
documents. It requires per-write commits with `on_dirty: refuse`. Both paths
must already be canonical, distinct, be selected by a collection (its `files`
match and no `exclude_files` glob does; § File records) or match a
section-history glob, and match no archive glob. Recovery checks both paths
again.

```json
{"source":"tracking/TRACKING_WIDGETS.md","id":"<stay id>",
 "guard":"<get.revision>","collection":"actions",
 "typed_guard":"<get.typed.actions.revision>",
 "destination":"tracking/TRACKING_DOCS.md"}
```

Walkthrough step 8 executes this request with real guards and replays it to
verify the same commit and op. Optional `under` names
one exact, unique included member title in the destination collection.
Omitting it selects the bound section's own body. `source_parent`,
`destination_parent` and `before` apply only to sections and refuse
`invalid_request` on items. `lock_timeout_ms`, `message` and `session` work as
for ordinary writes; reserved Rivet trailers cannot appear in `message`.

Under the project lock, the writer settles pending ownership for both paths,
loads one configuration snapshot and looks for the source identity. If present,
it loads all matching collection bindings, and both files must be clean and
eligible, with no write blocker or lingering Git lock, and use the same newline
style. The native guard, source membership and binding-aware typed guard must
still match. Imported item stays require plain supported attributes and valid,
fresh child evidence. The unit must own its stays, have no outside
identity claim or unaddressed marker, and contain no link reference definition.
The sole item of a list refuses pending 101 E1; a last open item with completed
neighbours is supported. Every carried identity, including aliases, must be
absent from the destination and every configured collection or section-history
archive. The destination must bind the requested collection and include the
selected member after exclusions.

Insertion uses the last collection-item list in that member's own body,
immediately after its last item and before its trailing list stay. Existing
item bytes and identities are imported exactly; they are never rendered again.
An empty member receives a new list with one fresh list stay. Only that list
stay is minted. Both candidates pass ordinary edit verification, typed-field
equality and membership checks for every matching binding. Existing lists must
keep their ordered/bullet kind, marker class and tightness; a split or merge
refuses. Each candidate is limited to 8 MiB.

A section request uses its own heading stay and native subtree guard. Both
parents name stamped sections: `source_parent` is the direct source parent;
`destination_parent` is exactly one heading level above the moved section.
Item fields (`collection`, `typed_guard`, `under`) refuse `invalid_request`.

```json
{"source":"tracking/TRACKING_WIDGETS.md","id":"<Notes heading stay>",
 "guard":"<Notes native revision>","source_parent":"<Widgets heading stay>",
 "destination":"tracking/TRACKING_DOCS.md",
 "destination_parent":"<Documentation heading stay>","before":"<Log heading stay>"}
```

Walkthrough step 9 executes this request and verifies its replay and heading
level. The section lands at the destination
parent's subtree end, or immediately before the heading line named by
`before`, which must be an existing direct child section of that parent.
No heading is relevelled. A blank line precedes the content extent and one
separator newline follows its original final terminator; separators stay
outside the extent. Only trailing unowned physical blank lines are excluded.
The exact content bytes, every carried id and each marker's relative owner
must survive in the candidate. An unterminated final line or a terminal
unclosed fence or HTML block refuses `not_native`.

A level-1 title, collection queue or member section, or a subtree containing
one, refuses `protected_unit`. A same-title direct sibling refuses
`sibling_title`; a parent at the wrong level refuses `parent_level`.
`Log` is an ordinary title to the engine; a store wrapper may protect it.
For every binding, the existing collection instance, member sections and
items retain membership. Each section and checkbox item in the moved
subtree must belong to the same collection set after the move, including
exclusions inherited from ancestors. An excluded Rule candidates subtree
can move beneath another Action parent while its records stay unbound.
Moving Details out from under excluded Plans into a collection member
refuses `membership_changed`.

All planning refusals leave both documents, HEAD, index, receipt log and pending
records unchanged. Publication rechecks per-write/refuse policy before replacing
either file; recovery rechecks the same rule before roll-forward. An unwaived
pending move blocks unrelated writers too, including with commits off or
unreadable configuration. `move_refused` carries a closed `move_reason`: `source_not_live`,
`source_archive`, `destination_not_live`, `destination_archive`, `not_native`,
`sole_item`, `reference_definition`, `destination_carries_id`, `archived_id`,
`newline_mismatch`, `list_shape`, `unit_matches`, `fields_changed`,
`membership_changed`, `protected_unit`, `parent_level`, `sibling_title` or
`fence_unsupported`. Dirty files and unsuitable commit
policy use `commit_refused`; stale guards use `guard_conflict`; missing or
excluded collection members use `collection_conflict`; duplicate included
member titles use `ambiguous_selector`. The filesystem must provide birth time
and a no-replace rename for the record-owned durable Git index fence. A move
proves no-replace rename on `.git` before its pending record is published;
without it the move refuses `fence_unsupported` with no pending record, no
staging entry, no `index.lock` and no receipt event, both documents, HEAD and
`.git/index` unchanged, so no writer is left blocked. The candidate blobs it
already stored in Git's object store may remain, and so does the probe file
described with the fence lifecycle in
[§ Pending records and receipts](#pending-records-and-receipts). Without
birth time the move also refuses `fence_unsupported`, before any staging
entry exists.

A successful move creates one commit changing exactly the two paths, with one
`Rivet-Op` and one `Rivet-Move` trailer. The destination is made durable before
the source loses the unit. Pending recovery, compensation and explicit fence
waivers follow the two-path rules below. `get` of the old source identity uses
its existing `missing_selector` diagnostic; the destination resolves the same id.

If the source no longer carries the requested id, retry searches at most 4096
first-parent commits touching the destination, bounded by the Git read budgets.
It verifies the receipt against immutable parent/candidate blobs and requires
the entire original key: paths, id, both guards, collection, `under`, and the
section parent/before fields. It returns `status: unchanged`, the original
commit `sha` and `op`, and `current: matches`, `changed` or `absent` for the
destination unit. This read precedes the clean-file check, so a dirty destination
can still answer. It writes no new event. Different `message` or `session` does
not change the retry key. No verifiable matching receipt returns `not_found`;
unsupported receipt versions or malformed receipt encoding refuse explicitly.

### `archive-copy` and guarded deletion preconditions

`archive-copy` commits exactly one existing destination file. It requires
`git.commit: "per-write"` and `git.on_dirty: "refuse"`. Its source is an
item in a live bound collection, a stayed direct bullet of a heading named by
a `section_archives` binding's `items` key (a *heading item*), or a stayed
section subtree in a configured `section_archives` binding. A collection item's
destination matches its collection's `archive` globs; a heading item's or a
section's destination matches its section archive binding. The source and
destination differ.

```json
{"source":"tracking/example.md","id":"<item id>","guard":"<native item revision>","path":"archive/month/closed.md","select":{"kind":"heading_path","path":["Closed"]},"destination_guard":"<section revision>"}
```

The destination `select` names a section; a position handle there is refused
with `invalid_request` before anything else, including the retry of a copy
already committed. A delete receipt whose operation names a position is
refused by `verify-delete` with `invalid_delete_receipt`.

This shape is exercised with real guards by the CLI tests. The engine reads
both files under the project lock, checks both guards, and copies the exact
source item bytes, including its item-owned stays and ordinary nested content.
For sections, it copies the content extent described below, including nested
stays with their original attachments.
The source stays byte-identical. General marker-bearing paste remains refused.
A sole collection item is supported: archive-copy plans its removal with the
list's current native revision, while copying only the item's bytes and stays.
The list stays disappear only in an explicitly list-guarded delete. A heading
item is never its list's sole item (§ `section_archives`).
An item lands at the end of the destination section's own body, before any
child section: appended to the own body's final block when that block is a
list with the same marker delimiter (`-`, `*`, `+`, `.` or `)`), otherwise as
a new list. A list using another delimiter is left as it is.
Unsupported native deletions (including a foreign stayed child),
hash drift in copied markers, incompatible newline/layout, conflicting ids,
missing archive configuration, dirty or staged paths, symlinks and confinement
failures refuse explicitly. The copier rereads the source guard before
publication. `message`, `session` and `lock_timeout_ms` are optional.

Create a marker-free archive envelope with ordinary `create` and `stamp: true`.
`archive-mkdir` accepts `{"path":"archive/month"}` for parent directories
leading to a configured archive. It uses confined directory descriptors and
refuses symlinks. Directory creation confers no deletion authority.

The committed result's `commit.copy_op` and `commit.sha` identify its copy.
The reserved `Rivet-Archive` trailer and pending/Event `archive_copy` metadata
record `(source path, unit id, native revision, destination path)`, the unit's
byte hash and ids. The version-1 field `item_sha256` also holds a section's
content-extent hash; its name and receipt layout stay the same. Section ids
come from the native delete's dropped-id calculation, including nested stays. Retrying that key returns `unchanged` only when exact
bytes and attachment have committed provenance. It never appends a duplicate.
A failed or pending copy is recovered through `commit-pending` before removal.

A native `write` or CLI `delete` may supply these enclosing preconditions:

```json
{"require_commit":true,"archive":{"path":"archive/month/closed.md","copy_op":"<committed copy op>","commit":"<full copy commit oid>"},"policy_guard":"<prune snapshot revision>"}
```

These fields supplement the usual path, selector and native guard; they are
not operation content. `archive` and `policy_guard` require exactly one delete
and imply `require_commit`. They remain optional for ordinary deletion.
`require_commit` refuses unless per-write/refuse preflight and final candidate
hashing produce an eligible clean commit intent, including index flags,
conversion attributes and known Git locks. A subsequent Git or durability
failure still reports applied/pending state honestly; it does not roll back
published source bytes.

Under the same lock, an archive precondition verifies an ancestral copy
commit and this source's exact bytes, ids and attachment in both that commit
and today's archive. HEAD, index and raw archive worktree bytes must agree.
The archive precondition alone allows later appends that preserve the copied
unit. A combined `policy_guard` still refuses any archive change or HEAD
movement and requires a new snapshot and policy decision. Recovery repeats
the check before attempting a new commit; a change that fails that check
requires repair and leaves the source candidate and archived history intact.

### `section_archives`

Section archive bindings live beside `collections` in `.rivet/project.json`:

```json
{"section_archives":{"history":{"files":["tracking/*.md"],"archive":["archive/*/history/*_HISTORY_*.md"],"items":{"headings":[["Log"]]}}}}
```

A source matches exactly one section binding whose archive globs match the
destination. Patterns use `*`, `?` and whole-segment `**`, with the same
path checks as collection bindings. A section archive glob must be disjoint
from every live glob (collection and section sources) and every collection
archive glob. Configuration loading checks intersection of the glob languages,
even in empty directories. Section source globs may overlap collection sources.
Citation resolution reads both kinds of live and archive namespaces and prefers
live carriers during the copy/delete overlap. `archive-mkdir` recognizes both
archive namespaces, including projects with no collections. Readiness and
`prune-snapshot` continue to discover collection archives only.

History files use `archive/<YYYY-MM>/history/<NAME>_HISTORY_<YYYY-MM>.md`.
The `history/` directory keeps this namespace disjoint from collection archives
matching `archive/*/*_CLOSED_ITEMS_*.md`.

A section copy supplies its heading stay as `id` and its direct parent's heading
stay as `parent`. Both must be stamped identities. The native section guard
covers the whole subtree; the engine checks ancestry under the writer lock:

```json
{"source":"tracking/example.md","id":"<entry heading stay>","guard":"<native section revision>","parent":"<parent heading stay>","path":"archive/month/history/example_HISTORY_month.md","select":{"kind":"heading_path","path":["Log"]},"destination_guard":"<destination section revision>"}
```

This request shape is exercised by the CLI section archive tests. Deletion
selects that same heading stay by id and supplies `parent` inside `archive`:

```json
{"path":"tracking/example.md","select":{"kind":"id","id":"<entry heading stay>"},"guard":"<native section revision>","archive":{"path":"archive/month/history/example_HISTORY_month.md","copy_op":"<copy op>","commit":"<full copy oid>","parent":"<parent heading stay>"}}
```

The copy keeps the expected parent in pending metadata separately from its v1
receipt, and recovery checks it again. Both copy and delete refuse a missing
or changed parent. Collection item requests and their archive guards refuse
`parent`.

#### Heading items

The optional `items` key lets a binding archive the direct plain bullets of
named headings, such as a tracker's bullet-list `## Log`, as items:
`{"headings":[["Log"]]}`. Each path is relative to the file's one level-1
title and matched exactly, each component one heading level below the last:
`[["Log"]]` names `# Title / ## Log` and never a nested `### Log`, a
`### Log` directly under the title, or a list inside a `###` entry below
`## Log` (that entry goes as a section). A file with no level-1 title or more
than one, or with two headings at a configured path, refuses. Loading refuses
an empty `headings` list, an empty path or component, a duplicate path, a
value of the wrong type and unknown fields. `items` adds no file glob, so glob
disjointness is unchanged. Without `items`, a binding archives sections only.

A heading item is a bullet (not ordered, not a task-list item) of a list in
the heading's own body, in a file matching exactly one section binding whose
archive globs match the destination and whose `items` names that heading.
The bullet itself lies outside every bound collection's protected subtree (a
collection under a child heading of the Log does not matter). It carries its
own item stay with valid child evidence; a foreign stayed child, drifted
evidence or a member of a bound collection refuses (a collection member keeps
the collection rules). The request supplies the heading's stay as
`parent`, as a section copy supplies its parent:

```json
{"source":"tracking/example.md","id":"<bullet item stay>","guard":"<native item revision>","parent":"<Log heading stay>","path":"archive/month/history/example_HISTORY_month.md","select":{"kind":"heading_path","path":["Log"]},"destination_guard":"<destination section revision>"}
```

Deletion selects the bullet by id and supplies the same `parent` inside
`archive`. One validation runs on the copy, a retry, the guarded delete and
recovery of either: `parent` must name the stamped heading directly above the
bullet's list, the bullet must not be its list's sole item, and the request
must carry no list authority. A bullet that became sole after its copy
therefore refuses on every path, and no list-version delete receipt arises.
Deleting the bullet removes its native deletion extent: its span through the
whitespace before the next item, or, for the last item, from the end of the
previous item, refusing if that gap holds anything but whitespace. The same
write refreshes the list's container digest; no other byte changes. The item's native revision
covers only its own bytes and stays, so a sibling inserted before it leaves
its guard valid.

The copy places the bullet's exact bytes as § `archive-copy` describes for
items. List tightness in both files follows the parser and may change; no
copied byte does. The destination list gains no container stay. These cases
are exercised by the CLI heading-item tests.

The copied bytes are the section's **content extent**: its whole span minus
the maximal suffix of physical lines that contain only spaces or tabs and
are owned by no parsed block. Every nonblank line remains, including standalone
link reference definitions, marker-only lines, and descendant content. CRLF
pairs remain whole. The source delete still removes the whole span. Before a
following section the archive inserts one blank separator outside the extent; at
end of file it ends with the source's own excluded blank lines instead, so the
archived section's native revision equals the guard the copy froze. A later
append leaves the copied extent and its provenance valid. When the source ended
with no blank line, that append's separator joins the earlier section's whole
span and moves its native revision. No copied content byte is normalized.

The destination section is exactly one heading level above the source. A
copied subtree becomes its direct child after any existing descendants. The
engine refuses level-1 titles, unstamped sections, overlap with any collection's
whole protected subtree (including excluded batches and the bound heading),
legacy-only descendant headings, unaddressed or outside-duplicated stays,
drifted evidence, and any checked or open task-list item, including nested tasks.
A section ending without a physical line terminator, or in an unterminated
fence or parser-owned HTML block, refuses explicitly. A consumed EOF indentation
also refuses because its ownership cannot be proved. Repair and evidence refresh
are separate writes: [`refresh`](#refresh) rewrites drifted evidence, and
`doctor` lists it beforehand as `evidence_drift`.

Section and heading-item duplicate discovery searches all section archives and
collection archives. A collection item copy and retry retain their own
collection's archive scope.
Every successful section copy preserves the source bytes and compares exact
content bytes, native dropped ids, source-relative marker positions and owning
block/item extents in its destination. Every copied id must resolve uniquely
in both the committed and current archive. Source rechecks compare against
the frozen copy candidate, including during recovery.
The same committed provenance and clean-archive checks apply to section deletes
and recovery as to item deletes. The CLI tests cover these requests, refused
writes, copy/delete crash recovery and ancestry races with real processes.

### `prune-snapshot`: immutable policy input

Configure the trusted policy reader's complete discovery namespace in
`.rivet/project.json`, for example:

```json
{"prune":{"files":["tracking/*.md"],"inputs":[".rivet/policy-reader.toml"]}}
```

This fragment supplements the normal collection and Git configuration.
`files` must describe all policy source files; any discovered file outside
the live bound scope holds capture and deletion. `inputs` names additional
confined data inputs, such as the reader's root/discovery configuration.
Documents use `.md` or `.markdown`; configuration inputs live under `.rivet`.
No executable or reader policy runs inside core.

`prune-snapshot` takes `{}` (or `lock_timeout_ms`) and returns a version-2
`snapshot`: revision, captured full HEAD, sorted `live`, `policy` and `archives`
membership, raw UTF-8 `inputs`, and a manifest of byte lengths and SHA-256 hashes.
It captures all live bound files, including excluded batches' bytes, project
configuration, bound schemas, and every collection's archive bindings. Archives
must be regular committed files with working bytes identical to their blobs at
the captured HEAD. Untracked, missing or dirty archives refuse; staged-only
archive changes with working bytes still equal to HEAD are not policy inputs.
Overlapping bindings do not duplicate archive membership.

Discovery walks the union of `prune.files`, live file patterns and archive
patterns. Unrelated helper symlinks and undecodable names outside that namespace
are ignored. Unreadable relevant paths, relevant symlinks, nonregular entries,
undecodable names and incomplete enumeration refuse. A traversal gap never
becomes an absent member. Live discovery retains normal hidden/build exclusions;
an archive pattern that could reach an excluded entry instead refuses, including
hidden entries, `target`, `node_modules`, `.git` and `.rivet`. Archive inputs
with unsupported document suffixes also refuse explicitly.

Each document is capped at 8 MiB; aggregate raw inputs at 64 MiB. Discovery
refuses beyond 50,000 encountered entries or 8 MiB of cumulative relative native
path bytes, counting unselected entries too. Snapshot Git reads share a 64 MiB
received-output budget, with 1 MiB for tree-list stdout, 128 bytes per blob-size
probe and 8 MiB per blob. Each Git child has a five-second deadline. The tree
listing is scoped to the deduplicated literal directory prefixes of the archive
globs, with literal Git matching and a directory boundary. If any archive glob
has no literal directory prefix (for example, `*/closed/*.md` or `**/*.md`),
capture reads the full committed tree, so a large unrelated namespace can still
exceed the unchanged listing cap and hold capture. With no archive globs,
capture skips the tree listing; empty
archive membership keeps the same version-2 revision encoding. HEAD is still
pinned and confirmed.
Limits refuse rather than truncate. There is no short whole-capture deadline.

Evaluate policy only from verified bundle bytes in a private scratch root,
using a trusted reader with its root override. Independently check namespace
coverage, freeze policy-affecting arguments and environment, and pin historical
reads to captured HEAD. Pass the revision as `policy_guard`; deletion
re-enumerates and hashes the full scope under its writer lock. Any HEAD movement
or archive change, including a harmless append or committed A to B to A, returns
`policy_conflict` before source removal. Re-capture and reconsider policy after
your own `archive-copy`; never refresh a stale guard automatically. Version-1
bundle consumers must adapt to version 2 and its revision formula before use.

Recovery substitutes only the confirmed source preimage. Every other input,
membership entry and HEAD must match. An applied, uncommitted delete with a stale
or version-1 policy guard remains `repair_required`, preserving its candidate
and pending record. Retrying recovery cannot re-authorize a version-1 guard or
one invalidated by HEAD movement. After inspecting the retained candidate, use
[`commit-pending` abandonment](#recovery-commit-pending) to retire the record;
abandonment leaves document bytes unchanged. An already committed transaction
settles without retroactive policy checks. Capture costs O(tree listing + bounded
traversal + selected input bytes) while holding the lock, plus a size probe and
blob read per archive.
Cooperating Rivet writers cannot change dependencies inside that interval.
An outside editor's other-file edit after the final observation can still race
publication; rewinding a ref to the identical OID is also outside this guarantee.

### `append`

```json
{"op": "append", "select": {...}, "guard": "<section revision>", "content": "A paragraph.\n\n- or a list"}
```

Adds Markdown blocks to the end of a section's own body: after its last
block and before its first subsection, separated by a blank line on each
side. Nothing before the insertion point changes. Content may not hold a
heading (`insert` adds a child section), open with a `---` block, or carry
stay markers. A result where the new blocks merge with what precedes them
(a list continuing the section's last list) is refused with
`invalid_content`. The op returns the section's next revision.

### `replace_in_section`

```json
{"op": "replace_in_section", "select": {"kind": "heading_path", "path": ["State"]}, "guard": "<section revision>", "old": "waiting", "new": "landed"}
```

The CLI shorthand is `markrivet replace-in-section` with `path` and these fields
on stdin, omitting `op`. It replaces exact UTF-8 **source text**, not a parsed
table cell. It searches only the selected section's own body, before its first
child heading. `get` includes descendants; select a child section to edit its
body. For a repeated cell value, pass a unique same-line substring with row
context, such as the complete row.

The locked read checks the section guard before searching. A stale guard
returns `guard_conflict` with the current section revision, even when `old`
is absent. Descendant edits invalidate the parent's guard; sibling edits do
not. Re-read and inspect the section before retrying.

`old` must be nonempty and neither string may contain CR or LF
(`invalid_request`); only sections are targets. Zero exact occurrences return
`match_not_found`, multiple occurrences `ambiguous_match`. Overlapping
occurrences count (`aa` occurs twice in `aaa`), as do occurrences inside code
and markers. The unique match must lie wholly inside one paragraph, including
a paragraph in a list item or block quote. The item or quote needs no stay,
even when the section contains stays. List and quote prefixes, code blocks
and marker-only lines are ineligible: a nonparagraph match
returns `unsupported_syntax`, and overlap with a live marker returns `invalid_content`.
GFM table source is a paragraph in Rivet's write model; optional markstay row
identity is declined. A paragraph holding a live stay its block or direct list item does not own,
including a row `subhash`, is refused with `unsupported_syntax`.

Marker overlap, a new live marker, a changed block kind, a split or merged
paragraph, a change of nesting, or a changed stay attachment is refused with `invalid_content`.
Marker-shaped text inside a code span remains ordinary content, including
when the replacement uses the span's existing delimiters.
There is no `allow_drop`. Empty `new` is allowed if one paragraph survives.
Set `"top_level": true` to keep the edit out of list items and block quotes: a
match inside one is refused with `unsupported_syntax`, so a caller can leave a
list's items to their typed writes. Top-level paragraphs and GFM table source are
unaffected, and no other answer changes.
Equal strings return `unchanged` after validating the match, without a commit.
Only the matched bytes and affected owned `hash=` or `subhash=` evidence change; the
heading and its marker remain byte-identical. The result returns a section
revision. Commit mode follows the existing policy: a clean preimage commits
the write; `git.on_dirty: skip` lands it uncommitted with `commit.status:
skipped` and `reason: dirty`. Refusals change no bytes and create no commit.

#### `remove_paragraph_in_section`

To remove a whole top-level paragraph, use the explicit operation
`remove_paragraph_in_section`:

```json
{"op": "remove_paragraph_in_section", "select": {"kind": "heading_path", "path": ["State"]}, "guard": "<section revision>", "old": "Last scanned: before\ncontinued residue"}
```

Its CLI shorthand is `markrivet remove-paragraph-in-section`, with `path` and
these fields on stdin, omitting `op`. `old` must equal the complete paragraph's
source, including indentation and internal line endings, excluding its final
line ending. Unlike replacement, removal accepts multiline `old`. The guard,
own-body search and uniqueness checks are the same. A partial paragraph is
`invalid_content`; a nested paragraph or nonparagraph is `unsupported_syntax`.
Removal refuses any live stay within the paragraph or on its attached marker
lines, since it would orphan or re-home that stay. It removes exactly `old`,
leaving surrounding blank lines and the final line ending unchanged. It has
no `new` or `allow_drop` field and returns the section revision.

### `add_key`

```json
{"op": "add_key", "key": "reviewed", "value": "2026-10-10", "guard": "<document revision>"}
```

Adds a top-level frontmatter key the document does not have, as one
`key: value` line after the last field (after a block list's items, too).
The value is a JSON string, number, boolean or null, written plain where it
reads back the same and quoted otherwise. The guard is the document's
revision. An existing key is refused with `already_exists` (change it with
`set` and its own guard); a document with no frontmatter is refused with
`unsupported_syntax`; a key must be ASCII letters, digits, `_`, `-` or `.`,
not starting with `-` or `.`.

### `set_key_field`

```json
{"op": "set_key_field", "key_path": ["generated", "at"], "value": "2026-11-02T12:00:00+13:00", "guard": "<generated key revision>"}
```

Changes one existing immediate scalar field in a top-level mapping, keeping
the flow or block rendering, key spelling, comments and every byte outside
that scalar. `key_path` has exactly two literal string segments (parent key,
field key); dots in either segment are part of that key. Deeper paths return
`invalid_request`. The guard comes from `get` on the **parent** frontmatter
key, so a change to another field of that mapping invalidates it. A sibling
top-level key's change does not. The response returns the parent's revision.

`value` is a JSON string, number, boolean or null. Quoting follows `set`,
validated in the mapping's original context. An absent parent or field returns
`missing_selector`. A non-mapping parent, an empty or collection-valued field,
a non-scalar replacement, or unsupported YAML (including anchors and aliases)
returns `unsupported_syntax`. It neither adds a missing field nor replaces a
whole mapping.

For a file containing `generated: {by: 'process:example', at: 'old'}`, or
the block form of that mapping:

```sh
echo '{"path":"tracking/TRACKING_FIELDS.md","select":{"kind":"frontmatter_key","key":"generated"}}' | markrivet get
# Use the returned revision as <guard>.
echo '{"path":"tracking/TRACKING_FIELDS.md","key_path":["generated","at"],"value":"2026-11-02T12:00:00+13:00","guard":"<guard>"}' | markrivet set-key-field
```

### `delete_key`

```json
{"op": "delete_key", "key": "retired", "guard": "<retired key revision>"}
```

Removes a top-level key and its whole parsed source lines, including its
value, inline comments, collection continuation lines and final line ending.
Trailing standalone comments and all other bytes stay unchanged.
The guard is that key's own revision. A missing key returns `missing_selector`;
unsupported YAML and deleting the last key return `unsupported_syntax`.
Deleting the last key would turn the frontmatter fences into Markdown, so it
is refused. The response has no replacement key revision. Refusals leave the
file byte-identical.

For a file containing `retired: old # removed with key` beside another key:

```sh
echo '{"path":"tracking/TRACKING_FIELDS.md","select":{"kind":"frontmatter_key","key":"retired"}}' | markrivet get
# Use the returned revision as <guard>.
echo '{"path":"tracking/TRACKING_FIELDS.md","key":"retired","guard":"<guard>"}' | markrivet delete-key
```

### `append_key_item`

```json
{"op": "append_key_item", "key": "verified", "item": {"by": "claude/example", "at": "2026-11-21T10:00:00+13:00"}, "guard": "<verified key revision, or the document revision when the key is absent>"}
```

Appends one item to a top-level frontmatter sequence whose items are all flat
mappings of scalars, such as a `verified` list of `{by, at}` entries. `item` is
a nonempty JSON object whose values are strings, numbers, booleans or null; its
keys follow the `add_key` name rules and must read back as text written plain
(`true`, `null` or `1` as a key is refused). Anything else in `item` is
`invalid_request`.

When the key exists, the guard is that key's own revision (from `get`), and
its value must be one of:

- a flow sequence on the key's line, `verified: [{by: a, at: b}]` or `[]`. The
  item goes after the last item, separated as the last two items are (`, ` when
  there is only one), so a trailing comment on the line stays after the `]`;
- block entries, indented or at the key's indentation, each a flow mapping on
  its `-` line (`  - {by: a, at: b}`) or a block mapping whose first field
  shares the `-` line (`- by: a` then `  at: b`). The new item repeats the last
  item's indentation and `-` spacing in the same form, with a block mapping's
  later fields under its first. It goes after the last item's line, before any
  trailing standalone comment lines.

The new item's fields come in the last item's key order (a field with an empty
value keeps its place), then the remaining keys in sorted order (JSON objects
carry no order through the CLI). Each value is rendered as `set_key_field`
renders one, in the item's own flow or block context: plain where it reads back
the same, and keeping the last item's quote style for that key when one does,
so a single-quoted `at` stays single-quoted. A string reading as a YAML 1.1
boolean or null is quoted, and one holding a line break or control character is
refused with `unsupported_syntax`.

When the key is absent, the guard is the document revision and the key is
added as one `key: [{...}]` line after the last field, under `add_key`'s rules
(`unsupported_syntax` without frontmatter, `invalid_request` for a bad key
name). With no previous item, values follow plain rendering, so a timestamp
with `:` and `+` is written unquoted, which Rivet reads back as text. A guard
of the other kind is stale either way: an absent key's guard after someone else
adds it, or a key revision after the key is deleted, returns `guard_conflict`.

Refused with `unsupported_syntax`, leaving the file byte-identical: a key
holding a scalar, a mapping, an empty value or a list of anything but flat
mappings of scalars (an empty item `{}` counts as flat); a flow sequence that
continues past the key's line; a block entry, any of them, whose mapping opens
on a later line than its `-`, or whose `-` is followed by a tab; and
unsupported YAML in the frontmatter. Before planning, the candidate frontmatter
is read again and must give the same keys in the same order with only this
key's value grown by the item. The operation always changes the file, so it
never returns `unchanged`; replaying it with the old guard returns
`guard_conflict`. The response returns the key's new revision. It writes
through the same path as every other key write, so commit mode, its receipt and
the journal treat it the same way.

For a file whose frontmatter holds `verified:` with one block item,
`  - {by: 'claude/a', at: '2026-11-01T10:00:00+13:00'}`:

```sh
echo '{"path":"tracking/TRACKING_FIELDS.md","select":{"kind":"frontmatter_key","key":"verified"}}' | markrivet get
# Use the returned revision as <guard>.
echo '{"path":"tracking/TRACKING_FIELDS.md","key":"verified","item":{"by":"claude/b","at":"2026-11-21T10:00:00+13:00"},"guard":"<guard>"}' | markrivet append-key-item
```

The second item lands as `  - {by: 'claude/b', at: '2026-11-21T10:00:00+13:00'}`
on the line after the first.

### `add_key_yaml` and `set_key_yaml`

```json
{"op": "add_key_yaml", "key": "lenses", "yaml": "[a, b]", "guard": "<document revision>"}
{"op": "set_key_yaml", "key": "scope", "yaml": "'new scope'", "guard": "<scope key revision>", "replace_line": true}
```

Write the caller's exact YAML value token to a top-level key. `yaml` is a
string holding one physical line: a JSON-compatible scalar or a flat flow
sequence of them, such as `[]`, `[a, 'b c', 1, true, null]` (an optional
trailing comma is accepted), `'it''s'`, `"x"` or `2026-11-13`. The token is
copied byte for byte, so its quote choice, escaping and spacing are kept, and a
list reads back from `get` as a JSON array, never as a bracketed string. Tokens
follow the reader's own rules ([§ Supported syntax and
limits](#supported-syntax-and-limits)): a YAML-only number or keyword (`0x10`,
`~`) is refused, a calendar date reads as a string, and a quoted `#`, `]` or
`,` belongs to its string. No quoting or rendering is applied, so a token that
means the same value with different quotes still changes the bytes.

Refused with `unsupported_syntax`: an empty token, whitespace around it, a
comment or anything else after it, a raw control character or line break,
anchors, aliases, tags, mappings (`{}` included), nested lists and block forms.
A wrong field type, an unknown field (a `select` in place of `key`, or
`replace_line` on an add) is `invalid_request`, exit 1.

`add_key_yaml` follows `add_key`: the document guard, a new `key: TOKEN` line
after the last field in the file's newline convention, `already_exists` for an
existing key, `unsupported_syntax` for a document without frontmatter and
`invalid_request` for a key name outside the `add_key` rules. It never returns
`unchanged`. Replaying an add with its original guard returns `guard_conflict`,
because the add changed the document; with a fresh guard it returns
`already_exists`.

`set_key_yaml` checks the key's own guard (from `get`) and replaces the value
token of an existing single-line scalar or flat single-line flow sequence;
either can become the other. The key's spelling and every byte before the token
are kept. By default every byte after the old token is kept as well, trailing
whitespace and an inline comment included. `replace_line: true` instead removes
everything after the token on the key's line, up to but not including its line
ending. Standalone comment lines under the key, other keys, the fences, the
body and its stays are never touched. An absent key returns `missing_selector`;
an empty value, a block list, a mapping (including `{}`) or a nested or
multiline collection returns `unsupported_syntax`. So does a list under a
quoted key holding a character the collection reader refuses (a BOM or a
noncharacter), since it would not read back; that key can still take a scalar.
The guard is checked first, so a write that would change nothing still refuses
a stale guard, and with a fresh guard returns `unchanged`. Both operations
return the key's new revision.

`map` keeps lists out of `frontmatter.writable`: that list describes `set`.
These two operations decide eligibility for each write themselves. They have
no shorthand command; send them through `write`.

For a file whose frontmatter holds `scope: 'old scope' # old tail`:

```sh
echo '{"path":"tracking/TRACKING_FIELDS.md"}' | markrivet map
# Use the returned revision as <guard>.
markrivet write <<'EOF'
{"path": "tracking/TRACKING_FIELDS.md", "ops": [
  {"op": "add_key_yaml", "key": "lenses", "yaml": "[a, b]", "guard": "<guard>"}]}
EOF
echo '{"path":"tracking/TRACKING_FIELDS.md","select":{"kind":"frontmatter_key","key":"scope"}}' | markrivet get
# Use the returned revision as <guard>.
markrivet write <<'EOF'
{"path": "tracking/TRACKING_FIELDS.md", "ops": [
  {"op": "set_key_yaml", "key": "scope", "yaml": "'new scope'", "guard": "<guard>", "replace_line": true}]}
EOF
```

The first write adds `lenses: [a, b]`; the second leaves `scope: 'new scope'`
with the old tail removed. Without `replace_line` the line would read
`scope: 'new scope' # old tail`.

### Repairs: `rejoin` and `adopt_stay`

Two narrow edits for hand-written shapes that leave an action unwritable.
Each accepts one exact preimage and refuses anything else.

```json
{"op": "rejoin", "select": {...}, "guard": "<section revision>", "line": 14}
```

A code span that opens on `line` and closes on the next one makes the §3.4
check misread the rest of its list. `rejoin` moves that break: when text
precedes the span on its opening line, the break moves in front of the
span; when only the item's marker (or nothing) does, the two lines join. The
span's text and everything after it keep their bytes, and the rendered page
must not change apart from whitespace, so a break that would become a hard
line break (a backslash before the span) is refused with `invalid_content`.
Both lines must be inside one list item in the selected section
(`invalid_request` otherwise), exactly one span must wrap from `line` to the
next (`missing_selector` otherwise), and a stay on the opening line after the
span is refused with `unsupported_syntax`, since the edit would move it.

```json
{"op": "adopt_stay", "id": "handID01", "guard": "<list revision>"}
```

A bare `<!-- stay:<id> -->` typed into an item reads as a second id of the
list, not as the item's. `adopt_stay` rewrites it in place as Rivet's child
stay, `<!-- stay:<id> subhash=sha256:<digest> -->`, so a citation of the id
now resolves to the item. The guard is the revision `get` returns for that
id (the list's). Refused: an id no marker or more than one marker carries
(`missing_selector`, `ambiguous_selector`); a marker with a digest or other
attributes (`unsupported_syntax`); one in a nested item or on the list's own
marker line (`invalid_request`); an item that already has Rivet's stay
(`unsupported_syntax`: drop the hand-written one instead); and a marker
behind a `<` or backslash in the list (`stamp_refused`).

### `refresh`

A stored digest that no longer matches its unit means someone edited the unit
without refreshing its evidence. `archive-copy` and `move` refuse such a stay,
and `doctor` reports it as `evidence_drift`. `refresh` accepts that edit as its
own write: it rewrites the named stays' stored digests to their units' current
digests, at the stored precision, and changes no other byte.

```json
{"op": "refresh", "guard": "<document revision>",
 "select": [{"id": "aOne7yT3", "expect": "3379e4c27a51"}]}
```

As a single-op command, `markrivet refresh` takes `path`, `guard`, `select` and
the usual `preview`, `message` and `session`. The guard is the document
revision `map` returns. Each `select` entry names one stay and `expect`, the
stored digest the caller saw (`doctor`'s `evidence.stored`), so a digest that
changed since then refuses with `guard_conflict` even under a current guard.

- **Key.** A section, paragraph or list stay's `hash`; an item stay's
  `subhash`. Refreshing an item never changes its list's digest, since markers
  are removed before a unit is hashed.
- **Selection is validated in full before any digest is rewritten**, and one
  bad entry refuses the whole write with the file byte-identical: an empty
  selection or an id named twice (`invalid_request`); an id no live marker
  carries (`missing_selector`); an id carried more than once, counting
  orphaned, unaddressed and HTML-block copies and a heading's `{#id}`
  (`duplicate_id`); a marker that addresses no unit (`unaddressed_marker`); a
  marker with attributes other than its id and one plain digest each
  (`unsupported_syntax`); no stored digest under the key (`invalid_request`); a
  stored digest longer than 64 hex digits (`unsupported_syntax`); and an
  `expect` that differs from the stored value (`guard_conflict`). A fenced
  copy of a marker is content, not a carrier.
- **Result.** Each op lists `refreshed`: per id its `key`, `stored`, `fresh`,
  `status` (`refreshed`, or `unchanged` when the stored digest was already
  current), `owner` (`section`, `block`, `list` or `item`), the owner's byte
  `span` and the marker's `line`. `evidence_refreshed` counts the rewrites,
  and named rewrites are not repeated under `drift`. With nothing drifted the
  write is `unchanged`, after the same guard and selection checks.
- **Verification.** The candidate must equal the preimage with only the
  named digest values replaced, with every marker occurrence, owner,
  identity and marker diagnostic unchanged, and each refreshed digest
  current.
- **One write.** `refresh` is the only op in its write (`invalid_request`
  otherwise). It takes the lock, guard, journal and commit path of any other
  write, and writes no delete receipt: no stay leaves the file.
- **Policy.** The engine checks structure, not whether the edit behind the
  drift was reviewed. Agents refresh through the store's wrapper, which
  applies the store owner's approval rule: drift an agent inherited goes to
  the owner as a preview to approve, and an agent applies a refresh of its
  own edit only when that commit accounts for all of the drift (the stay's
  evidence was current before the commit, and the file is byte-identical to
  that commit's version of it). A direct engine call is a path the owner has
  approved, for a reviewed preview. A preview does not take the lock; an
  apply that sends the preview's preimage guard re-checks it under the lock,
  so any change since the preview refuses with `guard_conflict`. Send the
  preimage revision, not the candidate `revision` a preview returns.

### `create`

```json
{"path": "tracking/TRACKING_NEW.md", "content": "# New\n\n## Action\n\n- [ ] **First.**\n", "stamp": true, "preview": false,
 "message": "new tracker", "session": "s1"}
```

`create` is a command of its own, not an op: it writes a document that does
not exist yet and never replaces one. The directory must exist (`not_found`
otherwise); Rivet does not make directories. Content is UTF-8 Markdown, LF or
CRLF, with no stay markers outside code; `stamp: true` mints them as `stamp`
does with no selector, using the collections the path would belong to. The
file is published with `renameat2(RENAME_NOREPLACE)` under the project
lock, so a file that appears meanwhile, from any writer, is kept and the
create is refused with `already_exists`, nothing written. The response
carries the new document's `revision` and any `minted` stays.

### `stamp`

```json
{"op": "stamp", "guard": "<document revision>"}
{"op": "stamp", "select": {"kind": "document"}, "guard": "<document revision>"}
{"op": "stamp", "select": {"kind": "heading_path", "path": ["Log"]}, "guard": "<section revision>"}
```

Mints stays for unstamped headings and direct list items in the scope. A
heading gets a marker line under it; a heading's existing `{#id}` becomes its
stay id. An item gets a child marker at the end of its last paragraph, and its
list gets a container marker line after it. Stamping is idempotent, and `get`
and `map` never stamp.

| `select` | Headings | List items | Guard |
| --- | --- | --- | --- |
| omitted (`collections`) | every section in the file | only inside a bound collection's member sections | the document |
| `document` | every section | every list in the file | the document |
| a section | that section and its subsections | its lists | that section |

The default is `collections`: a tracker's Log heading stays addressable while
a prose list does not fill the file with ids nothing asks for. Over a
141-file store it mints 3,382 stays where `document` mints 10,237. A file no
collection matches gets its headings and no items.

For one explicit collection's checkbox members only, supply all three scoped
fields with the original document guard:

```json
{"op":"stamp","guard":"<document revision>","collection":"tasks","checkbox_only":true,"collection_guard":"<typed collection revision>"}
```

This mode requires `select: collections` (or omission). Partial combinations
and other selectors refuse `invalid_request`. Under the writer lock it checks
both guards, then mints missing checkbox identities and necessary list
carriers. It mints no heading or ordinary-bullet identities. Existing stays
keep their attachments; audit/prose bytes and unrelated queues survive, except
ancestor evidence digests that need refreshing. Schema extraction is not a
prerequisite, so transitional invalid records can receive identities. An
unsupported carrier refuses the complete scoped write as `stamp_refused` with
no bytes changed. A fresh repeat is unchanged. The CLI bound-body tests cover
mixed lists, 23 synthetic audit rows (five unmarked), idempotence and refusal.
Older engines reject these new request fields; clients must probe actual
precondition enforcement in a synthetic store before relying on this mode.

Items with no paragraph, or whose list text before the marker contains `<`
or a backslash (markstay §3.4), are left unstamped and reported under
`refused`. That check reads the item's own **list**, so one `<` early in a
list refuses every item after it, and a narrower stamp scope does not change
that: it is a property of the list, not of the document.

An item whose own text carries a hand-written stay without a digest
(`<!-- stay:<id> -->`, not inside a nested item) is also left unstamped and
reported under `refused` with that `id`. The stay reads as the list's, but
whoever typed or cites it means the item, and a second id beside it would split
the two. `adopt_stay` (see Repairs) makes it the item's stay in place. When
`adopt_stay` would refuse the stay (it carries other attributes, another
marker or a heading's `{#id}` carries the same id, or the list's text before it
holds a `<` or backslash), the message says what to repair by hand first. An
item with two such stays names both in the message and carries no `id`: keep
one, drop the rest by hand, then adopt it; the message adds any repair keeping
a given one would need. The repairs are read from the file as it is, with only
the item's dropped stays deleted. A hand edit that changes the text around
another stay (deleting a marker glued between two code spans joins their
backtick runs) can expose one more refusal, which `adopt_stay` names when it
refuses. A skipped item's stay does not count
as its list's container stay, so when stamp mints any item in the list it also
mints the container, and adopting afterwards ends where a fresh stamp would. A
list whose every unstamped item is skipped gets no container stay.

A §3.4 refusal carries a `blocker` naming the character that caused it:

```json
{"code": "stamp_refused", "line": 12,
 "message": "the list's source before this item carries '<' (markstay §3.4)",
 "blocker": {"origin": "as_read", "character": "<", "offset": 431,
             "line": 9, "column": 17, "list_line": 7, "list_id": "S26FetLP"}}
```

Because the whole cascade shares one cause, every refusal it produces reports
the same `offset`. That offset, scoped to the file, is the key to group by.

Read the two counts precisely. Within one file and one pass, distinct offsets
count the **first blockers currently reported**, and refusals count the item
stamps attributed to them. Neither is the size of the job: a carrier scan stops
at its first offender, so `- a < b < c` reports one offset, and clearing it
exposes the next. Fix what is reported, run again, and repeat until nothing
refuses. The gap between the two counts is not small in practice, so size a
migration from a converged run rather than from the first pass.

The two positions are in different coordinate systems and are not two spellings
of one number. `offset` is a **zero-based byte** index from the start of the
file; `column` is a **one-based count of Unicode scalar values** from the start
of its line, and `line` is one-based. `column` is not a display column: a tab, a
combining mark or a multi-scalar emoji will not line up with what an editor
draws.

Both address the bytes the operation read, and nothing else. A write that mints
stays moves every byte after each insertion, so take positions from a `preview`
and edit the original. After any change, read again and take fresh positions
rather than adjusting old ones; a guard does not make an old offset safe, since
only a document guard covers the whole file.

A `preview` writes nothing, but it also takes no lock and so gives no
isolation. If something else edits the file between the preview and your byte
edits, the positions are stale with nothing to say so. Keep the exact bytes the
preview read and check the file still matches them before applying an edit.
That check narrows the window, it does not close it: the comparison and the
edit are themselves two steps. A migration that needs the gap closed should
hold the file still by other means rather than rely on the preview.

There is no `blocker` when the offending character is not in the file. That
covers text you supplied, an item's content or the Markdown a typed insert
renders from its fields, and also a refusal raised while checking the document
the write would have produced, which the refusal stops from existing. So an
absent blocker means "no position in your file", not "the character was in your
input". The message names the character either way.

### Typed writes

On its own, a typed write is an `update-item` or `insert-item` request that
names the file in `path` and leaves out `op`, since the command names it:

```sh
echo '{"path": "tracking/TRACKING_WIDGETS.md", "collection": "actions", "id": "S26FetLP",
       "guard": "<action revision>", "set": {"done": true}}' | markrivet update-item
```

A request carrying `op` is refused with `invalid_request` (*drop "op" from the
request*), and one without `path` with *missing field `path`*; neither touches
the file.

On a checkbox collection, `update-item` names its item by exactly one of `id`
and `select`, where `select` is a position handle or an id selector; both,
neither, or another selector kind is `invalid_request`. A file record is named
by `path` alone (§ Record writes). By position, the item needs no stay:

```sh
echo '{"path": "tracking/TRACKING_WIDGETS.md", "collection": "actions",
       "select": {"kind": "position", "heading_path": ["Widgets", "Action"], "index": 2},
       "guard": "<action revision>", "set": {"done": true}}' | markrivet update-item
```

The handle must name a member of the collection (`missing_selector`
otherwise, as for an id), and the guard is the item's typed revision, as
`query` returns it. A conflict follows § Revisions and guards: no `current`,
and `relocated` when exactly one member still has the guarded revision. The
response's `ops[n].select` names where the item is after the write. Inside a `write` batch it is the other way round: each op names its
`op`, and the batch carries the one `path`:

```json
{"op": "update_item", "collection": "actions", "id": "S26FetLP", "guard": "<action revision>",
 "set": {"done": true, "gate": "the diff is empty."}, "guard_scope": "action"}
{"op": "insert_item", "collection": "actions", "position": "end", "guard": "<collection revision>",
 "fields": {"title": "Publish the fixtures", "actor": "agent", "date": "2026-09-11"}}
{"op": "insert_item", "collection": "batched", "position": "end", "under": "Open (2026-09-28)",
 "guard": "<collection revision>", "fields": {"title": "Chase the fixture diff"}}
```

`update_item` applies explicit field edits and nothing else: there is no
implicit merge of an object. `null` removes an optional field (a checkbox cannot
be removed); setting a tag to its schema default removes the tag. The whole item
is guarded (`guard_scope: "action"`, the default); `"field"` is reserved and
refused until field-level dependencies are implemented and tested. `insert_item`
renders a new stamped item from fields. `start` puts it first in the bound
section's own body and `end` last in the last member section;
`"under": "<title>"` names a member section by exact title instead, refused with
`missing_selector` when no member has that title (an excluded one included) and
`ambiguous_selector` when two do. Without `subsections` the bound section is the
only member, so `under` can name only it. A target section with no list yet gets
one at the end of its own body. Both re-read the written item and refuse the
write if any field reads back differently from what was asked, naming each field
that moved and what it re-read as. A title ending in `(YYYY-MM-DD)` is refused
before that, when the schema has a `title_date` field: the date would read back
as that field, so pass it there.

#### Record writes

On a file collection (§ File records), `update-item` takes `path`,
`collection`, `guard`, `set` and optionally `append`, and no `id` or `select`,
either of which is `invalid_request`: the record is the file `path` names, and
the guard is its `record` revision, as `query` and `map` return it.

```sh
echo '{"path": "bundle/margin.md", "collection": "concepts", "guard": "<record revision>",
       "set": {"status": "retired", "stale_after": "2027-06-30T00:00:00Z"}}' | markrivet update-item
```

`append` adds one flat mapping of scalars to each `entries` field it names, in
the same change as `set`, so recording a verification and pushing
`stale_after` out is one guarded write:

```sh
echo '{"path": "bundle/margin.md", "collection": "concepts", "guard": "<record revision>",
       "set": {"stale_after": "2027-06-30T00:00:00Z"},
       "append": {"verified": {"by": "human:sam", "at": "2026-11-22T10:00:00Z"}}}' | markrivet update-item
```

Every field in `set` and every entry in `append` is written in one change,
guarded by that one revision,
and `ops[0].revision` is the record's next `record` revision; there is no `id`
and no `collection_revision`. Inside `write` the record write is a frontmatter
change: it combines with body ops in the same batch (a log entry beside the
status change) and is refused beside any other frontmatter op, as two
frontmatter ops are. Each field is written as the native key op for its form
writes it:

| Field state | `set` value | Bytes written | As |
| --- | --- | --- | --- |
| key holds a single-line scalar | a scalar | the value token, quote style kept where it still reads back | `set` |
| key absent | a scalar or a list | a new `key: value` line | `add_key`, `add_key_yaml` |
| key holds a single-line flow list (`list`) | an array of strings | the token, as `[a, b]` with each element plain where it reads back and quoted otherwise | `set_key_yaml` |
| key holds a block list (`list`) | an array | refused `unsupported_syntax` | |
| key holds an empty value (scalar types) | a scalar | refused `unsupported_syntax` | `set` |
| `field` source, parent and field present | a scalar | the field's token; sibling fields of one key are one edit of its lines | `set_key_field` |
| `field` source, parent key or field absent | a scalar | refused `missing_selector` | `set_key_field` |
| `field` source, parent key or field empty | a scalar | refused `unsupported_syntax` | `set_key_field` |
| key present, `key` source | `null` | the key's lines, leaving trailing standalone comments | `delete_key` |
| key absent | `null` | nothing | |
| `field` source | `null` | refused `unsupported_syntax` | |
| `entries` | anything | refused `unsupported_syntax`: append to it | |

An appended entry is written as `append_key_item` writes it (§
`append_key_item`): into a single-line flow sequence after its last entry, or
as a new entry after a block sequence's last line, in the form, key order and
quote style of the entry before it; or, when the key is absent, as a new
`key: [{...}]` line among the added keys. Its item rules and refusals are that
op's: a nonempty mapping of scalars with plain key names (`invalid_request`
otherwise), values that render on one line, and a sequence `append_key_item`
appends to (`unsupported_syntax` otherwise, an empty `verified:` and a flow
sequence spread over lines included). A bare mapping, which reads as one entry,
is refused `unsupported_syntax` naming the list form: `append` never rewrites
it into a list. `append` on a checkbox collection, naming a field the schema
does not have or one that does not hold `entries`, or naming a field `set` also
names, is `invalid_request`. An append always writes, so a request that appends
is never `unchanged`.

A collection where a scalar type reads, or a scalar or list where a `field`
source reads a mapping, makes the record invalid, which is refused before any field's form is
looked at (below).

Added keys go in as one insertion at the end of the frontmatter, after any
trailing standalone comment lines, in key-name order, an appended-to absent key
among them. Removing the last key while adding others puts them where its lines
were, or after the comments that followed it. An append to a block sequence
that is the last key goes in before the added keys, and before any trailing
comments; one whose next key the same request removes goes in where that key's
lines were, leaving the comments after them. A write that would leave no key is refused
`unsupported_syntax`, as `delete_key` refuses deleting the last one.

The checks run in a fixed order, and each refusal leaves the file
byte-identical: a field the schema does not have, an `append` field that does
not hold `entries` or one `set` also names (`invalid_request`), or
`guard_scope: "field"` (`unsupported_syntax`); the guard (`guard_conflict`); a
file with no frontmatter or frontmatter that does not read
(`unsupported_syntax`); a record invalid before the edit (`invalid_item`: fix it
with native key writes); each field's form, by the table, whatever the value,
and each appended entry's item and sequence (above); each value's type
(`invalid_item`, which `null` on a required field also is). Then a request
whose values all equal the stored ones and that appends nothing writes nothing
and answers `unchanged`. A stored value is compared before defaults, so setting an
absent key to its default adds the key, and `null` equals only an absent key:
a present `key:` or `key: null` is removed. An unwritable form or a stale guard
refuses even when every value is current. The written frontmatter must read
back as the preimage's with exactly the planned edits and the record must
re-read as asked, or nothing is written.

## Stays and evidence

Stays are [markstay](https://markstay.org) v1.10 markers, so other markstay
tools read the same ids:

```markdown
## Action
<!-- stay:hAct9x2Q hash=sha256:2261b4c779c1 -->

- [ ] **Ship the widget.** Context. <!-- stay:aOne7yT3 subhash=sha256:3379e4c27a51 -->
<!-- stay:lAct4bQ8 hash=sha256:b0f9c55b88f6 -->
```

- A `subhash` marker is a list item's own stay and is never reported as its
  list's stay. One inside a nested item or a table addresses nothing and is
  diagnosed as `unaddressed_marker`.
- When a write changes a unit, Rivet refreshes that unit's stored digest (and
  its list's) at the stored precision, and `evidence_refreshed` counts them.
  A digest that was **already** stale before the write is listed under `drift`
  with its `id`, `key`, `stored` and `refreshed` values: someone edited the
  unit without refreshing it. Drift is reported even if the write restores
  the bytes that digest already describes, so no digest rewrite is needed.
  A write leaves drift in units it does not change; `doctor` reports it as
  `evidence_drift`, and [`refresh`](#refresh) rewrites it as its own write.
- Digests follow markstay's CommonMark profile (§5.2). Outside its §5.4
  agreement subset, for example a heading with no blank line above it or a
  loose list with a whole-list marker, the blank-line profile that `markstay
  lint` and `markstay restamp` use by default computes a different digest for
  the same unchanged block. Check and restamp a Rivet-bound document with
  `--commonmark --child-blocks` on `markstay lint` and `markstay restamp`, or
  each tool reports the other's digests as drift. A blank line above the
  heading removes the heading split, and a tight list removes the list split.
  markstay §5.4 states the full condition, which other shapes, such as a
  fence with an internal blank line, also fail.
- Moving or minting a child stay follows markstay §3.4: if the list text before
  the marker contains `<` or a backslash, the write is refused with
  `stamp_refused`. A write also may not introduce such text in front of an
  existing stay later in the same list.
- A typed write naming only the checkbox (`{"done": true}`) changes the one
  byte between the brackets and moves no stay, so neither rule above applies
  to it: an action can be ticked behind a `<` earlier in its list, and when
  its body carries a stay owned by one of its own blocks. Any other field
  rewrites the item, which moves its stay and is checked as above.
- **A closed inline code span is masked before that check** (markstay v1.8), so
  a list whose earlier item quotes `` `<repo>` `` no longer refuses every item
  below it. The mask is narrow on purpose: backtick runs pair by equal length
  and only within one line, the scan stops at the first line whose runs do not
  pair evenly, and a run on a line markstay §3.3 counts as fenced code pairs
  with nothing. A run that never closes masks nothing, so `` ` `` followed by
  `<` still refuses. §3.3 counts a fence indented up to three spaces; at four
  it is an indented code block and its backticks pair normally.
- A document with a multi-line or overlapping marker cannot be read or
  written; one with a live MDX-form marker can be read but not written.
  Rivet refuses rather than guessing what such a marker binds to.

## Write safety

1. Take the project lock (`.rivet/lock`, an advisory lock that is never
   removed). `lock_timeout` means another writer held it; nothing changed.
2. Re-read the file, resolve every selector and check every guard against
   that read.
3. Build the candidate, refresh evidence, re-parse the whole candidate and
   verify: every untouched block and item is byte-identical at its new offset
   with the same live stay markers at their mapped positions, lists changed
   only in the declared items, no stay was lost or re-attached, and no marker
   fault was introduced.
4. Write a temporary file in the same directory, carry over the permission
   bits, sync it, check for a live Emacs lock, re-read the target once more,
   rename over it, then sync the directory.

The project root is opened once and held by descriptor. Every document,
configuration and lock access walks from it with `O_NOFOLLOW` directory
descriptors, and the temporary file, recheck and rename all work relative to
the checked parent directory, so renaming or replacing a path mid-write cannot
redirect it. Symlinks, hard-linked targets, files owned by
another user, special mode bits and extended attributes are refused before
writing.

What this does and does not promise:

- Writers that use Rivet never overwrite each other's changes, and a stale
  write is refused, not merged.
- A file an editor saved before the lock was taken is simply the new
  preimage. An editor that saves during a write, ignoring the lock, is
  detected by the final re-read (`external_change`, nothing written) except in
  the short gap before the rename. This is best effort, not isolation.
- An Emacs buffer with unsaved changes holds a lock beside the file,
  `.#<name>`. While that lock is live a write, `create` included, is refused
  with `editor_lock` and nothing is written. A lock is dead only when it is
  from this host and its pid names no process: the pid is not positive or
  out of range, or Rivet runs in the initial pid namespace and
  `kill(pid, 0)` fails with `ESRCH`. Rivet never removes a dead lock.
  Everything else is live: a lock from another host, a same-host lock with
  a positive pid that fits a `pid_t` while Rivet runs in a child pid
  namespace (a sandbox cannot see the host's processes), and a lock whose
  pid now names some other process, such as one left by an Emacs that
  crashed. That last kind refuses until it is removed, and the message says
  so. A `.#<name>` that cannot be read as `USER@HOST.PID[:BOOT]` is refused
  too, naming the lock. Unlike Emacs,
  Rivet does not compare the boot field or any other clock: a clock step
  moves `/proc/stat`'s boot time and leaves the lock's fields where they
  were, and Emacs's boot source (utmp, a file's mtime) can already sit tens
  of seconds from the kernel's. Not covered: Emacs with `create-lockfiles`
  off or `lock-file-name-transforms` set, an Emacs in another pid namespace
  that shares this host name, any other editor, a host name Emacs spells
  differently from `gethostname` (refused as another host), and a buffer
  first modified between the check and the rename. Emacs checks the file's
  modification time when a buffer is first changed and again on save, and
  asks before overwriting, which covers that last gap unless the user says
  yes.
- A crash before the rename leaves the file unchanged (possibly with a
  `.<name>.rivet-*.tmp` file beside it, safe to delete). A crash after it
  leaves the new file. `durable: false` or `uncertain` means re-read first.
- Batches change one file. Queries read each file once but do not freeze the
  collection.

## Git

With commit mode on, a write to a tracker that matched HEAD before it
becomes exactly one git commit, made while Rivet holds the project lock. A
write to a file that was already dirty still lands; whether it commits is
the dirty policy's call. Committing is off unless the project asks for it.

### Configuration

```json
{"collections": {...}, "git": {"commit": "per-write", "on_dirty": "skip"}}
```

| Key | Values | Default |
| --- | --- | --- |
| `git.commit` | `off`, `per-write` | `off` |
| `git.on_dirty` | `skip`, `adopt`, `refuse` | `skip` |

Without the `git` key, or with `commit: "off"`, writes behave and answer
exactly as they always have: no `commit` field, no bookkeeping files.

### Invocation policy

Every git child Rivet starts:

- starts in the project root Rivet holds open, so a root renamed or replaced
  mid-call does not redirect it;
- gets a cleared environment except `PATH`, `HOME`, `XDG_CONFIG_HOME`,
  `LANG`, `LC_ALL` and `TZ`, plus `GIT_LITERAL_PATHSPECS=1` (a file named
  `a*.md` matches only itself) and `GIT_NO_REPLACE_OBJECTS=1`; read-only
  calls add `GIT_OPTIONAL_LOCKS=0`. Private index operations set an internal
  `GIT_INDEX_FILE` pointing into the held pending directory; an ambient
  index override is stripped;
- runs with hooks, fsmonitor, signing and automatic maintenance off and
  `core.fsync=committed,index`, so git cannot call back into Rivet or reach
  the network. Git 2.36 or later is needed;
- leads its own process group, gets `SIGKILL` if Rivet dies, and is killed
  and reaped after 5 seconds, or on any early exit from Rivet. The one
  exception is the ref transaction of a per-write commit (step 7 below),
  which has three budgets of 5 seconds each and so may run up to their sum
  plus reaping. A child whose
  parent died before that signal was armed exits without running git. When
  git exits, anything left in its group is killed, and output a descendant
  keeps open cannot hold Rivet past the deadline. If Rivet itself is killed,
  git dies but a process git started would not; with hooks, fsmonitor and
  maintenance off, Rivet's git calls start none.

Calls that change refs or indexes start through a launch helper (the `markrivet`
binary itself), so the pending record names the child before git runs; git
keeps that PID. Immutable candidate blob storage precedes the pending record
and uses the same process guard and deadline without publishing a ref.

### Item snapshots

`item-log` reads the first-parent integration history of a stay ID in one
explicit project-relative file. The file and item may be deleted in the live
tree. For example (exercised by `history_documented_example`):

```sh
printf '%s\n' '{"path":"history.md","collection":"actions","id":"abcdefgh"}' \
  | markrivet item-log
```

The response pins `history_tip`, reports `traversal: "first_parent"`, and
returns newest-first `transitions`. It does not follow file renames or join
identities across files. `collection` labels the caller's current context;
historical extraction uses the structural parser across the whole file and
does not require current schema or collection membership. This read works with
commit mode off, a dirty index or working tree, and detached HEAD. An unborn
HEAD returns an empty committed history with `history_tip: null`.

Each transition carries the full commit OID, `first_parent`, raw Git `author`
and `committer` header values (including dates), message, and before/after
snapshots. Each snapshot has `blob`, `mode`, and a `state` whose `status` is
`present`, `absent`, or `gap`. A present item includes editable `content`, its
structural `revision`, own `ids`, and section/list `location`. Another target
kind remains visible as a kind change. `before_rev` and `after_rev` name present
snapshot commits, including the last present snapshot before a deletion.
These OIDs select immutable post-commit snapshots for `revert-item`.

Events distinguish appearance, reappearance after an observed earlier
incarnation, deletion, content update, kind change, location change, and
structural-only changes such as evidence refresh. A merge row sets
`integration: true` and compares the merge tree with its first parent's tree;
second-parent intermediate versions are outside this traversal. Uncommitted
intermediate text never becomes a version. Whole-file adoption, multi-op writes
and ordinary Git commits use the same snapshot comparison. Commit subjects and
receipts are not discovery inputs. `writer_session` is null: Git identities and
forgeable messages do not authenticate item authorship. Receipt enrichment is
not implemented. Each historical state has `replayable: false`: a structural
observation alone does not certify restoration under today's schema. On a pinned
committed branch, when the current live item resolves uniquely in the named
collection, has a supported location, per-write mode is on and no pending record
blocks the path, the read also returns `restore_guard` and `current_location`.
Otherwise `restore_guard` is null. A failed current-eligibility check returns
`restore_diagnostic`; a missing history tip or observed branch prevents that
check. Neither case removes readable history.

Read `coverage.complete` before treating the result as complete. Coverage
distinguishes an actual root, a shallow boundary, missing objects, parser or
identity gaps, and resource cutoffs. Unsupported modes (symlinks, directories,
submodules), invalid UTF-8, orphaned carriers and duplicate identities are
explicit gaps, never deletions. A partial read still exits 0 with valid JSON,
`coverage.complete: false`, a named `boundary` and diagnostics. Unavailable Git
or unsupported repository layouts refuse with `history_unavailable` and exit
2. The existing repository policy rejects partial/promisor clones and linked
worktrees. Missing objects cannot trigger lazy fetch.

Limits per request are 10,000 inspected commits, 1,000 distinct document blobs,
8 MiB per blob, 64 MiB decoded document bytes, 256 KiB per commit's metadata,
64 MiB total received Git stdout/stderr, 64 MiB retained history details and
64 MiB for the complete compact or pretty JSON response including its newline.
Each Git child keeps its five-second deadline; stdout is capped for its
command and stderr at 64 KiB while receiving. Repeated cached content and JSON
escaping count towards response limits. A cutoff reports
`history_resource_limit` and the limiting budget. An envelope too large to
return a partial result refuses with the same code. There is no pagination.

This command opens raw Git objects without checking out historical content.
It takes no project writer lock, performs no recovery, and does not write the
target, index, refs, receipts, journal or `.rivet/`. A missing `.rivet/lock`
remains missing.

### Restore one live item

`revert-item` restores the editable content of one currently present item in its
current collection. It requires per-write commit mode. Read `item-log`, retain its
`history_tip` and `restore_guard`, then send those unchanged. For example (exercised
by `revert_documented_example_preserves_bytes_and_attachment`):

```sh
printf '%s\n' '{"path":"history.md","collection":"actions","id":"abcdefgh","history_tip":"<full OID from item-log>","guard":"<restore_guard from item-log>","rev":"<full source OID>"}' \
  | markrivet revert-item
```

`rev` is a full lowercase commit OID in the repository's object format, naming
the item's post-commit snapshot. Ref expressions and abbreviated OIDs are
refused before selection. Omitting `rev` chooses the snapshot before the latest
distinct editable content change at the supplied tip. This default requires live
content to match that tip. It skips evidence-only changes, refuses an absent
initial preimage, and never crosses a gap, deletion/reappearance, changed own-ID
set, kind or location.

The `item_restore` guard binds the canonical path, item ID and own-ID set,
structural item revision, collection/schema/binding inputs, stable ancestor
section ID sets, parent-list identity, supplied tip and observed branch. Section
titles and sibling content are not identity inputs. Every ancestor section must
have unique stays; the parent list needs unique stays or must be the only direct
list in its owning section. Multiple unstayed lists refuse
`history_location_unsupported`. Reordering siblings within that list is allowed;
moving the item or an ancestor to another stable location is refused, including
a committed move away and back.

An actual attempt holds the project lock and revalidates the current source. A
supplied tip must remain on the current first-parent chain on the observed
branch. Later commits are allowed only when they leave this item's content,
identities and location unchanged. Rewritten tips and second-parent-only
selectors refuse `history_revision_unreachable`; missing selected objects refuse
`history_revision_missing`. Target change-and-change-back refuses
`history_changed`. History gaps, identity ambiguity, absent targets, kind
changes, location changes and incarnation boundaries use `history_gap`,
`history_identity_ambiguous`, `history_target_absent`, `history_kind_changed`,
`history_location_changed` and `history_lineage_boundary`
diagnostics. An absent target is never recreated. No default yields
`history_no_previous_version`. Budget cutoffs refuse `history_resource_limit`,
never select from partial evidence.

With `preview: true`, the response includes `resolved_rev`, selected `content`,
`history_tip`, `restore_guard`, and `current`/`selected` objects. Each contains
strict native `fields` and `authored` values from discovery under the current
schema. An absent authored field is null even when a schema default supplies its
strict value. Core does not compare creation Session values or invent metadata:
a wrapper that preserves a creation Session must compare authored presence/value
itself and refuse mismatch before replay. Repeat the exact resolved OID,
original guard and tip in the actual request. Preview creates no lock file,
settles no pending record, and changes no project, Git or bookkeeping bytes. It
is advisory, not a reservation.

Restoration refuses pending records without implicit recovery; use
`commit-pending` separately. It reuses structural `set`, candidate verification
and durable replacement. The edit footprint is the current item splice plus
ancestor evidence-marker refresh spans. Current own stays remain on this item
and their digests are recomputed; historical stays and digests are not imported.
Foreign/nested stays, unsupported carrier attributes, invalid current-schema
fields and ambiguous targets are refused. Equal editable content returns
`unchanged` without rendering, evidence refresh, replacement, commit or receipt,
so current supported formatting and carrier placement survive.

Landing validates history against its actual preflight HEAD/branch, then checks
that pair again immediately before calling replacement. A moved pair refuses
before publication and concludes a prepared record as not applied. Ordinary Git
can still race after that last check: the project lock serializes Rivet writers,
not external Git, and the normal applied/skipped/pending outcome describes that
later window. Transient ref changes ending at the same OID are not observable.
The outside-edit and Emacs-lock checks remain active.

Normal `message`, `session`, `lock_timeout_ms`, dirty skip/adopt/refuse policies
and commit/durability outcomes apply. A commit message includes the source OID
in its body and uses the current caller's session. A restore creates no
`Rivet-Delete` trailer and uses the standard pending recovery protocol. Dirty
adopt may include existing unrelated dirty bytes in the commit while preserving
them in the document. A failed commit can follow an applied file edit; exit 3 is
uncertain or not durable and must not be replayed automatically. Reads and
preview never retry or substitute a new source selector.

### Preflight

One snapshot per path: the branch, the HEAD commit, the blob and mode at that
commit, the index entry, and the bytes and git mode read from the tree. The
base state comes first:

| Base state | When |
| --- | --- |
| `known_clean` | the HEAD blob equals the file's bytes, the file's git mode (the executable bit, when `core.fileMode` is on) equals the HEAD mode, and the index entry equals both |
| `known_dirty` | the bytes or the mode differ from HEAD (a mode-only change counts), the index entry differs, or the file was deleted |
| `absent` | a path to be created that is in neither HEAD, the index nor the tree |
| `unknown` | anything else: a git call failed, HEAD is unborn, an existing file is untracked, or a path absent from HEAD and the tree is staged |

`on_dirty: refuse` refuses `known_dirty` and `unknown` before anything
else. Otherwise the first matching reason makes the path ineligible: the
write lands and does not commit.

| Reason | When |
| --- | --- |
| `git_too_old` | git is older than 2.36 |
| `git_error` | a git call failed or ran past its deadline |
| `unsupported_layout` | the root is not the work tree's top level, `.git` is not a directory, a linked worktree, a bare repository, `core.worktree` set, or a partial clone |
| `no_branch` | HEAD is detached or unborn |
| `repo_busy` | a merge, rebase, cherry-pick, revert or sequencer is in progress, or the index has unmerged entries |
| `untracked` | the path is not at HEAD (a `create` excepted) |
| `ignored` | a `create` only: git ignores the path, so `git add` would refuse it |
| `staged` | the index entry differs from HEAD |
| `index_flags` | assume-unchanged or skip-worktree is set |
| `git_conversion` | any of `filter`, `ident`, `working-tree-encoding`, `text`, `eol`, `crlf` is set or given a value for the path (unset, as `-text`, is fine), a filter driver named `unset` or `unspecified` is configured (git prints those like no driver), `core.autocrlf` is set to anything but false, or `core.eol` is set |
| `no_identity` | `user.name` or `user.email` is unset |
| `git_locked` | a write only: `index.lock`, `HEAD.lock` or the branch's lock is still present after a short retry |

Configured transformations are refused rather than proved harmless.

With either `on_dirty: skip` or `on_dirty: refuse`, a lingering Git lock on
an otherwise eligible, `known_clean` path lets the first write land without
committing: `commit: ineligible, reason: git_locked`. The file is then dirty.
Under `refuse`, the next write is refused byte-identical with `commit_refused`,
even after the lock clears, until the file is explicitly committed, for
example with `markrivet adopt`. Under `skip`, subsequent writes land: while the
lock remains they are still `ineligible` / `git_locked`; after it clears they
are `skipped` / `dirty`. They keep skipping until the file is committed;
clearing the lock does not itself commit the dirty file.

Resolve the lock and any retained pending records before adopting the file.
`markrivet commit-pending` recovers retained records; its `clear_lock` request
removes a reported lock only when the recovery conditions below hold. A
completed preflight `git_locked` write normally leaves a receipt and no
pending record, so `commit-pending` has nothing to commit for that write.
Without a retained record, the explicit unowned `clear_lock` form below can
remove a stale blocking lock when its stricter checks pass. Once eligible, `markrivet adopt` commits the whole
file, including any outside edits, and subsequent writes can commit again.

### Writing in commit mode

All of it runs under the project lock.

1. A path with an unresolved pending record first gets recovery (below). If
   a record remains, the write is refused with `pending_unresolved` under
   every policy, whatever the path's spelling.
2. Read, plan and verify as without commits.
3. Preflight. `on_dirty: refuse` refuses a `known_dirty` or `unknown` file
   here with `commit_refused`; nothing is written.
4. Store an eligible candidate's exact in-memory bytes as a Git blob,
   without filters. Publish a pending record (`.rivet/pending/<op>.json`) naming the
   intended outcome: a commit, or a skip or ineligibility with its reason.
5. Replace the file. A refused replace ends with a `not_applied` receipt and
   no record.
6. A write that will not commit ends with its receipt here. A write that
   will re-reads the file (an outside edit since the rename makes it a skip,
   `external_change`), then branch, HEAD, the path's HEAD entry and its
   index entry (any change makes it a skip, `head_moved`).
7. If the candidate's blob and mode equal HEAD's (an adopted write that put
   HEAD's bytes back), there is nothing to commit: `unchanged_in_git`.
   Otherwise build a tree from the pinned HEAD and the recorded blob/mode
   using a private index, then create its single-parent commit with
   `git commit-tree`, carrying exactly one `Rivet-Op: <op>` trailer.
   Hold the real `index.lock`, recheck its target entry and prepare its
   replacement while preserving every unrelated staged entry. Prepare a
   `git update-ref --stdin --no-deref` transaction comparing the full recorded
   branch ref against the pinned HEAD. Recheck symbolic HEAD while Git holds
   its prepared ref locks, then commit the ref update and durably install
   the replacement index before releasing `index.lock`. A lost comparison
   publishes no transaction and, once Git has exited by itself, reports
   `skipped: head_moved`. Acquisition
   of a just-taken index lock is retried three times; ref-lock failures stay
   pending as `git_error` for recovery after the lock is released.
   The transaction child has three separate 5-second budgets: preparation
   (from its recorded launch to Git's acknowledgment, including the input
   it is sent), the HEAD recheck (both of its reads share one budget) and
   completion (from just before `commit` is sent to Git's exit and output).
   A recheck that fails or finishes after its budget sends no `commit`:
   Rivet closes Git's input, and Git aborts the prepared transaction and
   releases its locks, within the completion budget. When Git finished that
   abort, a moved HEAD skips as `head_moved`; otherwise the write stays
   pending for recovery (`deadline` when the recheck ran late or the abort
   had to be killed, `git_error` when Git died by a signal or exited
   non-zero instead of aborting). A child killed at any of these bounds after preparing
   can leave `HEAD.lock` and the branch lock, and its record is never
   settled as a skip; the locks go to `stale_git_lock` recovery below and
   are never removed automatically.
8. Whatever git's exit status, inspect HEAD and the index. Committed means
   HEAD advanced one commit on the pinned branch, its parent the pinned
   HEAD, changing only the path, to the candidate's blob and mode, with
   exactly one `Rivet-Op` trailer naming the op, and the index entry is the
   candidate's. An incomplete index installation leaves a published pending
   transaction; recovery installs its recorded entry only when safe.
   An editor save after the candidate reread cannot change the committed
   blob. A detected later change adds `reason: external_change` to the
   committed response and receipt, leaving the editor's worktree bytes alone.
   This observation is best effort; an editor can save again after it.

A `create` has the base state `absent`: nothing at HEAD, in the index or
in the tree. Its record says `creation: true` with an empty `read_sha256`
(nothing was read), publication never replaces a file, and its recorded
blob takes the same private-index route as an existing file or untracked adopt.

| `on_dirty` | A write to a `known_dirty` file |
| --- | --- |
| `skip` | lands; `commit: skipped, reason: dirty`, with the ids of the units that differ from HEAD. Later writes skip too until the file is committed |
| `adopt` | commits the whole file, outside changes included; each unit that differs from HEAD, bytes outside every unit, and a mode change become an `Includes-outside-edit:` trailer |
| `refuse` | refused before anything is written (`commit_refused`) |

Without `message` the subject is `rivet: <path>`. The response keeps
`status: applied` and exit 0 whatever happened to the commit, because the
write itself is certain; `commit` says what:

```json
"commit": {"op": "<op>", "status": "committed", "sha": "...", "retries": 0}
"commit": {"op": "<op>", "status": "unchanged_in_git"}
"commit": {"op": "<op>", "status": "skipped", "reason": "dirty", "units": ["XqnPhqCs"]}
"commit": {"op": "<op>", "status": "skipped", "reason": "external_change"}
"commit": {"op": "<op>", "status": "ineligible", "reason": "staged", "detail": "..."}
"commit": {"op": "<op>", "status": "failed", "reason": "index_locked", "published": false, "pending": "<op>"}
"commit": {"op": "<op>", "status": "failed", "reason": "parent_mismatch", "published": true, "pending": "<op>"}
"commit": {"op": "<op>", "status": "failed", "reason": "unverified", "published": "unknown", "pending": "<op>"}
```

Every `commit` object referring to a write record includes `op`, the
operation id shared by its pending record and `.rivet/commits.jsonl` events,
including `ineligible` writes. `pending` and `copy_op` keep their existing
values and locations; when present in `commit`, they equal `op`. An
archive-copy retry returns the original copy's `op` without a new event.
A not-durable write or a failed receipt append can return `op` before its
journal event exists; recovery uses the same id when it appends the event.
Commit mode off, previews and ordinary unchanged writes (including an
already clean `adopt`) create no write record and return no `commit.op`.
Refused requests and uncertain renames return no `commit` object; if they
leave a pending record, its id stays in the existing top-level `pending`.

`failed` reasons: `index_locked`, `deadline` and `git_error` (HEAD did not
move); `branch_mismatch`, `parent_mismatch`, `paths_mismatch`,
`blob_mismatch` and `trailer_mismatch` (HEAD moved, but not as this write);
`index_changed` (the transaction published but its index installation is incomplete
or its durability could not be confirmed); `unverified` (HEAD and the index could not be read). A
failed write's record stays for `commit-pending`. When the directory sync
after the rename fails, the response is exit 3 with `durable: false` and
`"commit": {"op": "<op>", "status": "pending", "reason": "not_durable", "pending": "<op>"}`;
an uncertain rename is exit 3 `uncertain` with `pending` beside `error`.
`receipt: "failed"` means the receipt could not be appended; the record
stays and recovery appends it. A skip decided after the rename
(`external_change`, `head_moved`) is reported only once its `landed` record
or its receipt is durable; when neither could be written, the answer is
`"commit": {"op": "<op>", "status": "failed", "reason": "bookkeeping", "pending": "<op>",
"intended": {...}}`, because the record on disk still intends a commit and
recovery will treat it so.

### `adopt`

```json
{"path": "tracking/TRACKING_WIDGETS.md", "message": "adopt hand edits", "session": "s1"}
```

Commit mode only (`invalid_request` otherwise). Under the project lock, `adopt`
settles the path's pending records first and is refused with
`pending_unresolved` if one remains, before stamping anything. It then stamps
the items a bound collection holds unstamped and commits the whole file as one
transaction, as `on_dirty: adopt` would, with each outside change since HEAD
named in an `Includes-outside-edit:` trailer. It commits a dirty file even when
there is nothing to stamp, since committing the file is the point. A file
nothing tracks yet, with no index entry (a `create` that landed ineligible
leaves one), is committed as a creation of its verified present bytes through
the same private index and ref transaction. A file that is clean with nothing to stamp answers `unchanged` with no
`commit` field. A path that is ineligible (staged, a conversion attribute, a
lingering lock and the rest) is refused with `commit_refused` naming the reason,
and nothing is written: `adopt` never lands stamps it cannot commit.

To bind adoption to a snapshot checked by the caller, supply the optional
`expect` object with both `revision` (the document revision from `map`) and
`head` (the full Git commit id used by that check). `revision` must be
`rivet:v1:document:` followed by 32 lowercase hexadecimal characters; `head`
must be 40 or 64 lowercase hexadecimal characters. Neither abbreviations nor
ref names are resolved. An absent `expect` keeps plain adoption's behaviour.
An explicit null, missing inner field, extra field, wrong type or malformed
value is `invalid_request`, exit 1, before opening the project or taking its
lock. Transaction responses echo the supplied `expect`, including refusals.

Guarded adoption holds the project lock and refuses pending records without
settling them: either a record on the path or a two-path record whose index
fence blocks it gives `pending_unresolved`. Run `commit-pending` separately,
then check the snapshot again. After loading the document, adoption compares
its revision before planning stamps. If stamp planning would change the certified
bytes, it refuses `guard_conflict`, exit 2, leaving file, index, HEAD, sequence
and pending records unchanged. Stamp and check that no stamp changes remain
before certifying. The certificate does not bind collection configuration,
and a binding change can bring unstamped items into scope.
After Git preflight it checks HEAD and, when `core.fileMode` is enabled, that
the file's mode equals HEAD's. These
checks precede the clean-file `unchanged` shortcut and all transaction
bookkeeping. A stale revision, HEAD or mode gives `guard_conflict` with the
path, exit 2, leaving file, index, HEAD, sequence and pending records unchanged.

Immediately before replacing the file, guarded adoption checks HEAD, branch
and the path's index entry again. A mismatch gives `guard_conflict` with
"nothing applied": the file, index and HEAD are untouched, but the sequence
has advanced and the candidate blob and pending metadata have been written.
A successful conclusion appends one `not_applied` receipt (`applied: false`)
and retires the record. If receipt append fails, the prepared record remains.

Guarded adoption prepares only candidates equal to the certified bytes.
Every commit it makes, live or through recovery of these records, has the
certified HEAD as parent and exactly those bytes as the path's blob.
Otherwise it commits nothing. A pending record prepared by an engine that
permits stamps during guarded adoption retains its recorded candidate,
including stamps, during recovery. A switch to another branch at the same
commit before preflight can pass: the certificate names a commit, not a
branch. The engine also accepts matching certificates for untracked files
with a born HEAD; a caller that certifies only existing HEAD blobs must refuse
those files itself.

This is a guarded transaction. The best-effort outside-editor check still has
a narrow gap before the rename in which an editor's save can be overwritten.
A Git commit between the rename and Rivet's commit is caught by the HEAD
recheck and ref compare-and-swap: adoption may answer `skipped: head_moved`,
leaving the certified bytes uncommitted, but Rivet cannot commit on an
uncertified parent.

### Pending records and receipts

Every commit-mode write that will land has a record from before the rename
until its terminal receipt is synced. A record holds the op id, a sequence
number from `.rivet/seq`, the path, SHA-256 of the bytes read and of the
candidate, the session and message, and the git facts preflight read: a
write that will commit also carries the branch, base HEAD, base and
candidate blob ids and modes, and the git child's PID, spawn time, boot id
and process start time (clock ticks since boot). These are recorded before
Git is released to run. If the process identity cannot be read, Git does not
run. A skipped or ineligible write's record carries only what preflight
managed to read, so it can be written while git is unavailable.

A guarded adoption's record also carries `expect_head`, omitted from plain
records. Its `base_head` stays equal to this certified HEAD throughout the
record's life. Recovery may settle an already-published transaction after a
later unrelated commit, and repair its index as usual, because the recorded
transaction's parent is checked. An uncommitted candidate is never rebound
to a different HEAD: recovery retains it as `repair_required` with reason
`guard_head_moved`, without changing its base, file, index or HEAD. The commit
routine also refuses an inconsistent certified parent before considering
`unchanged_in_git`. This applies to explicit `commit-pending` and settlement
by later plain writes, including ordinary commit failures and failed receipts.
Abandon a `guard_head_moved` record explicitly and inspect the remaining dirty
file before checking it again.

Older engines reject records carrying `expect_head`. Before rolling back to
an older binary (including a saved `markrivet.prev`), hold `.rivet/lock` and check
the pending JSON records with the current engine; settle or abandon every
record carrying `expect_head` before swapping binaries. Do not trust an older
engine's pending count to establish that no such records exist.

| State | Meaning |
| --- | --- |
| `prepared` | written; the rename has not been confirmed |
| `applied` | the candidate is in the tree, durably |
| `landed` | a write that will not commit is in the tree; only its receipt is owed |
| `unverified` | git ran, but HEAD or the index could not be read |
| `committed` | HEAD and the index carry the candidate |
| `repair_required` | Rivet cannot finish safely; `repair` says why |

`.rivet/commits.jsonl` holds one JSON event per line: `op`, `seq`,
`session`, `path`, `outcome`, `reason`, `applied`, `sha`, `published` (for
`failed`), `retries`, `git_ms`, `recovered` and `at`. `failed` is not
terminal; `committed`, `unchanged_in_git`, `skipped`, `ineligible`,
`abandoned` and `not_applied` are, and each op gets exactly one. An op's
outcome is its latest event. `applied` is `true` when the write is in the
tree, `false` for `not_applied` (and an abandoned write that never landed),
and `"unknown"` when the file held neither the bytes read nor the candidate.
Only `applied: true` outcomes count as writes or name a path's last writer.
Order ops by `seq`, never by line position: recovery appends late. A
partial last line is ignored, and truncated by the next writer.

Full-history receipt reads for `status` (including ownership and write totals)
are capped at 256 MiB; larger logs produce a `limit_exceeded` diagnostic.
`doctor` warns with `receipt_log_size` at 128 MiB, using file metadata without
loading the log. Its warning leaves time to plan retention before the status
cap is reached. The log is not rotated automatically: external readers may
need its complete history and fixed filename.

Recovery and abandonment scan complete receipt lines backwards in 64 KiB
chunks, retaining only selected pending ops' terminal events. They stop when
every selected op is found or the beginning is reached, so recovery works
regardless of the log's total size. A complete line larger than 256 MiB is
refused with `limit_exceeded` if the scan reaches it. Malformed complete lines
are ignored, as in full-history reads. An unterminated suffix is discarded
without buffering it, and append truncation searches backwards in bounded
chunks while preserving every complete prefix byte.

Pending records have a separate 40 MiB serialized limit, checked before
publication and read with the same bound during recovery. This accommodates
the caller text retained separately from a composed receipt-bearing message
within the 16 MiB request budget. An unreadable pending record blocks new
writes because its path ownership cannot be established.

A two-path record keeps `path`, `read_sha256`, `candidate_sha256`, `base`
and `candidate` for the source and adds `peer` with those same fields for
the destination. It also carries `move_receipt` and a durable index fence,
`fence: {dev, ino, btime_ns}`. The destination is replaced and synced first;
the source follows only after that sync succeeds. The record starts
`prepared` before either replacement and becomes `applied` after both. It
never concludes `skipped` or `landed`. Candidate blobs are stored in Git
before the record is published. Single-path record fields and omission rules
are unchanged. Older engines reject these additional fields; do not roll
back an engine while a two-path record is pending.

The event adds `peer_path`, `peer_applied` and `move_receipt`. Each path's
application certainty uses `true`, `false` or `"unknown"`; last-writer
ownership credits the destination from `peer_applied`. The pair has one op,
one terminal event and one commit changing exactly its two paths, with one
`Rivet-Op` and one `Rivet-Move` trailer. The latter holds canonical base64url
JSON without padding, version 1, at most 64 KiB of decoded JSON. The same
decoded object is stored in the record and event as `move_receipt`:

| Receipt field | Meaning |
| --- | --- |
| `version`, `op`, `kind`, `id` | Format version (`1`), operation id, unit kind (`item` or `section`) and requested stay id. |
| `ids`, `unit_sha256` | Every stay carried by the unit, in document order, and SHA-256 of its exact content bytes. |
| `config_sha256`, `bindings` | Digest of the move's binding and namespace configuration, plus entries containing `collection`, its `binding` fingerprint and schema `digest`. |
| `source.path`, `source.guard`, `source.typed_guard` | Source path, native unit guard and binding-aware typed guard (`null` for a section). |
| `source.collection`, `source.parent` | Item collection or section parent stay id; the inapplicable field is `null`. |
| `destination.path`, `destination.collection`, `destination.under` | Destination path, item collection and optional member title; section requests use `null` for the item selectors. |
| `destination.parent`, `destination.before` | Section parent and optional direct child stay id for placement; item requests use `null`. |
| `source.read_sha256`, `source.candidate_sha256`, `destination.read_sha256`, `destination.candidate_sha256` | SHA-256 of each complete file before and after the move. |

This is the two-path contract for store wrappers' journal, status, lock-check
and recovery readers. Pending ownership comes from the record's `path` and
`peer.path`; event ownership comes from `path` and `peer_path`, using each
side's `applied` or `peer_applied` certainty. A wrapper can pin post-write
reads to both candidate hashes and recover the carried ids from
`move_receipt`. It must treat the pair as one operation and recover it
together, rather than adopt or commit either pending path alone.

The fence blocks git writers that honour `index.lock` before either file
is replaced. The record is published first, without a fence. Rivet then
creates the fence inode under its own name, `.git/rivet-fence-<op>`,
publishes its identity in the record, and only then renames it onto
`index.lock` without replacing an existing one. A crash at any point
therefore leaves either no `index.lock` from this move or one its record
names, never one no record owns, provided nothing else writes Rivet's
staging name: the rename names a path, not the held inode, so a process
replacing that entry between Rivet's check and the rename can put its own
file at `index.lock`, which recovery then reports as a foreign lock.

Rivet never deletes a staging entry. Checking an entry and then unlinking it
by name leaves a gap in which a replacement would be removed, so an
entry leaves its name only through the creating move's own rename. A refused
move ends one of three ways:

- Refused before the record is published (no-replace rename not proved on
  `.git`, below): no record, staging entry or receipt event.
- Refused before the entry exists (no birth time on `.git`): the record is
  removed with no receipt event.
- Refused after its fence is named but before a document is replaced (HEAD
  or an entry moved under the fence, a configuration check): the fence is
  released and the record removed with no receipt event. If either step
  cannot finish, the refusal carries the record's op as `pending`.
- Failed after creating the entry but before `index.lock` is shown to be its
  fence (its identity not read or synced, its identity not published, the
  rename refused because `index.lock` is held or no-replace rename is
  unsupported, the entry no longer the published inode, a rename error that
  does not prove the rename failed): the entry stays where it is and the
  record stays pending, with the op as `pending`.

While an entry is present, recovery and abandonment take no fence, whatever
holds `index.lock`: recovery reports `repair_required` with `stale_git_lock`
and abandonment refuses with `pending_unresolved`, both naming the entry. Stop Git and Rivet
writers (the quiescence the unowned-lock clear asks you to assert), check
the entry, remove it, then run `commit-pending`. With both files at their
read bytes, recovery then concludes `not_applied` once its usual checks
pass, and otherwise reports `repair_required` with the check's reason (such
as `commit_policy_changed`), to be fixed or abandoned. A move that finds
another inode at `index.lock` after its rename keeps its record, and
recovery treats that lock as foreign.

No-replace rename is proved before a pair record is published, and again
before recovery or abandonment acquires a fresh fence (taking over an
existing fence needs no proof). The proof renames one empty regular file
between `.git/rivet-noreplace-a` and `.git/rivet-noreplace-b`, without
replacing, under the project writer lock. Exactly one of the two names exists
between moves, and it stays on purpose: removing it would be a check and then
a removal by name, the gap the staging rule avoids. It is not a `.lock` name,
document discovery excludes `.git`, Git ignores it, and `status` and `doctor`
do not report it. An operator may remove it with writers stopped; the next
move creates it again. The probe never removes, truncates, writes or replaces
anything. If something other than one empty regular file is at those names
(a non-empty file, a directory, a symlink, both names, a target that
appears, or an entry swapped in while the probe runs, which the rename moves
with its bytes), the move refuses `commit_refused` before publication, naming
both names: with Git and Rivet writers stopped, check them, leave at most one
empty regular file under those names, and retry. Recovery and abandonment
needing a fresh fence report the same detail, as `fence_unsupported` when the
rename is unsupported and `stale_git_lock` when the probe is obstructed, and
create no staging entry.

Two cases remain. If the proof passes but the fence's own rename is then
refused as unsupported (a filesystem that answers differently for the fence
file, such as some FUSE implementations), the entry stays and the record
stays pending, as above, and on that filesystem the pending move can keep
blocking locked writes. A record that needs a fresh fence on a filesystem
without no-replace rename (a store moved there while a move was pending)
cannot be settled there: recovery refuses `fence_unsupported`, creates no
entry, and the record waits for a filesystem where the rename works. A record
whose existing fence recovery takes over settles as before.

Rivet retains the fence across exits and crashes while either file
differs from its HEAD or index entry, unless explicitly waived. Recovery
takes over the same inode in place; a failed index write retains it and a
retry truncates the lock before writing. Installing the index consumes the
fence. A subsequent directory-sync or receipt failure can therefore leave
recovery pending after both paths are committed, without a fence present.
Birth time is required; a filesystem without it
cannot host a move. Device numbers can change after reboot, so a fence whose
device, inode or birth time differs is foreign and needs an explicit clear
decision. Editors and scripts that bypass Git's locks remain outside this
guarantee.

Every locked document writer checks pair ownership even with commit mode
off or unreadable configuration. It settles the owning move when policy
allows recovery, or refuses `pending_unresolved`; an unrelated writer also
settles a crashed move's index fence first. Archive-copy checks its source
as well. Policy snapshots refuse pending pairs without settling them.

### Two-path recovery

`commit-pending` holds or reacquires the durable fence before examining a
pair. With no move commit in first-parent history, it checks both paths'
HEAD and index entries, eligibility, commit policy, collection configuration
and archive absence before changing a document. Each file must hold exactly
its recorded read bytes or candidate; an outside edit returns
`repair_required` with `file_changed` and remains untouched.

| Source | Destination | Recovery |
| --- | --- | --- |
| read | read | sync destination, then source; conclude `not_applied` |
| read | candidate | sync destination; roll source forward and commit both |
| candidate | read | roll destination forward and sync it; sync source and commit both |
| candidate | candidate | sync destination, then source; commit both |
| neither | any | preserve files; require repair |
| any | neither | preserve files; require repair |

A failed sync stops recovery before any further write. Fence retention and
release follow the lifecycle above. A replacement fence follows the same
order and the same staging rule: its identity is published in the record
before the name exists. If a step after the staging entry is created fails,
the entry stays and recovery reports it as above; if a step after the
rename fails, the record already names the new fence and the next recovery
takes it over. A fence whose
identity differs from the record's is foreign: recovery requires operator
repair and never guesses ownership or removes that lock. If HEAD advanced without changing either
path, recovery commits on the new HEAD. If the move already reached
history but a file differs from HEAD, recovery reports `worktree_differs`
and defers index repair, keeping the fence. Restore each named HEAD blob
with `git show HEAD:<path> > <path>`, then run `commit-pending`. The report
names the path. `git checkout` and `git restore` themselves need the locked
index, so use the lock-free blob route.

Abandoning a pair requires proof that no first-parent commit carries its op,
that both worktree files equal HEAD and the index, and that the two files
agree. They disagree when any stay id the move carries is in both files, or
when one file holds its recorded candidate and the other its recorded read
bytes (the stay would be in both, or in neither). An outside edit on either
side is the operator's; only a carried id in both files refuses it. A
disagreeing pair refuses with `pending_unresolved` and changes nothing:
commit both files with the move finished (both candidates) or undone (both
read bytes), then abandon. An applied or published move cannot be abandoned
to leave a half-transaction. To keep an
outside edit, an explicit `clear_lock` request for the verified fence marks
the record `fence_waived` durably and only clears the fence. It refuses a
published move. The record's branch must be current; its history audit stays
on that recorded branch, so switching HEAD cannot hide a published op.
The audit reads raw commit objects rather than decoded log messages: Git's
display and commit-encoding settings cannot hide a Rivet trailer. Missing,
malformed or oversized history output refuses the clear or abandonment.
An absence proof also refuses if that branch's tip changes during the audit.
Its raw first-parent chain must reach the recorded base; a shallow boundary
does not prove absence. The audit caps the revision walk's stdout at 1 MiB,
the raw object batch at 64 MiB and each child's stderr at 64 KiB, with a shared
66 MiB received-byte limit. This includes supported large caller messages.
Recovery of a waived record never acquires a fence, writes a
document or commits; writes to either path still refuse. Commit both edited
paths together by hand, then abandon the record. While waived, Git can
commit a single path, so the operator must commit the pair together.
Abandonment checks the committed result, not how it was committed: a
one-sided commit that leaves the files disagreeing is refused. Clearing
a recorded ref lock keeps its ordinary clear-then-recover behaviour. If a
crash leaves a waived record with its fence still present, repeat the explicit
clear before abandonment; abandonment refuses to orphan that fence.
A generic index-lock clear, including an older single-path op or the unowned
form, refuses while an unwaived pair is pending or pending ownership is unreadable.
A pending name ending in `.json` must be a regular file with a visible UTF-8
name. Directories, FIFOs, hidden record names and non-UTF-8 record names make
ownership unknown and block writes and snapshots too. Staging names end in
`.tmp` and remain outside the record namespace.
Status and recovery reports prefer the pair's exact fence identity over an older
child's timestamp.

### Recovery: `commit-pending`

```json
{}
{"op": "<op>"}
{"abandon": "<op>", "reason": "why"}
{"clear_lock": ".git/index.lock", "identity": "<dev>:<ino>:<mtime ns>", "op": "<op>"}
{"clear_lock": ".git/index.lock", "identity": "<dev>:<ino>:<mtime ns>", "size": 0, "quiescent": true}
```

`commit-pending` takes the project lock and settles every record (`{}`) or
one, in sequence order and in two passes. The first needs no git at all: a
`landed` record gets the receipt it names; a skip or ineligibility record
still `prepared` is settled by comparing the file with the hashes (the
candidate, the bytes read, or neither: `applied: "unknown"`); a commit
record whose file still holds the bytes read was never renamed,
`not_applied`. A creation's record is left to the second pass, which needs
the index to tell a create that never published (no file, no index entry,
nothing at HEAD: `not_applied`) from one that did.

A recorded child is dead when its boot id differs from the current boot or
its PID has a different process start time, even when that PID is alive.
Recovery, abandonment and lock clearing all use this identity check.
Records without a process identity remain readable and use PID existence
and zombie checks. Unreadable process identity data does not prove death;
PID existence and zombie checks still apply.

A pending list-guarded delete repeats its list-id citation check before
committing. A citation or incomplete scope returns `repair_required`;
`repair.detail` names the citing path and line, or the unreadable path.
The file keeps the candidate, the receipt remains in its record, and HEAD
and index stay unchanged. Inspect and abandon the record, then restore the
source from HEAD with `git checkout -- <path>` before another write or prune
run. Abandonment itself leaves document bytes unchanged. An earlier archive
copy remains committed.

Each remaining commit record:

- whose recorded git child is alive is reported `git_child_running` and left;
- whose child is dead has `next-index-<that pid>.lock` removed, unless some
  process holds it open or a git process works in the repository (the
  veto), which leaves it reported;
- meeting `index.lock`, `HEAD.lock` or the recorded branch's lock, whoever
  left it (a ref transaction killed at one of its budgets after preparing
  included), goes to `repair_required: stale_git_lock` with each lock's
  identity, the record it is attributed to and the veto's finding. No git
  runs for it and nothing is removed;
- otherwise is looked up by its `Rivet-Op` trailer on first-parent history
  back to its base, and retired as `committed` when found (repairing an index
  left at the base); or, when the tree still holds the candidate and the path
  and its index entry are as the write left them, committed on the current HEAD
  after a fresh preflight. For a creation that means the path is still absent
  from HEAD and its index entry is absent or exactly the recorded candidate,
  as legacy writers can leave. An old record naming an absent object is
  recoverable only from raw bytes and mode verified against that record;
  those verified bytes are stored before constructing a commit. A `.gitattributes` `filter` added since, for
  one, makes it `repair_required: git_conversion`.

A record found already concluded in the log (the sync of its receipt
failed) is synced and retired, never given a second event. `abandon`
retires a record as `abandoned`, carrying the certainty it had:
`applied: true` once it reached the tree, `false` when the file still holds
the bytes read (for a creation, when neither the file nor an index entry
exists), otherwise `"unknown"`, which includes an `adopt` with nothing to
stamp killed before its rename, whose bytes read and candidate are the
same.

`clear_lock` with `op` removes one reported `index.lock`, `HEAD.lock` or branch
lock, then recovers its record. It is refused (`clear_lock_refused`) unless the
record's git child ran and is dead, the file is regular, no older than that
child's spawn (or the recorded inode timestamp of Rivet's matching index
lease, which spans multiple children), still has the reported `identity`,
and the veto finds
nothing. Timing never proves who owns a lock, so the request is the
ownership claim and the floor only stops an obviously wrong one: confirm
nothing else is using the repository first. Any other lock file is reported
and left for you to remove.

Without `op`, `clear_lock` requires `identity` and `size` from the same
`status.git.locks` entry, and **`quiescent: true`**, the operator's explicit
assertion that nobody is using Git in this repository or any of its
worktrees. Stop editors' Git integrations and other Git writers before
making that assertion, and keep them stopped until the command returns.
The process veto is best effort: unreadable processes, closed lock
descriptors and repository targeting through worktrees can escape it.
The assertion supplies the quiescence that an unprivileged scan cannot
prove. Age alone does not establish ownership.

This form clears only a regular `index.lock`, `HEAD.lock` or current branch
lock, at least 30 seconds old. It refuses unreadable pending entries, a
pending record that may own this lock, or any recorded live Git child.
Unrelated records with no live child can remain. Under the project lock it
checks the reported inode, mtime and size, applies the existing process veto,
then rechecks the identity and size just before removal. A detected change
is refused. Linux host PID visibility is required for the veto; child PID
namespaces are refused. The operator assertion remains necessary even on
the host.

Clearing does not recover records or commit dirty Markdown: success has
`cleared`, `records: []` and `unresolved` equal to the number of remaining
pending records. Refusals use exit 2 `clear_lock_refused` with `error.reason`:
`pending_records`, `repository_unknown`, `lock_unknown`, `lock_missing`,
`unsupported_lock`, `identity_mismatch`, `lock_recent`, `process_unknown`,
`live_git_process`, `lock_in_use` or `identity_or_remove_failed`.
`live_git_process` means wait for that process to finish. Omitting `size` or
`quiescent: true`, combining either with `op`, or supplying an incomplete
form is `invalid_request`. Use the op-owned form for an attributed lock.

The response lists one entry per record, with `result` one of `committed`,
`unchanged_in_git`, `skipped`, `ineligible`, `not_applied`, `abandoned`,
`reconciled`, `repair_required`, `git_child_running`, `failed` or
`receipt_failed`, and `unresolved`, the number of records left. An entry
whose commit happened but whose receipt could not be appended carries
`receipt: "failed"`; its record stays, and the next run settles it. An
entry whose outcome is durable in the log but whose record could not be
removed (an abandonment or a reconciliation among them) keeps its result
and carries `record_left: true`; the next run reconciles it.

A repair that installs the candidate in the index happens only when no
commit after the transaction touched the path. The default commit subject,
`rivet: <path>`, escapes control characters, so no file name can add a
trailer line.

### Scope

The guarantee covers Rivet processes and tools that take Rivet's project
lock before touching git: among them, each Rivet write is exactly one
commit, a reported non-commit, or a pending record `commit-pending`
settles. Commits contain the recorded candidate even if a later editor save
changes the worktree. Git writers that honor index/ref locks are excluded
from the publication window. For `move`, exclusion covers both paths through
the durable `index.lock` fence described in
[Pending records and receipts](#pending-records-and-receipts), including
crash recovery. Its release and explicit waiver follow that lifecycle and
the two-path recovery rules above.
Editors and scripts that bypass those locks can still race the write, and
Rivet reports the changes it detects. The project root must be the
work tree's top level, and the platform is Linux. Durability covers a
process killed at any point (Rivet, its git child); after a power cut a
just-made commit or a record may be lost, and what is left is a dirty
tracker that `status` reports.

## Typed collections

`.rivet/project.json` binds a collection to a schema, a set of files and a
section title (a collection of file records takes no section; § File records):

```json
{"collections": {"actions": {
  "schema": "tracker-action@1", "files": ["tracking/*.md"], "section": "Action", "level": 2}}}
```

`.rivet/schemas/tracker-action.json` declares flat fields and where each is
read from in a checkbox item:

```json
{"id": "tracker-action", "version": 1, "item": "checkbox", "fields": {
  "done":    {"type": "boolean", "from": "checkbox"},
  "actor":   {"type": "string",  "from": "tag", "tags": ["agent", "human:*", "process:*"], "default": "human:owner"},
  "title":   {"type": "string",  "from": "title"},
  "date":    {"type": "date",    "from": "title_date"},
  "context": {"type": "string",  "from": "text"},
  "gate":    {"type": "string",  "from": "label", "label": "Gate"}}}
```

| `from` | Reads | Example |
| --- | --- | --- |
| `checkbox` | the task checkbox | `- [x]` is `true` |
| `tag` | a leading code span matching one of `tags` (`prefix:*` or exact) | `` `agent` `` |
| `title` | the leading `**strong**` text, unless it is a declared label followed by `:` | `**Ship the widget.**` |
| `title_date` | a trailing `(YYYY-MM-DD)` inside the title | `**Ship. (2026-09-01)**` |
| `label` | a line `**Label**: value` in the item's first paragraph; with `"multiline": true`, its continuation lines too, up to the next declared label line or the end of the paragraph | `**Gate**: it passes.` |
| `text` | every line of the first paragraph outside the title and the declared labels: the lines before the first label line, and those after a one-line label up to the next; a `**Progress**: ...` line naming no declared label is text | the context sentence |

A declared label followed by `:` is a field even when it leads the first line,
immediately after the checkbox or optional tag. For example, `- [ ] **Gate**: x`
has `gate: "x"` and no title when the schema declares the `Gate` label.

A label is one line unless it says `"multiline": true`, and the lines after a
one-line label are text. So a schema reading a tracker that wraps its gates
and keeps `**Waits**:` directly under the title says so field by field:

```json
"gate":  {"type": "string", "from": "label", "label": "Gate", "multiline": true},
"waits": {"type": "string", "from": "label", "label": "Waits"}
```

Text on both sides of a one-line label reads as one value, its runs joined by
a line break. A text write replaces the whole value where the text starts, in
the head when the head has text and otherwise in the first run, and removes
every other run, so the item re-reads as exactly what was written. A multiline
label writes its value's later lines at the indentation its continuation lines
already had, or its own line's; a one-line label refuses a value that spans
lines. Several missing labels can be added in the same `update-item` request;
they share one insertion at the paragraph's end, ordered by field name.

A text write immediately after a title adds a separating space when the
source has no gap, so replacing the text after `**Title**:` keeps the title
readable. A no-op preserves the original punctuation and spacing.

A field is optional unless it declares `"required": true`, and `title` is
left optional above on purpose. Trackers written before a schema existed carry
actions with no `**strong**` title, and they are ordinary members: their prose
reads through `text`, and `title` is simply absent rather than an error. Mark
it required only in a project that wants every action titled, which turns an
untitled one into an `invalid_item` diagnostic instead.

Editing any of the actor, title or date resolves the ordered head parts and
their separators together. Untouched parts retain their source spans rather
than being re-rendered: a title written `__T__`
stays `__T__`, a tag keeps whichever backticks spelled it, and the whitespace
between the last of them and the text is kept byte for byte, so two trailing
spaces still end the line and text on a continuation line stays on it. Setting
a field to the value it already holds writes nothing at all, including a title
wrapped across lines. New title values must fit on one line. The one thing that
is tidied is a wide gap between `]` and the first part, which becomes a single
space; a gap holding a line break is kept. A request setting only a checkbox or
a label to its current value writes nothing.

The one thing an untitled action cannot hold is a `title_date`, because the
date lives inside the title envelope; setting one alone is refused with *a date
lives in the title; set the title first*. Promote the action instead, in a
single guarded write:

```sh
echo '{"path": "tracking/TRACKING_LEGACY.md", "collection": "actions",
       "id": "aCr4uPYg", "guard": "<action revision>",
       "set": {"title": "Chase the missing invoice", "date": "2026-09-14",
               "context": "From last month."}}' | markrivet update-item
# - [ ] **Chase the missing invoice (2026-09-14)** From last month.
```

Types are `string`, `boolean`, `number`, `date` (a real calendar date),
`enum` (with `values`) and `timestamp` (§ File records gives its grammar).
Only checkbox items of lists in the section's own
body, above its subsections, are members, unless the binding takes
subsections:

```json
{"collections": {"batched": {
  "schema": "tracker-action@1", "files": ["tracking/TRACKING_BATCHED.md"],
  "section": "Action", "level": 2,
  "subsections": true, "exclude": ["Plans", "Numbered threads"]}}}
```

Then the own body of every descendant section (`###`, `####` and deeper) is a
member too, except a section whose title is in `exclude`, which leaves with its
whole subtree. Exclusion is by name, so a new dated `###` batch joins the queue
as soon as it is written. The collection revision covers member sections only,
so prose in an excluded one never conflicts with an insert. A non-empty
`exclude` without `subsections` is `invalid_schema`, and a binding without
either behaves as above. Moving an item between member sections and creating a
member heading are ordinary edits, not typed operations. The binding names a
schema id and version; a mismatch or missing schema file is `invalid_schema`.
Editing a schema changes its digest, so every typed guard taken before the edit
conflicts. Schemas are configuration: commit them.

Literal `files` and `archive` paths (patterns with no `*` or `?`) cannot
select hidden entries or entries named `target` or `node_modules`, at any
depth. Such a binding is `invalid_schema`, even if the file is absent. The
diagnostic names the collection, binding field, literal path and skipped
component. `map`, `get` and document writes refuse a path matched by that
collection's live or archive patterns; refused writes preserve the file bytes.
Glob patterns keep the existing traversal policy and do not produce this
configuration diagnostic. A literal path does not opt into excluded directories,
and a glob does not make them discoverable.

A binding may also name `"archive": ["archive/*/*_CLOSED_ITEMS_*.md"]`: files
holding the collection's pruned items. They are read only to resolve a
`ready` query's waits and the stay ids citations name (`doctor`'s citation
check, a write's `dangling_citations`), never listed, written or stamped
through the collection.

### File records

A schema whose `item` is `"file"` makes each matched Markdown file one record,
with every field read from a top-level frontmatter key:

```json
{"id": "concept", "version": 1, "item": "file", "fields": {
  "kind":         {"type": "string",    "from": "key", "key": "type", "required": true},
  "status":       {"type": "enum",      "from": "key", "key": "status", "values": ["draft", "current", "retired"]},
  "stale_after":  {"type": "timestamp", "from": "key", "key": "stale_after"},
  "tags":         {"type": "list",      "from": "key", "key": "tags"},
  "generated_at": {"type": "timestamp", "from": "key", "key": "generated", "field": "at"},
  "verified":     {"type": "entries",   "from": "key", "key": "verified"}}}
```

```json
{"collections": {"concepts": {
  "schema": "concept@1", "files": ["bundle/**/*.md"],
  "exclude_files": ["**/index.md", "**/log.md"]}}}
```

The binding takes `files` and `exclude_files` only. A member is a `.md` or
`.markdown` file that `files` matches and no `exclude_files` glob matches, in
the same glob grammar, and nothing is excluded by default. An excluded file is
in no collection anywhere: `query` and `doctor` do not read it, pathless `find`
does not search it, a write naming the collection refuses it as "not in
collection", and the prune snapshot's live scope and citation scope leave it
out. `section`, `level`, `subsections`, `exclude` or `archive` on a file
binding is `invalid_schema`, and so is `exclude_files` on a checkbox binding or
a checkbox binding without `section`. A literal `exclude_files` entry follows
the literal-binding rule above.

`from: "key"` is the only source a file schema takes, and a checkbox schema
refuses it. `key` names a top-level key; `field` reads one immediate scalar
field of that key's one-level mapping (`generated: {by: agent, at: ...}`).
Key and field names follow `add_key`'s rule. Two fields may not read the same
key and field, and one key may not be read both whole and through a field.
`tags`, `label` and `multiline` do not apply to a key field, and `id` stays
reserved.

| Type | Reads | `field` | `where` operators |
| --- | --- | --- | --- |
| `string`, `boolean`, `number`, `date`, `enum` | as for checkbox items, with no coercion: `yes` is not `true`, and a `string` field reads a date or timestamp as its text | yes | as for checkbox items |
| `timestamp` | `YYYY-MM-DDTHH:MM:SS`, an optional fraction of one to nine digits, then `Z` or `±HH:MM`, with a calendar date, hours 00-23 and minutes and seconds 00-59 | yes | `eq`, `ne`, `in`, `lt`, `lte`, `gt`, `gte` by instant; `exists`, `missing` |
| `list` | a flat sequence of strings, flow or block (`tags: [a, b]`); `[]` is an empty list | no | `contains` (an element equals the string), `exists`, `missing` |
| `entries` | a sequence of flat mappings of scalars, flow or block (`verified`); a bare flat mapping reads as one entry | no | `exists`, `missing` |

`timestamp` is available to checkbox schemas too, where a label can hold one.
Uppercase `T` and `Z` only: lowercase `t` or `z`, a space separator, a missing
offset and `24:00` are not timestamps, and nothing is coerced into one. Equal
instants in different offsets are equal, so `2026-12-31T23:00:00-05:00` is
after `2026-12-31T20:00:00Z`, and a value returns as its source text. An absent
key, an empty value (`key:`) and `null` all read as absent, so `default`
(scalar types only) and `required` apply as they do for labels.

A member is an invalid record when a value has the wrong type, a `field`
source's key holds no mapping, the file has no frontmatter, or its frontmatter
does not read (§ Supported syntax and limits). `query` reports each as an
`invalid_item` diagnostic, with the frontmatter reader's message and span in the
last case, and does not list it; `doctor` reports it as `invalid_item` beside
any `unsupported_frontmatter` finding. Exclude a file such as an index that is
not a record rather than leave it invalid.

A record is named by its project-relative path. It carries no stay id, and a
file renamed outside Rivet is a different record: a query cursor taken before
the rename is `stale_cursor`. Its guard is the `record` revision (§ Revisions
and guards), which `query` and `map` return, and the guard `update-item` writes
a record under (§ Record writes). Every checkbox-only command refuses a file
collection with `invalid_request` naming the shape, leaving the file
byte-identical: `update-item` with an `id` or `select` (a record is addressed
by path), `insert-item`, `delete` and `set` with the collection, the
checkbox-only `stamp`, `move`, `revert-item`, `analyze`, `export` and `query`
with `ready`. `item-log` still reads history, with `restore_guard: null` and
that refusal as `restore_diagnostic`. A file bound by a checkbox and a file
collection at once reads, stamps and writes on its checkbox side exactly as it
does without the file binding: `stamp {select: collections}` and `create` with
`stamp: true` stamp checkbox collections only.

An engine that predates file records refuses a project holding a file binding
with `invalid_schema` for anything that loads the configuration, so install the
new engine before adding one.

## Supported syntax and limits

- CommonMark with task list items, parsed by comrak 0.55. GFM tables are
  paragraphs for identity and writes; table-row stays are not supported.
- Frontmatter follows markstay §5.3: `---` on line 1, the first later `---` or
  `...` closes it, no blank lines, at least one YAML-looking line. Keys are
  unique and top level.
  - **Read and written**: single-line scalars, being strings, JSON numbers,
    `true`/`false`/`null`, and a calendar date (`updated: 2026-09-06`, a YAML
    timestamp rather than a JSON type). A date reads as its own text and
    writes back unquoted, so `updated` keeps the shape the file had. A
    timestamp (`2026-12-31T00:00:00Z`) reads as its own text too.
    Strings matching YAML 1.1 booleans (`y`, `n`, `yes`, `no`, `on`, `off`,
    `true`, `false`, in any capitalization) or null spellings (`~`, `null`,
    in any capitalization, or an empty string) are quoted when written, so
    other YAML readers preserve their string type.
  - **Read, with limited writes**: an empty value, and collections of those same scalars:
    flow or block lists and mappings, at either indentation and nested
    (`aliases: [a, b]`, a `generated:` mapping of `by` and `at`, `verified` as
    a list of `{by, at}` flow mappings, `sources` as a list of block
    mappings). `get` returns the decoded JSON as `value`. `map` reports these
    under `frontmatter.keys` but not under `frontmatter.writable`, and a `set`
    on one is refused with `unsupported_syntax`. `set_key_field` writes an
    existing immediate scalar field of a top-level mapping in place, preserving
    its rendering. `add_key_yaml` and `set_key_yaml` write a caller's exact
    single-line scalar or flat flow-sequence token to a top-level key.
    `delete_key` removes a whole top-level key's source lines.
    `append_key_item` appends one flat mapping to a single-line flow or a
    block sequence of flat mappings, or adds the key holding it. Other block
    and multiline list writes, editing or removing a list item, and
    whole-mapping replacements are refused.
  - **Unsupported**, at any depth: anchors, aliases, merge keys, tags, complex
    keys, a flow mapping key with no value (`{a, b: c}`), block (`|`, `>`) and
    multiline scalars, a duplicate key (the diagnostic names it), a plain key
    YAML reads as something other than text (`32:`, `0x20:`, `true:`, `~:`;
    quote it to mean the string), and a token only YAML reads as a number
    (`0x20`, `01`, `1_000`, `1:30`, `.nan`, `.5`), which is refused rather than
    coerced either way. `1Password`, `6.9.6` and `../plans/X.md` are strings, as
    YAML reads them.
  - Anything unsupported makes frontmatter `unsupported` for reads and key
    writes; body writes still work.
- LF or CRLF files (inserted text uses the file's convention), an optional
  BOM, and a missing final newline are preserved. Lone CR is refused.
- Files up to 8 MiB, and a write whose result would be larger is refused;
  requests up to 16 MiB (100 MiB for `verify-delete` only); `get` up to 64 KiB unless
  `max_bytes` is raised; `map` pages at 200 entries and `query` at 50 by
  default (1000 at most).
- Time grows roughly in proportion to the file: ten times the lines cost nine
  to fifteen times as long. Measured on a synthetic tracker whose `## Action`
  list and log sections are all stamped, release build, 20-core shared host,
  load 5-41, median of 7 runs (method and raw results in
  `benchmark/latency/`): at 3,000 lines (117 KB, 629 stays) `map` takes
  10 ms, `update-item` 27 ms and `stamp` 28 ms; at 30,000 lines (1.2 MB,
  6,254 stays) 97 ms, 236 ms and 416 ms. Files nearer the 8 MiB cap were not
  timed.
- Writers to one project take turns on its lock. In the same measurement,
  eight processes each making 10 writes to one 30,000-line file all
  succeeded: a write took 1.05 s at the median and 2.69 s at p90 (slowest
  5.3 s), inside the default `lock_timeout_ms` of 10 seconds, and none
  returned `lock_timeout`.
- Stays are optional for list items. An unstamped item is its bytes at its
  place: a position handle (§ Selectors) reads it and writes it through `set`,
  the bound-body `set` and `update-item`, and a write is refused when another
  item has moved into that place or the item's own bytes changed (§ Revisions
  and guards). The guard covers the item span, so the one byte a consumed
  final indentation leaves outside every span is not guarded. An item whose
  bytes repeat anywhere in the document cannot be written by position
  (`ambiguous_selector`) until the file is stamped. Positional `delete` is
  refused: delete by stay id, or `set` the section body.
- An address that outlasts inserts, moves and renames needs a stay, and so
  does every operation that carries identity across files, commits or time.
  Stamp first for `move`, `archive-copy` and section archives, `adopt_stay`,
  `refresh`, `item-log` and `revert-item`, a `ready` wait that names an item,
  and citations: each takes a stay id and cannot be spelled with a position.
- Setext titles, deleting the last frontmatter key, and adding a key to a
  document with no frontmatter are refused.

## Tracker walkthrough

Run it with `bash examples/tracker/walkthrough.sh` (set `RIVET` if the binary
is not on `PATH`). It copies the example project to a scratch directory and
checks every result. It runs the loop twice, on separate copies: first on the
unstamped tracker, where items are addressed by place, then stamped, with ids.
The unstamped run, abbreviated:

```sh
W=tracking/TRACKING_WIDGETS.md

# U2. Open actions: each has "id": null, "addressing": "position" and a select:
#     {"kind": "position", "heading_path": ["Widgets", "Action"], "index": 1}
echo '{"collection": "actions", "where": [{"field": "done", "op": "eq", "value": false}]}' | markrivet query

# U2b. find names the same place; U3. get reads it with its typed guard.
echo "{\"path\": \"$W\", \"select\": <select>}" | markrivet get

# U4. Complete it by position and log it under the Log heading path, in one batch.
markrivet write <<'EOF'
{"path": "tracking/TRACKING_WIDGETS.md", "ops": [
  {"op": "update_item", "collection": "actions", "select": <select>, "guard": "<action revision>",
   "set": {"done": true}},
  {"op": "insert", "select": {"kind": "heading_path", "path": ["Log"]}, "position": "start",
   "guard": "<Log section revision>",
   "content": "### 2026-09-11: fixtures regenerated\n\nRegenerated from the seeded corpus."}]}
EOF

# U5. Query again; U6. replaying the old guard exits 2 with guard_conflict and
# no error.current; U7. re-read by position and retry with the fresh guard.
# U8. move takes a stay id; the unstamped item has none, so it is refused.
```

The stamped run:

```sh
W=tracking/TRACKING_WIDGETS.md

# 0. Diagnose the collection; no bytes change. Unstamped items are warnings.
echo '{"collection": "actions"}' | markrivet doctor

# 1. See the structure, then stamp ids (preview first).
echo "{\"path\": \"$W\"}" | markrivet map
echo "{\"path\": \"$W\", \"guard\": \"<document revision>\", \"preview\": true}" | markrivet stamp
echo "{\"path\": \"$W\", \"guard\": \"<document revision>\"}" | markrivet stamp

# 2. Open actions across every tracker.
echo '{"collection": "actions", "where": [{"field": "done", "op": "eq", "value": false}]}' | markrivet query

# 2b. Or find it by its text, with the same id, its selector and typed guard.
echo "{\"text\": \"Refresh the fixtures\", \"path\": \"$W\"}" | markrivet find

# 3. One action with its typed guard.
echo "{\"path\": \"$W\", \"select\": {\"kind\": \"id\", \"id\": \"<id>\"}}" | markrivet get

# 4. Complete it and add a newest-first log entry, in one batch.
markrivet write <<'EOF'
{"path": "tracking/TRACKING_WIDGETS.md", "ops": [
  {"op": "update_item", "collection": "actions", "id": "<id>", "guard": "<action revision>",
   "set": {"done": true}},
  {"op": "insert", "select": {"kind": "id", "id": "<Log section id>"}, "position": "start",
   "guard": "<Log section revision>", "stamp": true,
   "content": "### 2026-09-11: fixtures regenerated\n\nRegenerated from the seeded corpus."}]}
EOF

# 5. Query again; 6. replaying the old guard exits 2 with guard_conflict and
# error.current; 7. re-read with get and retry with the fresh guard.
```

Uncertain commits (exit 3) cannot be produced on demand outside the test
build, so the script does not show one. The response to one is either
`status: "applied", durable: false` (the new bytes are visible) or
`status: "uncertain"`. In both cases `get` the unit and compare it with what
you meant to write before doing anything else.

## Records walkthrough

Run it with `bash examples/records/walkthrough.sh` (set `RIVET` if the binary
is not on `PATH`). The example is a synthetic bundle with one record per file
(§ File records): the schema and binding shown there, four concepts under
`bundle/`, and an `index.md` and `log.md` the binding excludes. The script
copies it to a scratch directory and checks every result. Abbreviated:

```sh
C=bundle/metrics/shelf-turnover.md

# 0. Diagnose the records; the index and the log are excluded, no bytes change.
echo '{"collection": "concepts"}' | markrivet doctor

# 1. Records gone stale before 2027 and not retired: one, with its record guard.
echo '{"collection": "concepts",
       "where": [{"field": "stale_after", "op": "lt", "value": "2027-01-01T00:00:00Z"},
                 {"field": "status", "op": "ne", "value": "retired"}],
       "fields": ["status", "stale_after"]}' | markrivet query

# 2. map hands a writer the same guard.
echo "{\"path\": \"$C\"}" | markrivet map

# 3. Make it current, push stale_after out and record the check, in one write.
#    Only those three lines of the frontmatter change.
echo '{"path": "bundle/metrics/shelf-turnover.md", "collection": "concepts",
       "guard": "<record revision>",
       "set": {"status": "current", "stale_after": "2027-06-30T00:00:00Z"},
       "append": {"verified": {"by": "human:wren", "at": "2026-11-28T09:00:00Z"}}}' | markrivet update-item

# 4. Query again: nothing is stale, and the record reads both verified entries.
# 5. Replaying the old guard exits 2 with guard_conflict and error.current.
# 6. A bare verified mapping reads as one entry; append to it is refused
#    unsupported_syntax and the file is left as it was.
```

## Not implemented

These are design directions, not features:

- An MCP server. The CLI contract comes first; MCP follows once real tool
  overhead is measured.
- Multi-file batches or transactions beyond `move`'s two live paths.
- Field-scope guards (`guard_scope: "field"` is reserved and refused).
- Table-row identity and table-shaped typed collections.
- Positional `delete`, and position handles for lists, blocks or sections
  (a section has `heading_path`; a new item goes in by `insert_item` or a
  section write).
- Setext title edits, frontmatter mapping replacement, block or multiline list writes other than
  `append_key_item`'s, editing or removing a frontmatter list item, and
  mapping field paths deeper than one parent and one scalar field.
- Token counts. Sizes are reported in bytes; no tokenizer is bundled.
- Typed record creation (`insert-item` on a file collection; `create` writes a
  new file), record writes that add a missing mapping or mapping field, replace
  a block list or set an `entries` field, and archives, `ready` waits and
  moves for file records (§ File records, § Record writes).
- General directory creation (`archive-mkdir` creates confined archive parents),
  and adding frontmatter to a document that has none.

## Analytics: rows, counts and captured reports

`analyze` provides version 1 read-only analytics alongside the legacy `query`.
It scans each matching document once. Names are explicit: `item.done`,
`item.actor`, `item.id`, and `document.path`. The last is always present and
identifies the project-relative document. Other document fields need a named
`.rivet/analytics/<name>.json` declaration. Analytics and view files do not
participate in typed item or collection write guards.

The complete synthetic project in `examples/analytics/project/` contains three
trackers and three actions. Two trackers have actions and one queue is empty.
Its declaration is:

```json
{
  "version": 1,
  "collection": "actions",
  "fields": {
    "cluster": {"type": "enum", "key": ["cluster"], "values": ["alpha", "beta", "unused"], "required": true},
    "status": {"type": "enum", "key": ["status"], "values": ["active", "parked"], "default": "active"}
  }
}
```

Metadata keys are paths through the existing frontmatter parser's mappings.
Types are `string`, `boolean`, `number`, `date` and `enum`. Arrays and objects
are invalid field values. `nullable: true` admits an authored null. An omitted
optional key is missing; `default` supplies an effective value only when the
key is genuinely absent. A null default is distinct from no default. Required
missing keys are invalid. Invalid defaults refuse the declaration before a scan.
A field named `path` is reserved. Declarations must name their collection;
multiple declarations for one collection require explicit selection.

Run these requests against that example root:

```sh
markrivet --root examples/analytics/project analyze <<'JSON'
{"collection":"actions","analytics":"census","source":"documents"}
JSON
```

This counts all three documents, including the empty queue. Item rows are the
default; each carries path, line, nullable stay id, revision, binding state and
field envelopes. Envelopes distinguish `presence` (`present`, `missing`,
`unknown`), `origin` (`authored`, `defaulted`, `none`) and `validity` (`valid`,
`invalid`, `unsupported`). Valid effective values appear under `value`,
including explicit null. Invalid inputs appear under `raw` with field errors.
Absent and unsupported values omit `value`. `<field>.origin` is an explicit
provenance selector for filters, projections and groups. An exact declared item
field name takes precedence over that suffix; append another `.origin` to
select provenance of a field whose own name ends with `.origin`.

```sh
markrivet --root examples/analytics/project analyze <<'JSON'
{"collection":"actions","analytics":"census","where":[{"field":"item.done","op":"eq","value":false}],"group_by":["document.cluster"],"measures":{"actions":{"op":"count"},"trackers":{"op":"count_distinct","field":"document.path"}}}
JSON
```

Each category has one open action in one tracker. Metrics apply to the entire
filtered set, before `limit` (default 50, maximum 1,000), `offset` or `top`.
`top` selects groups by descending base-row count, then the ordinary typed-key
order. `total_groups`, `omitted_groups`, `returned_group_rows`, `total` and
coverage counters disclose the page's scope. Distinct-document counts can span
several groups and must not be added to obtain a corpus document count.

`count_where` takes its own `where` clause array. It returns `count`, `false`
and `unknown`; these sum to `base_rows`. `count_distinct` accepts any declared
scalar field and returns a key count plus separate `missing`, `null`, `invalid`
and `unsupported` exclusion counters. Numbers compare exactly, so `1` and
`1.0` share a key. Grouping or distinct counting refuses numbers outside the
legacy exact-comparison exponent range. Values are never coerced across types.

```sh
markrivet --root examples/analytics/project analyze <<'JSON'
{"collection":"actions","analytics":"census","source":"documents","facets":["document.cluster"],"zero_enums":true}
JSON
```

The facet counts are alpha 1, beta 2, unused 0. Facets require enum fields and
apply the supplied filter exactly, including a filter on the faceted field.
`zero_enums` supplies declared zero-count facet values, or the Cartesian product
of enum dimensions for groups. Non-enum zero-count group dimensions are refused.
Bucket objects tag missing, null, invalid and unsupported states, so literal
enum strings cannot collide with them. Key ordering is missing, null, invalid,
unsupported, Boolean, number, string; numbers sort numerically, strings sort
lexically, and multiple dimensions compare from left to right.

### Complete censuses and diagnostic reads

Strict mode is the default. It validates candidates before filtering and refuses
invalid item fields, invalid declared metadata, unreadable or unparseable files,
ambiguous sections, and traversal errors whose scope is matching or unknown.
A false filter cannot hide an invalid candidate. A missing section is refused
unless `missing_binding: "allow"` is explicit. That policy includes missing-section
documents for document rows and contributes zero items for item rows. Diagnostic
mode (`mode: "diagnostic"`) retains invalid field envelopes and diagnostics.
Ambiguous sections contribute a diagnostic document row and no invented items.

Comparisons on invalid or unsupported values are unknown. AND is false if any
clause is false, otherwise unknown if any clause is unknown. `exists` and
`missing` inspect established presence, including invalid values; unsupported
presence is unknown. Other comparisons on valid missing optional fields are
false, including `ne`. Defaulted values are present, matching legacy query
semantics. `unknown_predicates` is separate from nonmatching rows.

Coverage distinguishes discovered/read/failed/unreadable files, binding failures,
candidate/valid/invalid items and rows, matches, unknown predicates, addressable
items and duplicate ids. Unstamped items remain countable. Duplicate identities
are diagnostic and impair addressability without merging source locations.
Strict refusals include structured coverage evidence as JSON in `error.current`.

The descriptor-confined walker never follows symlinks. It reports hidden,
`target`, `node_modules`, symlink, nonregular and non-UTF-8 exclusions, along with
pruned directories and enumeration errors. Native filenames have a reversible
hex encoding separate from `document.path`. Excluded/unlisted directory
contents, and whatever an excluded symlink names, have an unknown count
(`descendants_unknown`). These scope exclusions do not constitute a
census of the entire filesystem. Unsupported frontmatter never takes defaults;
a leading fence with a YAML-looking line but failed recognition is unknown.
A horizontal rule followed immediately by a blank line, or only prose, is absent
frontmatter and can take defaults. Legacy parsing and guards retain their rules.

### Saved views, export, replay and drilldown

A view in `.rivet/views/<name>.json` contains `version: 1` and `query` with the
same bounded fields as `analyze`. It contains no script or implicit clock.

```sh
markrivet --root examples/analytics/project run-view <<'JSON'
{"view":"open-actions"}
JSON
```

`export` accepts exactly one direct `query` or saved `view` and a plain bundle
name. It creates `.rivet/exports/<bundle>/` atomically and refuses an existing
name. It writes bundle artifacts; source Markdown stays unchanged.

```sh
markrivet --root examples/analytics/project export <<'JSON'
{"bundle":"open-actions-capture","view":"open-actions"}
JSON
markrivet --root examples/analytics/project replay <<'JSON'
{"bundle":"open-actions-capture"}
JSON
```

The export response includes `report`, scan start/end (Unix epoch milliseconds),
`bundle_digest` and `consistency: "per_file"`. Replay returns the same report,
using captured source bytes, schema, binding, declaration and view definitions.
It opens no live Markdown or configuration. It reparses complete captures and
compares outcomes, records, coverage, fingerprints and content hashes. Read or
oversize failures without a complete capture, and traversal events, replay as
recorded observations. Changed engine build, parser or analytics versions and
malformed/corrupted bundle members refuse replay. Capture is a sequence of
per-file observations, not a multi-file transaction.

`drilldown` accepts the original `query`, report `fingerprint`, tagged `group`
key array and `measure` name. For a live saved-view report also supply `view`;
for captured inputs supply `bundle`. A live fingerprint mismatch refuses with
`external_change`. Count returns included rows. Conditional count returns
true/false/unknown partitions. Distinct count returns keys with their contributing
rows and separate excluded-state partitions; reconcile key count rather than
contributing-row count. Full row envelopes and source locations remain visible.

Bounds are 8 MiB per source/config/member, 256 MiB captured source bytes, 10,000
files, 8 MiB each for manifest and records, 100,000 retained rows or distinct
keys per measure/group, 10,000 groups, 160,000 walker events, 32 MiB retained
typed state, eight group dimensions, 32 measures/facets, and 64 clauses per
filter. Scan diagnostics are capped at 100,000 entries and 8 MiB total, with a
64 KiB limit per diagnostic, including errors on nonmatching rows. Duplicate
facet fields are refused. Exceeding a bound refuses the operation; no completed
partial bundle is published. A failed artifact publication may leave an
incomplete hidden staging directory under `.rivet/exports/`.

The CLI analytics tests exercise these example files and requests. Run the named
acceptance gates with `python3 examples/analytics/check-tests.py --phase all`;
`--self-test` verifies the checker's own missing/ignored/wrong-target refusals.

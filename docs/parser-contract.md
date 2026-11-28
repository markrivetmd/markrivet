<!-- markdownlint-configure-file
{"MD013": {"code_blocks": false, "tables": false}}
-->

# Implementation contract

This is the contract `markrivet-core` implements and its tests enforce, for
markrivet (Rivet for short). The user contract (commands, requests, exit
codes) is [usage.md](usage.md).

## Source and spans

Every span is a half-open UTF-8 byte extent into the original file bytes.
The file is never re-serialised: writes are splices of original bytes, and
all bytes outside a write's splices survive unchanged.

- Parser: comrak 0.55.0, CommonMark with the task list extension, byte
  columns. comrak's inclusive ends become half-open ends. A container that
  precedes a blank line reports its end at column 0 of that blank line;
  Rivet treats it as ending on the line before.
- Blocks, items and sections use whole source lines, excluding the last
  line's terminator. LF and CRLF are both preserved; inserted text uses the
  newline of the file's first line. Lone CR is refused.
- A leading BOM is excluded from parsing and kept in the file.
- `inspect` is a separate read-only view that also enables GFM tables and
  reports parser node extents. Its nested extents keep container prefixes and
  are not mutation targets.

## Frontmatter

Recognition follows markstay §5.3 on LF-split lines: line 1 (after an
optional BOM) is `---` with optional trailing spaces or tabs; the first later
`---` or `...` line closes the span; the payload is non-empty, has no blank
line, and has at least one YAML-looking line. If any condition fails there is
no frontmatter and nothing is rescanned. Skipping a BOM before `---` is a
Rivet addition; markstay does not recognise frontmatter after a BOM.

Inside recognised frontmatter, `yaml-rust2` 0.13.0 validates each field and a
local lexical scan supplies exact value spans. Keys are unique and top level.

**Writable**: single-line scalars, being plain or quoted strings, lowercase
JSON booleans and null, JSON numbers with arbitrary precision, and a calendar
date, which reads as its own text and writes back unquoted. A key write
renders one scalar, keeps the old quoting when it decodes to the same value,
and re-parses the line. String values matching YAML 1.1 boolean spellings
(`y`, `n`, `yes`, `no`, `on`, `off`, `true`, `false`) or null spellings (`~`,
`null`, or empty), in any capitalization, render quoted. This applies to
both setting a key and inserting one.

**Readable collections and empty values**: an empty value, and flow and block sequences and mappings at either indentation, nested to a bounded depth,
read by recursive descent over the scanner's tokens. A `key: scalar` line is a
whole field; only a key YAML leaves open takes continuation lines, so an
indented comment after a scalar is a comment rather than part of its value.
Collections are read because a real tracker's ordinary frontmatter otherwise
makes the whole block unreadable. Whole-collection `set` writes are refused;
`Scalar::writable` carries that distinction and `map` reports it. The same
recursive reader records the byte spans and quote styles of immediate scalar
fields in a mapping. `set_key_field` uses exactly two literal key path segments,
checks the parent key's guard, and splices only the selected scalar. Rendering
uses the shared scalar candidates and re-parses the original mapping context,
checking its complete decoded value and the new scalar's exact extent. This
keeps flow punctuation inside strings quoted and every neighbouring byte intact.
Empty values and collection-valued fields remain refused as field write targets.

**Raw YAML token writes**: `add_key_yaml` and `set_key_yaml` copy a caller's
token, never rendering one, and decide eligibility per write rather than
through `Scalar::writable`, which keeps describing `set`. The token is read as
the value of a synthetic `k: TOKEN` line by `key_line_token`, which must
consume it exactly: a scalar through the same line parser as a writable key, a
flow sequence through the same recursive reader, flat and on one line. The
read spans are not reused for the target, because a flow collection's read
extent runs to the field's end and can take in a comment, later comment lines
or more value lines. Instead `key_line_token` reads the key's first line alone
and returns the token's exact span; for a sequence that is the scanner's own
`[` and `]` tokens, whose columns count characters and are converted to byte
offsets and checked against the source. After the token only whitespace and a
comment may follow on that line, and any later lines the reader gave the field
must be standalone comments; anything else is refused. The splice replaces the
token alone, or, with `replace_line`, the token through the end of the line's
content, excluding its terminator. An add inserts one line at the payload's
end exactly as `add_key` does, sharing its key-name and absence rules. Before
planning, the candidate payload is read again by the unchanged frontmatter
reader and must give the same keys in the same order, with only this key's
value replaced or appended, and the key's first line must re-read with the
token where it was put. A quoted key may hold characters the collection reader
refuses anywhere, so such a key can take a scalar but not a list.

The intent gate does not trust the planner's splice. Each operation's
expectation derives the one permitted splice, extent and replacement text, from
the preimage and the request again, and requires the change to consist of
exactly that splice. After the candidate is parsed, its whole frontmatter must
equal the preimage's with that splice applied, the key's first line must equal
the derived line and re-read as the token and value, the field count must be
the original plus the added key, and every other field keeps its value and
source lines. The guard is checked before planning, so an unchanged result
never skips it.
**Sequence item appends**: `append_key_item` decides its own target and guard
kind from the locked preimage: the key's revision when the key exists, the
document revision when it does not. For an existing key the decoded value must
be an array of objects whose values are scalars, and `sequence_tail` re-scans
the field's tokens to find where one more item goes. A flow sequence's `[` and
`]` must both sit on the key's line; the item goes after the last top-level
`{...}`, located by the scanner's own `}` token (columns converted to bytes and
checked against the source), after the separator found between the last two
items, or right after `[` when there are none. Every top-level entry of a
block sequence must be a flow mapping closing on its `-` line, or a block
mapping whose first key shares that line, with only spaces between the
indentation, the `-` and the item; the last entry decides the form. The new
item repeats that line's prefix and
goes at the end of the field's lines less trailing standalone comments, the
same extent `delete_key` removes. Field values are rendered by
`render_mapping_field`, the renderer `set_key_field` uses, in a one-field
mapping of the item's own flow or block form, with the last item's quote style
for the same key; item keys share `add_key`'s name rule and are written plain,
so they must decode as text. An absent key is added with `add_key`'s site and
rules as one `key: [{...}]` line. Before planning, the candidate payload is read
again by the unchanged reader and must give the same keys in the same order,
with this key's value equal to the preimage's array plus the item, or appended
holding only the item. The intent gate derives the splice again from the
preimage and request, requires the change to be exactly that splice, requires
the candidate's frontmatter to equal the preimage's with it applied, and checks
the key's value, the field count and every other field's value and lines.

`delete_key` checks the selected key's guard and removes its whole parsed field
lines including the last terminator, leaving trailing standalone comments in
place. Deleting the last key is refused because without a YAML-looking line
the fences would become Markdown. A flow collection must alternate
entry and `,`, so `[a,,b]` is unsupported rather than silently two entries. The
scanner folds a scalar that continues onto the next line into one token with
nothing marking it, so every scalar in a collection is checked against its
source line: a plain one must appear there as decoded and a quoted one must
close, or the field is refused rather than read as a value its author did not
write on one line.

**Unsupported**, making the whole frontmatter `unsupported` with no partial
fields, at any depth: aliases, anchors, merges, tags, complex keys, a flow
mapping key with no value, block and multiline scalars, duplicate keys (a
top-level one named in the diagnostic), a plain key that does not decode as a
string on the same terms as a value (so `0x20` and `32`, one integer to YAML,
never become two JSON keys), and a token only YAML reads as a number (hex,
octal, binary, leading-zero and underscored integers, sexagesimal, `.inf`,
`.nan`, and the floats JSON refuses). Those are never coerced in either
direction. A token that merely starts with a digit is not one of them:
`1Password`, `6.9.6`, `../plans/X.md`, `1:99` (a sexagesimal group must be under
60) and `+_1` are strings, as YAML reads them. Unsupported frontmatter blocks
frontmatter reads and key writes only; body writes still work.

## Indented code at the end of a list item

When an indented code block is the last thing in a list item and a blank line
closes the item, comrak reports the block ending one byte into its first line,
and the item inherits that end. `Document::parse` takes the block's `literal`
as a floor for its extent and for every ancestor's, so the item keeps every
content line of the block. The trailing blank lines stay outside, as
CommonMark §4.4 excludes them from the block.

## Known limitation: a consumed final indentation

`Document::parse` derives a block's extent from comrak's `sourcepos` and, for
HTML blocks and fenced code blocks, from the `literal` it retains. comrak can
trim **both** for the same block, so no arithmetic over the two recovers the
line. The smallest case is six bytes:

```text
- <?\n␠
```

A final line that is only the container's indentation, with no terminating
newline. comrak consumes it as list indentation, so it leaves `literal`, and
HTML finalization recomputes `sourcepos.end` from the trimmed content, so it
leaves that too. The list and item spans cover `- <?` and not the space.

**What this does and does not cost.** The document revision covers the byte,
so a document-guarded write still sees a change to it, and every write splices
into the source it read, so the byte is never lost. What a block- or
item-scoped guard cannot do is detect a concurrent change to it.

Closing this needs a block-boundary scanner over the source rather than a
reading of comrak's positions. A narrower repair was tried and reverted: a
rule that gives the partial final line to the *last* block is position
dependent, so a write that appends a marker changes which block is last and
the candidate then fails its own shape check.

## markstay profile

Rivet pins the markstay specification v1.10, `SPEC.md` SHA-256
`9dcbc452f10120af3c97ca20dd79b4d98eb77dcabc363f8abaad5a3f902cba1a`, and the
`markstay` crate 0.15.0 (repository revision
`d000a95bc1ac8717961a9be73b792f8131d09270`) for the marker grammar (§4) and §8
digests. That crate implements v1.11, whose changes (§9.1 exclusive assignment,
§11 relocated ids, a §5.3 writer refusal) leave the grammar, identity and
hashing as v1.10 has them. Rivet uses only the crate's marker scanner, fence
mask, digest and id minting, and makes no v1.11 claim. The crate keeps marker offsets private, so `markers.rs` locates each
marker's bytes on its line and re-parses its attributes, refusing the document
when its reading disagrees with the crate's.

v1.9 adds one linter requirement, `DROPPED_CHILD_ID` for an item stay that an
edit removes, and leaves readers, writers, the grammar and digests unchanged.
Rivet runs no linter. It refuses a write that loses or re-attaches a uniquely
resolvable stay attached to a heading, a block, or a direct item of a top-level
list, unless the write declares that stay dropped (a `delete`, or
`allow_drop`). A section-body `set` and a section or block `delete` also
declare the id of every live marker Rivet does not address in the region (a
nested item's, a table row's, a marker-only line carrying `subhash`) unless a
marker outside the region still carries that id, so the body `set` needs
`allow_drop` to remove one and lists it under `dropped`. A block `set` declares
every marker in the block it does not own, survivor elsewhere or not. A
whole-item replacement, an item delete and a title write refuse a marker their
unit does not own; a checkbox-only update changes one byte and moves none.
Every structural delete checks its removal span against `target_span`, the
bytes its guard covers. Any prefix or suffix outside that guard must contain
only whitespace; non-whitespace refuses with `unsupported_syntax` before a
candidate is planned. Delete receipt replay and archive-copy source validation
use the same delete planner. An item delete with explicit `list: {guard}`
requires a sole item and that list's current native revision, then uses the
list block's guarded delete span. The item foreign-marker rule still applies;
markers outside the item must attach to that exact list or the writer refuses
`list_trailer_refused`. Both item and list stays are declared dropped.
Without `list`, sole-item deletion refuses. This opt-in is carried through the
independent expectation recheck and version-2 receipt replay. Archive-copy
supplies the current list revision for its structural source check; the
delete's archive precondition uses the delete's explicit list authority.
A live list delete additionally checks all citation-scope files at planning,
publication and pending-delete recovery, refusing `list_citation_refused` on
any list-id mention or incomplete walk. Replay and archive-copy stay structural.
Across the whole batch, an id no live marker carries after the write must be
one the batch declared, so removing a live marker that v1.9 would report as
`DROPPED_CHILD_ID` is never left undeclared. markstay's linter can also count
marker text Rivet reads as content, such as a marker quoted in a code span;
removing that is an ordinary content edit here.

v1.10 adds the linter's in-place adoption rule (markstay `SPEC.md` §16): a
bare stay rewritten where it stands as its list item's child stay keeps its id
and is reported as the warning `ADOPTED_ID` rather than `DROPPED_ID`. The rule
is lexical and fails closed: the marker must share its line with the item's
content and the block must be otherwise unchanged. So not every adoption
`adopt_stay` accepts passes it: a stay on its own line inside the item, one in
a table inside the item, or an adoption batched with another change to the same
list still reports `DROPPED_ID`. Readers, writers, the grammar and digests are
unchanged.

The attachment profile is §5.2 CommonMark-tree segmentation with §5.5 list
children:

- A marker is **live** only when it is exactly an inline HTML node or lies in
  an HTML block. Marker-shaped text in fenced or indented code, code spans,
  link text, and inline HTML such as a tag's attribute is content. This
  follows the tree adapter's reading (§5.4 leaves the non-fence cases to the
  tool). Inside an HTML block recognition is lexical, as it is for every
  markstay reader, so a marker-shaped attribute value there is a live marker.
- Top-level CommonMark nodes are blocks. An HTML block holding only markers
  binds its markers to the preceding content block, across blank lines, or is
  an orphan. Markers inside a block without `subhash` bind to that block.
- Direct items of a top-level list are child blocks. A `subhash` marker on an
  item's lines, outside nested items, is that item's stay. Any other
  `subhash` marker (nested item, paragraph, blockquote, table, marker-only
  line) addresses nothing and is never reported as a container's stay (§16).
- Table-row identity (§5.6, the optional `rows` profile) is declined. Tables
  are paragraphs for identity and writes.
- A heading's stay identifies its section. Stay ids share one namespace per
  document.
- Block digests hash the block's lines with live markers removed and ASCII
  trimmed; child digests use the §5.5 child body ported from the markstay
  Python reference, including its prefix rule and tab stops.
- Multi-line or overlapping markers make the document unreadable; a live
  MDX-form marker makes it read-only.

`crates/markrivet-core/tests/markstay_conformance.rs` checks this profile against
markstay's tree-tier vectors, its §8 hash vectors, and child vectors generated
from the Python reference linter (`--commonmark --child-blocks`).

Block digests are this profile's and no other's. markstay's CLI (`lint`,
`stamp`, `restamp`) and its pre-commit hook segment by §5.1 blank lines unless
given `--commonmark` (`MARKSTAY_MODE=commonmark` for the hook). On a document
inside §5.4's agreement subset the two profiles give every stay the same
digest. Outside it, one stay can have a different correct digest under each.
The two common shapes are an ATX heading with no blank line above it, and a
loose list carrying a whole-list marker. There a digest Rivet refreshed reads
as `HASH_DRIFT` to a default `markstay lint`, and a digest a default
`markstay restamp` wrote reads as drift to Rivet. Neither computes wrongly.
`profile-divergence.json`, generated with the other vectors, pins both values
on those two shapes and on a control inside the subset, and
`block_digests_follow_the_tree_profile` holds Rivet to the tree value. Lint
and restamp a Rivet-bound document with `--commonmark --child-blocks`, the
reference profile these vectors come from.

## Identity policy

A heading's native identity is a markstay marker on a marker-only line right
after the heading. An existing `{#id}` attribute stays byte-for-byte intact
and serves as the heading's identity when it has no stay. When both exist
they must agree; a disagreement is a `duplicate_id` diagnostic and neither id
resolves. `stamp` adopts an `{#id}` as the new stay's id. Reads never insert
ids, and Rivet never recovers a missing or duplicated id by similarity.

## Revisions

`revision(kind, identity, bytes, dependencies)` is SHA-256 over length-framed
fields: the domain `rivet:revision:v1`, the kind, the identity (the unit's
first stay id or empty), the unit's current bytes, then any dependencies. The
string form is `rivet:v1:<kind>:<hex>`, the digest truncated to its leading
128 bits, which is what a guard needs to detect concurrent change. Stored
marker digests are never
inputs. Kinds and covered bytes are listed in usage.md. Typed `action` and
`collection` revisions add the collection name, one binding fingerprint and
the schema file's SHA-256. `schema::bind_bytes` computes the fingerprint once
on `Bound`: SHA-256 of compact JSON `["rivet-binding-v1", schema, files,
section, level, subsections, exclude, archive]`, including explicit defaults.
The schema key includes its version. The three list-valued settings are sorted
and deduplicated literal strings; title/glob spellings remain exact. Both flat
and descendant collection guards use this fingerprint. Descendant content is
each member heading and own body framed by length, excluding nonmember content.
Query cursors and restore binding checks reuse the same fingerprint. Old typed
guards conflict after upgrading; native revisions retain their inputs.

A schema's `item` decides the binding's shape. `ItemShape::File` bindings carry
`files` and `exclude_files` only (`CollectionConfig.section` is an `Option`, so
a checkbox binding's serialized config and fingerprint keep their bytes), and
their fingerprint is SHA-256 of compact JSON `["rivet-file-binding-v1", schema,
files, exclude_files]`, both lists sorted and deduplicated. Membership is one
predicate, `CollectionConfig::selects`: `files` matches and no `exclude_files`
glob does. Every site deciding whether a path is in a collection asks it;
archive-overlap validation and the walker's globs keep the raw patterns, which
can only refuse or visit more. Citation scope classifies each found file with
`ProjectConfig::live_member` (some collection selects it or a section archive's
`files` match it), dropping a file only an `exclude_files` entry keeps out.
A file record's fields come from `Source::Key`: a top-level frontmatter key read
through `yaml::recognize`'s scalars, or one field of its one-level mapping, with
a bare flat mapping read as a one-entry `entries` value. `schema::record`
returns the fields or a `RecordError` (no frontmatter, unsupported frontmatter,
or field errors). The `record` revision frames the record's path as identity
and `doc.source[frontmatter.span]` as bytes, with the collection name, binding
fingerprint and schema digest as dependencies. `schema::instance` refuses a file
binding with `invalid_request`, so a checkbox-only reader that misses its
`is_file()` check fails loudly instead of reporting a missing heading; sites
that iterate a path's bindings (collection stamping, archive source checks,
move membership, status) skip file bindings first.

A record write (`update_item` on a file binding) is one change, not a batch of
native key ops, which could not share a frontmatter. `record_edits` derives
every splice from the preimage and the request alone, through the renderers
and shape checks the native key ops use (`scalar_token`, `mapping_field_token`,
`key_token_site` with `yaml_set` for a flow list, `new_key_line` and `yaml_add`,
`frontmatter_key_lines` for a removal): one edit per touched key inside its own
lines, sibling mapping fields composed on their key's lines and re-read as one
mapping, and added keys merged into one insertion at the payload end by key
name, coalesced with a removal ending or starting there. A field whose stored
value equals the request is left alone, so a request of current values has no
splice. The candidate payload is read back before planning returns.
`Expectation::Record` carries the request (binding, path, `set`) and the
expected fields, and re-derives the splices at every stage: `allowed` returns
exactly their spans, `validate` requires the change's splices to equal them in
span and text before a candidate is built (which refuses a byte-equivalent
forgery that containment and the candidate comparison would accept), and the
candidate check requires every byte before the frontmatter unchanged, the
frontmatter equal to the preimage's with exactly those splices applied, its keys
in the derived order with the derived values, every untouched key's value and
source lines kept (a removed key's trailing comments may extend its
predecessor), and `schema::record` re-reading the expected fields. The body is
left to the batch's other expectations and the whole-candidate checks, so a
record write and a body op combine; `change.frontmatter` keeps any other
frontmatter op out of the batch. Its guard subject is `GuardSubject::Record`,
the `record` revision at the write's path, which authorizes only a
`Expectation::Record` of the same binding and path; a native document or key
guard never authorizes one. The follow-up guard is the candidate's `record`
revision.

Guard scope for typed writes is `action` (the whole item) by default and the
only accepted value. `field` is reserved: it would need a field revision plus
schema, binding and structural dependencies, and whole-item invariant checks
under the lock. It is refused until those exist with tests. A scope travels
with its guard; a changed scope cannot reinterpret an old digest.

## Planning and verification

The edit, create, adopt, archive-copy and restore paths enter one write
transaction. It checks the message before acquiring the project lock, reads the
commit policy under that lock, handles pending ownership, loads the document
and bindings, checks the candidate size, and publishes through the common
commit or filesystem landing. Previews take no lock and settle no records.
Restore refuses pending ownership even in preview, without recovery. Adopt
lands equal bytes when an outside change still needs committing. Archive-copy
proves its source binding and checks for a committed retry before loading the
destination; unrelated destination collection schemas do not govern the copy.

Guarded adopt supplies a strict `expect` certificate and enforces the
[adopt contract](usage.md#adopt). It refuses pending ownership without
settlement, checks the loaded revision and requires byte-identical stamp
planning, then checks HEAD and the certified mode after preflight but before
bookkeeping or a clean no-op. The pre-rename check also pins the branch and
index entry. Its record carries an optional `expect_head`, owned once by
`RecordData` and omitted for plain writes. Recovery checks that HEAD before
assigning `base_head`; the commit routine checks their equality before even
the equal-blob shortcut. A mismatch retains `repair_required` with reason
`guard_head_moved`. Already-committed history is verified before recovery
considers retrying. The [adopt contract](usage.md#adopt) describes the refusal
bookkeeping, editor boundary and older-engine rollback constraint.

Item moves use the paired transaction under the same project lock. Both
candidates are planned before any record or fence is published. The source
uses the existing guarded collection delete; the destination uses
`Expectation::Move` with the shared item end position or new-list frame.
Its allowance imports only the proved source item's exact bytes and identities,
declares only the empty destination's fresh list stay as minted, and verifies
the owning list and member after parsing. Whole-pair verification checks list
shape, all collection memberships and typed fields before durable publication.

Section moves use native section deletion and `Expectation::Move` placement
computed again by the expectation's allowance and splice validation. The
destination's stamped parent is exactly one level above the source unit.
Placement is at its whole-subtree end or the heading-line start of `before`,
an existing direct child of that parent. The content extent has its own final
terminator, with framing blank lines outside it. Relative marker occurrences
and their block/item owners are checked by the same provenance proof as
section archive; heading levels are unchanged. The engine refuses titles,
bound queues and members, and ancestors containing them. It treats Log as
an ordinary section title.

Pair verification keys existing collection instances and members by their
identities rather than shifted section indices. Every moved descendant section
and checkbox item's collection set is compared in document order before and
after, with ancestor exclusions applied on both sides. Candidate bindings that
create or ambiguate an instance refuse `membership_changed`. Existing list
shapes outside the subtree stay equal; the imported subtree stays byte-exact.

All operations in a request resolve against one locked read. Each produces
splices and a required typed expectation bound to its guard subject. Before
applying any splice, planning rechecks the document's write blocker and the
guard, derives the editable units and marker policy from that expectation,
and checks source bounds, UTF-8 boundaries, splice containment and candidate
item-region descriptors. A valid guard for one unit cannot authorize another
unit's edit; typed guards also bind collection membership and schema identity.
A native delete with a collection precondition carries its resolved binding in
the delete expectation. Planning rechecks membership against the locked source
document independently of the native byte guard; that project precondition is
not part of the portable delete receipt.
A bound-body `set` carries both the native item guard and typed item guard in
its guard subject. Mandatory preflight independently checks both against the
locked document and resolved binding, authorizes only that item's field
expectation and re-extracts the complete candidate under the same schema.
A checkbox-only collection `stamp` carries both document and collection guards.
Its expectation retains exact member-section spans and excludes headings and
non-checkbox item carriers from allowed insertions. It reuses native carrier
placement and whole-candidate verification, and refuses any unsupported
selected carrier atomically. No schema extraction is required to stamp.
Builders use the same preflight for early refusals with operation context;
guard checks precede content rendering and substring search. Missing guards or
expectations are refused, including on no-ops. Splices must not overlap, and
operations may not share a unit. Then:

1. Apply the splices. Stored digests of every stay whose unit body changed
   (and of minted stays) are brought current at their stored precision; a stale
   digest before the write is reported as `prior_drift`, even when the write
   restores the bytes the stored digest already describes.
2. Re-parse the complete candidate and require: every untouched top-level
   block reappears byte-identical at its mapped offset with the same kind and
   stays at their mapped marker spans; every edited list holds exactly its
   untouched items (byte-identical, with live marker spans preserved)
   plus the declared new or rewritten items, with no split or merge; every
   surviving stay keeps its kind of target, except an id `adopt_stay`
   declares, which must move from its list to exactly its own item; dropped
   ids are gone; no unknown
   ids appear; orphan, unaddressed, malformed, duplicate and unsupported
   diagnostics do not increase; untouched frontmatter is byte-identical.
3. markstay §3.4: a child stay the write places, moves or mints must have
   plain carrier text (its list's raw source up to the marker, with plain
   markers masked and then the content of closed inline code spans masked,
   holds no `<` or backslash). The code-span mask is v1.8 and applies only at
   §5.5 child carriers, which are the only carriers Rivet writes; §5.6's row
   carrier keeps the unmasked presence rule. Pairing is by equal-length
   backtick run, within one line, dropping runs on lines §3.3 recognises as
   fenced code, and it stops at the first line whose runs do not pair evenly.
   §3.3 recognises a fence indented up to three spaces; at four it is an
   indented code block rather than a fence, so its backticks pair like any
   others and the span between them masks. Rivet answers every stay of a
   list from one forward pass over that list's source; the rule above, applied
   to the list's source up to each marker, remains the definition, and tests
   compare the pass with it at every prefix. A stay left in place
   may not lose a plain carrier because of the write. Moving a stay to the end
   of an edited item's last paragraph counts as insertion. A checkbox-only
   typed write is an in-place region: it splices the one symbol byte, moves no
   stay, and so places nothing for this rule to judge. `adopt_stay` turns a
   block stay into a child stay in place, which counts as placing it.
4. Run each operation's typed expectation, such as an edited item re-reading as
   exactly the requested content, or typed fields re-extracting as requested.

Step 1 refreshes only units a write changes. Drift in a unit no write touches
stays until a `refresh` op names it. That op is the only op in its write, under
the document guard. It validates every named stay before splicing any digest: one
live marker carries the id (orphaned, unaddressed and HTML-block claims and a
heading's `{#id}` all count), the marker addresses a block or item, its
attributes are plain, a digest of the attachment's key (`hash` or `subhash`) is
stored at 1 to 64 hex digits, and it equals the caller's `expect`. Its splices
replace only those digest values at stored precision, and the blocks holding them
skip step 2's whole-block byte comparison. Its expectation then requires the whole
candidate to equal the preimage with exactly those values replaced, every live
marker at the same span with the same id, attachment, attributes and unnamed
digests, the lexical markers, identities and diagnostics unchanged, and each
refreshed digest current. `doctor` classifies evidence with the rule archive and
move apply (an item stay needs plain attributes and a stored `subhash`; a stored
digest must prefix the current one), and reports whether `refresh` could rewrite
it with the same selection checks.

Only a candidate that passes every check reaches the filesystem.

### Portable deletion evidence and archive provenance

`delete_receipt` version 1 carries the original typed single-delete intent
from API planning into `commit::Write`, the pending Record, commit message
and Event. It is emitted only for a clean eligible single-delete commit.
Legacy/no-receipt fields are omitted, and off-mode output keeps its shape.
The whole canonical decoded JSON is capped at 65,536 bytes before message,
Record or document publication. Recovery rechecks that budget when rebinding.

The reserved `Rivet-Delete` trailer is unpadded base64url of fixed-field-order
compact JSON. Receipt structs reject unknown and duplicate keys. Version,
canonical path, full object ids, regular unchanged modes, SHA-256 hashes and
unique dropped ids are validated. Recovery's transaction-shape check requires
exactly one trailer equal to the typed Record, in addition to the existing
parent/path/blob/op checks. User text is separately persisted and validated;
recovery never derives intent from a caller-supplied or stale trailer.

Before retrying on a later unrelated HEAD, recovery verifies the original
preimage/candidate by pure replay, rechecks archive/policy preconditions,
binds the receipt to the final checked parent/base entry, recomposes the
message from user text and typed data, and durably publishes the amended
Record before any Git mutation child runs. Failed publication leaves pending
repair state and the actual candidate intact. Already committed transactions
are verified and settled before retry preconditions are considered.
Serialized pending records use a separate 40 MiB producer/reader bound, so
the separate user/composed messages remain recoverable. Unreadable records
refuse new writes before their ownership can be established.

The pure `verify-delete` command parses parent bytes, candidate bytes and a
receipt directly from stdin without a project. It runs `Planner::structural` and
`edit::plan` for exactly one delete, checks both byte hashes and the exact
dropped set, and compares every candidate byte, including evidence refreshes. It
does not validate repository objects; its caller owns raw Git identity and diff
checks. Raw transport is capped at 100 MiB, decoded documents at 8 MiB each,
canonical decoded receipt JSON at 65,536 bytes and combined non-document
framing/metadata at 1 MiB. Unicode escaping in receipt transport consumes that
framing budget rather than changing canonical receipt eligibility. Other
commands retain 16 MiB. CLI validation rejects duplicate JSON fields before
Value conversion.

`archive-copy` and `move` are the confined import allowances for existing marker bytes.
It accepts source identity/guard and destination identity/guard, never caller
content. Under the project lock it proves native deletability and the binding
for an item or stayed section subtree. Items keep their original span.
Collection items keep their collection archive scope; a heading item (a direct
plain bullet of a heading a section binding's `items` names by exact path under
the level-1 title) searches every archive namespace for duplicates. Sections use `ids_in(whole)`, exactly as native section
delete does, and a content extent excluding only trailing physical CommonMark
blank lines owned by no block. Reference definitions and every other nonblank
line remain. An unterminated final physical line, unclosed fence or terminal
parser-owned HTML refuses. A legacy-only child heading, unaddressed or
outside-duplicated stay, or drifted marker evidence refuses in both import
paths. Section archive also refuses any task-list item and overlap with any
bound collection's whole protected subtree. Section move accepts task-list
content when every descendant retains its collection membership.
The shared native import check requires supported plain attributes and valid
child evidence on every item-owned stay, including aliases; a scanner-recognized
but invalid digest is refused rather than treated as missing evidence.

Imported stays get a separate internal provenance allowance keyed by id, with
actual attachment kinds, source-relative marker positions and owning-unit
extents retained for verification. Every copied identity resolves uniquely
in both committed and current archives. Source rechecks compare against the
frozen candidate, including during recovery. The usual complete-candidate
and carrier checks run. The copied content must reread byte-identically with
the native unit ids and attachments; no digest rewrite, foreign carrier or
newline conversion is accepted. A section is exactly one heading level below
its selected destination and rereads as its direct child. Its separator lies
outside its content extent, so subsequent sibling appends keep it stable. At end
of file the separator is the source unit's own excluded blank lines, so the
copied section's whole span, and its native revision, equal the source's. Every
untouched destination unit retains its bytes, kind, mapped marker positions
and attachment.

The source guard is reread before destination publication. Source bytes are
never replaced. Section copy and archive-backed deletion require a stamped
direct-parent stay checked under the writer lock. The copy's expected parent
persists separately from its unchanged v1 receipt in optional pending metadata,
omitted for collection items; every source recheck reads it. Collection item
requests and archive guards carrying a parent refuse, as before. A heading item
requires its stamped heading as parent and is never its list's sole item nor
carries list authority: one validation, with the source kind derived from the
project rather than the request, runs on copy, retry, guarded delete and both
recoveries, so no list-version receipt can arise for it. Item placement in the
destination's own body is one shared rule for planning and verification, for
collection and heading items alike: appended to the own body's final block when
that block is a list with the same marker delimiter, otherwise a new list
before any child section. Per-write/refuse eligibility is mandatory.
`Rivet-Archive` is reserved; its canonical typed metadata persists in the Record
and Event. The v1 `item_sha256` field hashes the content extent for a section.

Idempotent retries require exact current unit bytes/attachment and ancestral
committed copy provenance, rather than matching text alone. Delete's optional
archive precondition requires the original copy and a clean current archive,
checking the individual unit while allowing later surrounding appends when
used alone. A combined `policy_guard` refuses any archive change or HEAD movement,
including a surrounding append. Recovery repeats the same checks; an archive-only
precondition still permits a clean committed append that preserves the unit.

`require_commit` is a native publication precondition. It checks the configured
policy, clean eligibility, index flags, conversion rules, known Git locks and
the final candidate hash before replacement. Archive/policy preconditions
imply it and persist for recovery. Failures after publication report their
actual applied or uncertain state and retain repair records.

`prune-snapshot` version 2 captures immutable UTF-8 inputs under the writer
lock. Scope includes all live collection files, project configuration, bound
schemas, configured policy inputs, and every collection's archive patterns.
`prune.files` independently describes policy discovery; its discovered files
must remain live-bound. Archives are a separate sorted, deduplicated namespace
and need not be live-bound. Captured archive membership and exact raw bytes
must agree with the committed tree at pinned HEAD. Only ordinary blobs with
mode 100644 or 100755 are supported. Index-only archive changes are not policy
inputs when working bytes still agree with HEAD.

The bounded snapshot walk traverses the union of live, policy and archive globs,
pruning unreachable directories. Relevant symlinks, nonregular entries,
unreadable paths and native names refuse. Native-byte relevance considers every
archive filename, including non-Markdown suffixes and committed names absent
from the worktree. Archive reachability of any traversal-excluded entry refuses
before following it, including wildcard matches under hidden/build paths,
`.git` or `.rivet`. Live exclusions retain their existing meaning. Unsupported
archive document suffixes refuse. Schema validation uses captured bytes.

Caps are 8 MiB per document, 64 MiB aggregate raw inputs, 50,000 encountered
entries and 8 MiB cumulative relative native-path bytes. Enumeration counts
unselected entries and refuses before retaining beyond its budget. Snapshot Git
operations all use one shared 64 MiB read budget: tree-list stdout is capped at
1 MiB, blob-size stdout at 128 bytes and blob stdout at 8 MiB. Each child has
a five-second deadline. A whole-tree listing can refuse on unrelated paths;
no whole-capture latency bound is promised. Partial results never authorize a
write. Existing public strict-enumeration APIs retain their unbounded behavior.

The revision is `prune:sha256:` plus SHA-256 of compact JSON
`[2,head,live,policy,archives,manifest]`, with sorted memberships and manifest
keys; each manifest value serializes `sha256` then `bytes`. HEAD is captured
before discovery and branch/HEAD are confirmed unchanged before returning.
Deletion compares a fresh locked capture before replacement. Any HEAD change,
even an unrelated commit or committed A to B to A, or any selected archive
change conflicts. A caller must re-capture and decide again after its own
archive-copy. Version-1 verifiers must adapt; old policy guards are refused.

Recovery substitutes only the verified source preimage. An applied uncommitted
candidate with a mismatching guard, including a version-1 guard, remains pending
as `repair_required`; it is not automatically restored or committed. Already
committed operations settle before current policy is checked. Core implements
no readiness/dependency policy. Immutable reader inputs avoid uncommitted
A to B to A ambiguity. The writer lock serializes cooperating writers; outside
writes after final observation and ref rewinds to the identical OID remain
outside the guarantee.

## Batch list conflicts

A list gaining or losing items is exclusive to one operation in a batch.
Another operation that edits an item, reshapes the same list, or rewrites
the list block is refused with `invalid_request`, in either operation order.
Edits to distinct items of one list and changes to different lists can combine.

## Guarded source replacement in a section

`replace_in_section` resolves a section and checks its guard before any
substring search. It counts exact UTF-8 source occurrences, including overlaps,
in `[section.body.start, section.own_end)`. It never searches descendants.
Empty `old`, CR or LF in either value, and other target kinds are
`invalid_request`; no match is `match_not_found`, more than one is
`ambiguous_match`. Search counts code and markers as raw source too.

The unique span must fit in one paragraph, at top level or inside a list item
or block quote, and overlap no live marker. With `top_level`, a list or quote is
ineligible as well (`unsupported_syntax`), decided by this same write parser, not
by `inspect`, whose GFM tables can place a block's boundaries differently. Container prefixes, code blocks,
HTML blocks and marker-only lines are ineligible
(`unsupported_syntax`; marker overlap is `invalid_content`). A live marker
inside the paragraph's region, including its attached marker-only lines,
must be owned by its containing block or direct list item. Row `subhash` stays are unaddressed under
Rivet's declined optional `rows` profile, so they refuse the edited paragraph
with `unsupported_syntax`, even if the match avoids the marker.

The change plans one splice and marks only the containing block edited.
Candidate verification parses the containing block using the write dialect
(including tables as paragraphs), preserving the containing block's mapped
source extent and every nested block's kind and parent. This forbids splits,
merges and changes of nesting while permitting inline whitespace edits.
The section owner stays the same. Each live
marker must survive at its mapped original span, attached to the same mapped
block or item;
an exposed quoted copy cannot substitute for a hidden marker with the same id. New
live markers, including ones assembled across splice boundaries, are refused;
untouched blocks and untouched items of partially edited lists also retain
their mapped live-marker spans, including foreign markers and attached
marker-only lines. Reference-definition edits cannot expose a quoted copy
elsewhere in the document in place of an old live marker. Other declared
batch edits and minted stays retain their existing verification rules.
All surviving stay attachments must remain stable. Affected owned stored
hash evidence is refreshed by the existing length-preserving refresh pass.
The section heading and its marker bytes stay intact. Postcondition failures
are `invalid_content`; there is no `allow_drop` and no automatic guard retry.
Equal strings plan no splice after match validation and return unchanged;
empty `new` still needs a surviving paragraph. Both API write batches and
the `replace-in-section` shorthand follow the section for the next revision
and use the existing filesystem and commit protocols, including dirty skips.

`remove_paragraph_in_section` uses the same section guard, exact own-body
search and overlapping-occurrence count. Its nonempty `old` may be multiline
and must equal a whole top-level paragraph's source span, excluding its final
line ending. Partial matches are `invalid_content`; nested paragraphs and
nonparagraphs are `unsupported_syntax`. Any live stay in the paragraph's region,
including attached marker-only lines, refuses removal. One empty splice removes
the paragraph, leaving every surrounding byte intact; verification requires no
replacement bytes and the existing checks preserve all other blocks and stays.
It has no `new` or `allow_drop` field and follows the section for its revision.

## Item history reads

`history::item_log` pins HEAD once and reads literal file entries through the
held-root Git runner. It walks at most 10,000 commits of the first-parent
integration chain, using one extra traversal entry to detect cutoff. Raw
commit headers establish the actual first parent and distinguish real roots
from shallow/missing-parent boundaries. Missing parents never supply a known
absent predecessor. Raw blob reads accept only regular-file modes and probe
immutable object size before allocation; no tree is materialized in the live
root. Replace objects are disabled. Partial/promisor repositories are refused
before history access, and history Git children set `GIT_NO_LAZY_FETCH=1`.

Each distinct document blob is parsed once per request with `Document::parse`.
ID resolution covers the whole document, including conflicts, orphaned stays
and target-kind changes. Missing files/IDs are absent; missing objects,
unsupported input and ambiguous identities are gaps. `item_content` removes
only the item's own carriers. Current source, schema, membership, receipts
and Session metadata are not inputs to discovery. Location records contain
ordered ancestor ID sets and a uniquely stayed list or sole-direct-list
sentinel; unsupported locations are labelled rather than guessed. Discovery is
independent of current eligibility. An optional current-source/configuration read
issues an `item_restore` guard only for a supported live collection item in
per-write mode, with no blocking pending record.

History's opt-in bounded Git runner receives both nonblocking pipes on the
calling thread. Stream/request overflow, pipe error or deadline closes the
pipes and kills/reaps the guarded process group. Existing Git callers retain
their invocation behavior. Repository inspection shares the same request-wide
received-byte budget as object reads.

The independent limits are 10,000 commits, 1,000 distinct blobs, 8 MiB per
blob, 64 MiB cumulative decoded documents, 256 KiB commit metadata, 64 MiB
received stdout/stderr, 64 MiB retained details, and 64 MiB complete serialized
response in either CLI format. Before a response row retains another reference
to cached content, a counting serializer charges its compact and pretty JSON,
including escaping, keys, separators and outer indentation. Envelope space is
reserved for coverage counters, diagnostics and the API's `ok` field. Complete
serialization is counted again before return. Overflow stops at a complete
row and returns partial coverage, never truncated JSON or a complete-history
claim. Counters describe scanned work, including the row that hit a limit.

History reads never enter locking, settlement, replacement or bookkeeping
paths. CLI fixtures compare target bytes, directory entries and all Git and
`.rivet/` files before/after successful, partial and refused reads. Synthetic
fake-Git tests check invocation policy, independent pipe overflows and process
cleanup. A two-blob, many-transition test checks response accounting separately
from decoded document accounting.

## Filesystem protocol

Linux and local filesystems only. The project root is opened once and held
by descriptor; the lock, configuration, schemas and documents are all reached
from it. One advisory lock, `.rivet/lock`, is opened with `O_NOFOLLOW` and
never unlinked, so it survives every target replacement.
Under the lock a write re-reads the file, plans and verifies, then:

1. walks to the file from the project root with `O_NOFOLLOW` directory
   descriptors and refuses symlinks, non-regular files, hard links, other
   owners, setuid/setgid/sticky bits and extended attributes;
2. creates `.<name>.rivet-<pid>-<nanos>-<n>.tmp` with `O_EXCL` in the parent
   descriptor, copies the permission bits, writes and syncs it;
3. refuses with `editor_lock` if `.#<name>` in the parent descriptor is a
   live Emacs lock or does not parse as one (see
   [usage.md](usage.md#write-safety)), then re-reads the target through the
   same descriptor and refuses with `external_change` if it differs from the
   planned preimage;
4. `renameat` over the target, then `fsync` the directory descriptor.

`create` walks to the parent the same way and requires the name to be absent,
writes the temporary file with mode 0644 and no copied bits, checks the Emacs
lock as in step 3, and publishes
it with `renameat2(RENAME_NOREPLACE)`: `EEXIST` there means another writer
created the name first, so the temporary file is removed and the create is
refused as `already_exists` with nothing of it visible.

A candidate larger than the 8 MiB read limit is refused before step 1. Errors
before step 4 remove the temporary file and leave the target unchanged. A
directory sync failure after the rename is reported as applied but not durable
(exit 3). A rename that reports failure while the target no longer holds the
planned preimage is `commit_uncertain` (exit 3). A panic after a rename started
is reported as uncertain. Debug builds honour `RIVET_TEST_PAUSE` and
`RIVET_TEST_FAULT` so tests can order processes and inject failures; release
builds ignore both.

## Git commit publication

Per-write commit construction uses the recorded raw candidate blob and mode,
a private index seeded from the pinned parent, and a single-parent commit object.
It never stages later worktree bytes. The real index lock spans its target-entry
check, replacement preparation, ref CAS and durable index installation; other
staged entries are preserved. Index copies retain their source modification
timestamps so Git's racy stat-cache checks still expose unrelated edits.
Git's prepared ref transaction holds its ref locks
while symbolic HEAD is rechecked before publication. A lost CAS leaves the winner
unchanged and settles a head-moved skip. Failure after ref publication leaves a
published pending record for recovery, including a directory-sync failure after
index rename. Recovery syncs the installed index and its directory under the
index lock before retiring a record whose candidate is already visible.
Index-lock inode/time evidence remains
stable across child launches, while child PID/identity tracks current liveness.
Legacy recovery can store a missing candidate object only from bytes and mode
verified against the record. A detected post-reread worktree change is reported
on the committed outcome without writing those outside bytes.

The ref transaction child is the one Git call without a single deadline. Its
preparation budget starts after its launch is recorded and covers the
transaction input and Git's acknowledgment. Its validation budget starts at the
acknowledgment and is shared by the HEAD recheck's nested Git reads: a read is
not started once it has passed, and a running read ends with it. A recheck that
returns after its budget, even with correct results, cannot authorize
`commit`. Its completion budget starts once, just before `commit` is written,
and covers that write, Git's exit and output collection; an abandoned
transaction instead closes Git's input (Git aborts a prepared transaction on
EOF and removes its locks) and gives that abort the same bound. Only an abort
Git finished, exiting 0 after EOF, reports why it was abandoned; one killed at
its bound is a deadline, one that died by a signal or failed is a Git error, and a head move seen around a killed or unreaped child never settles
the write as a skip. Input writes are nonblocking and check the phase's
absolute end before each write, so a child that stops reading costs that
budget and nothing is written after it. An acknowledgment that arrives after
the preparation budget is refused, and the validation budget starts at the
acknowledgment's arrival. Each budget is `DEFAULT_DEADLINE` (debug override
`RIVET_TEST_GIT_DEADLINE_MS`); kernel reaping and filesystem overhead are not
covered by any of them. A kill at any bound after preparation can leave Git's
ref locks; they keep the existing identity-bound human recovery, and no lock is
removed automatically. A failed reap is reported as a Git error, never as a
finished abort. A handshake other than Git's acknowledgment closes the input
and counts as a lost comparison only once Git exits with a status within the
preparation budget; a child that died by a signal is a Git error, and one still
running at the budget is a deadline. `ref_phase_*` in `crates/markrivet-cli/tests/concurrency/ref_phase.rs`
covers it with a 4,000 ms debug budget and 2,400 ms stalls of real Git (1,600
ms under one budget, 800 ms over it when two are combined): split stimuli that
the single-clock engine was killed by in every run now commit once, and one
stall longer than its own budget is still killed.

## Analytics contract version 1

The analytics request, declaration, field-state, metric and bundle forms are in
[usage.md](usage.md#analytics-rows-counts-and-captured-reports). They are additive;
legacy `query`, item schemas, schema byte digests and typed write guards retain
their contracts. Metadata declarations and saved views participate only in
analytics fingerprints. Metadata fields cannot be typed write targets.

Strict `schema::extract` and diagnostic item extraction share `discover` and
`check_value`. Discovery returns authored inputs before defaults. Strict
extraction retains its all-or-nothing errors; diagnostic reads preserve valid
siblings and represent invalid fields separately. Typed writes require strict
extraction. Analytic grouping and predicates use effective defaulted values;
provenance is exposed only through an explicit origin selector.

An analytics scan uses the held project descriptor, observed no-follow walker
and one bounded raw read per matching Markdown file. Bytes are captured before
UTF-8 decoding or `Document::parse`. Parsed documents derive from those bytes;
export never rereads a live source. Frontmatter metadata uses the existing
structured parser. Analytics-only failed-recognition detection checks the raw
leading fence and existing YAML-looking-line criterion without changing legacy
recognition, body boundaries or revision bytes.

Strict validation precedes filtering. Missing required fields and disallowed
nulls belong to the invalid bucket, while known absence remains selectable with
`missing`. Optional absence, permitted null, invalid and unsupported fields each
have separate counting partitions. Comparisons are three-valued on invalid or
unsupported values; known missing optional values retain legacy false semantics.
AND short-circuits false and otherwise preserves unknown. Presence comparisons
use presence independently of value validity.

Document rows enumerate matching files even for empty or absent queues. Binding
is bound, missing or ambiguous. Strict mode refuses ambiguity. Explicit allowed
missing bindings are included as documents and yield zero item candidates.
Unstamped items remain valid; duplicate ids do not merge source locations.

The walker prunes only provably unreachable directory prefixes. An unlisted
matching directory refuses a strict census; an entry error without a known
filename remains scope-unknown. Proven unrelated errors are reported and do not
refuse. Policy-excluded directories and symlinks, which are never followed, have
unknown descendants and no invented file count. Malformed native filenames are
reported reversibly and do not become lossy Unicode document keys.
`Project::documents` adapts the observed walker into matching Markdown paths
and diagnostics for relevant gaps and policy exclusions. Collection discovery
uses this adapter, including archive-copy's provenance search, which refuses
incomplete or excluded inputs. `Project::list` is its whole-project adapter.
`status.complete` reuses `WalkEvent::is_gap` for working-tree enumeration and
unrepresentable names in HEAD or the index, retaining their native path suffix;
policy exclusions retain the adapter's diagnostics without creating a gap.
The combined path list is incomplete when git enumeration is unavailable or
bookkeeping reports diagnostics. Explicit paths define the requested set without
a walk; per-path inspection errors do not make that set incomplete.
Public strict discovery admits all matching regular files with uncapped
enumeration and skips hidden/build entries before type checks. The bounded
`prune-snapshot` mode instead follows the scope, caps and archive-exclusion
refusal rules above. Native names are matched as bytes before deciding relevance.

Counts and enum facets consume matching rows as a stream; aggregation does not
retain all matching action text. Typed keys use tagged state/type/value objects,
with exact numeric identity and deterministic ordering. All metric totals precede
pagination and top-N. Conditional true, false and unknown counts partition the
base group. Distinct metrics exclude missing, null, invalid and unsupported keys.
Drilldown binds the group and measure to the report fingerprint and exposes the
same partitions and contributing source rows.

Export bundles atomically publish under a no-follow `.rivet/exports/` descriptor,
without replacing existing captures. Every source member is separately hashed;
the manifest contains exact definitions, source outcomes, traversal events,
coverage and bounded typed records. A digest binds all members. Replay confines
member paths, enforces bounds and exact engine/source-build, parser and analytics
version matches, reparses complete captures, then compares derived outcomes and
records. Recorded failures lacking complete bytes and traversal observations are
authoritative only for their observed outcomes. Replay opens bundle artifacts
only, never live documents or live definitions. The consistency claim remains
`per_file`, not an atomic cross-file snapshot.

## Collection diagnosis

Collection binding validation rejects literal `files` and `archive` patterns
containing a hidden component or a component named `target` or `node_modules`
with `invalid_schema`. Rivet's glob metacharacters are `*` and `?`; patterns
containing either retain their existing membership and traversal behavior.
One validator checks both binding namespaces independently of file existence,
before schema resolution. Direct-path binding checks use the same predicate
and validator, including archive matches and non-item `get` selectors.
Collection discovery and citation scope retain the walker's observations and
report literal-binding errors separately. Traversal and discovery exclusions
are unchanged. `status` reports rejected configuration without changing its
enumeration-only `complete` field; `doctor` treats it as a schema error and
incomplete diagnosis, including invalid archive bindings outside selected
collections because those collections participate in citation scope.

`doctor` uses the existing checkbox collection membership, extraction and
selector semantics on one immutable parsed document per selected path. It
resolves binding and schema bytes before file reads, caches shared schema bytes,
and fingerprints those inputs. Membership counters include invalid items and
checked records; physical keys are `(path, list_index, item_index)`, independent
of stay ids, and a file record is keyed by its path alone. Explicit requested paths are accounted for independently of
directory discovery. One observed walk covers all live and archive binding
globs, with requested paths left out of pruning. Discovered counts filter its
files against selected live globs, keeping their whole-binding meaning; raw
events describe the full union. A walker error relevant to selected globs,
a global walker limit, and a
non-UTF-8 entry whose native bytes a glob could match or descend through retain
partial evidence and never produce complete healthy coverage; so do read and
parse errors. Proven unrelated errors and pruned prefixes do not impair
coverage. Policy exclusions a glob would otherwise match or descend through,
other than the root `.rivet` and any `.git`, are informational. Files that are
not `.md` or `.markdown` are not documents and are not discovered. The shared
`WalkEvent::is_gap` predicate re-evaluates relevance against the caller's globs,
including native-byte matching. Strict analytics refuses the same gaps that
make doctor incomplete. Doctor resolves citations using the same walk's files
and gaps outside selected scope, without enumerating again. Union discovery
limits conservatively bound selected coverage even when archive discovery
exhausts the budget.
Coverage gaps are decided before file reads, and informational exclusions and
walk events are retained after file detail, so a large walk cannot displace file
evidence from the response budget. Noncanonical requested paths are invalid.

The source-only stamp helper is the same paragraph/prefix check used by
`Planner::stamp_scoped`; it returns original carrier coordinates and does not
mint ids. Stamp and doctor both pass over an item whose own text carries a
digest-less hand-written stay: `adopt_stay` makes that stay the item's, where
stamping would mint a second id beside it. Whole-item rewrite prerequisites
share `Planner::set_item`'s foreign-marker and unsupported own-marker rules.
Current carriers use the existing prefix and plain-marker masking rules. The
document-wide `write_blocker` is reported separately and blocks every write,
including a checkbox update. In its absence, foreign body markers and existing
carrier blockers do not block the one-byte checkbox path. Structural `clear` is
not candidate verification: no replacement fields are supplied, so general typed
updates are informational `not_checked`.

Doctor defines its own severities: unaddressed markers are warnings; orphan,
malformed, duplicate/conflicted and unsupported-marker diagnostics are errors.
Unsupported frontmatter is a separate informational finding, because it refuses
only frontmatter key reads and writes. Other read commands retain their existing
exits. Missing ids and unselected subsection items warn; deliberately excluded
subtrees are informational. Receipt-log metadata is checked without reading its
contents: at 128 MiB it warns with `receipt_log_size`; an unsafe or unreadable
log produces `receipt_log_read` and incomplete coverage. Full-history status
reads remain capped at 256 MiB. Recovery and abandonment visit complete lines
backwards in 64 KiB chunks, retaining only selected terminal events and stopping
when all selected ops are found or the beginning is reached. This has no total
log size cap, but refuses a complete line above 256 MiB if reached. Incomplete
suffixes are discarded without buffering; malformed complete JSON lines retain
the existing skip behavior. Append truncation uses the same reverse block
traversal to preserve all complete prefix bytes.
The [usage contract](usage.md#doctor-collection-health-and-coverage) defines all
version-1 report fields, coverage counters, finding rules and exit meanings.

Detail retention is bounded incrementally to 10,000 diagnostics and an 8 MiB
response, including pretty serialization, with reserved incomplete-report space.
A limit stops further scan work and records omitted detail and lower-bound
counts. Incomplete diagnosis takes exit 2 with `ok: false`; complete findings
take exit 4 with `ok: true`. Doctor takes no writer lock and writes no project
bytes or entries. Its fingerprints and revisions describe inputs read per file,
not a project-wide atomic snapshot or protection from an editor changing them
after the read.

## Guarded item restoration

`revert-item` enters the common `api::write` orchestration with pure restore
preparation as a precondition. Actual requests acquire one project lock before
loading configuration and reading the document; preview takes no lock and never
settles pending records. Both use the same pure preparation, selection, current
membership/schema validation and authored-field discovery. Core returns authored
presence separately from strict defaulted extraction. Creation-Session comparison
is an additional wrapper policy; core does not synthesize or require equal Sessions.

The restore revision frames canonical path, requested ID and own-ID set, structural
item revision, collection name/key/schema digest, stable ancestor section ID sets,
parent-list identity, supplied history tip and observed branch. Locations use the
resolved owning list block, its innermost containing section and that section's
parent chain. Each section and stayed list identity resolves uniquely back to that
logical target. A sole-direct-list sentinel applies only when exactly one direct
list belongs to that owner, excluding child-section lists. Titles, offsets and
sibling bytes do not enter the identity. The same location function serves historical
snapshots, guard generation and the locked document. Unsupported locations refuse.

Selection follows immutable first-parent commits only and proves reachability
before accepting a source. Every source-to-tip state must be a present item with
the current own-ID set and location. Absence, gaps, kind changes and
identity/location changes refuse. The default additionally needs live content
equal to tip content and selects the predecessor of the most recent distinct
content transition. An initial absent preimage has no default. Explicit
selection may work within a proven interval newer than a shallow or missing
older boundary. Raw Git object access retains bounded pipes, object-size probes,
no lazy fetch, no replace objects and no historical checkout.

The historical and current item pass the existing set planner's carrier and
foreign stay prerequisites. Strict extraction uses today's schema for both
content snapshots; authored discovery returns null for absent inputs,
independent of defaults. Equal editable content exits before rendering or
evidence refresh. Otherwise the prepared operation is a structural item set with
current own carriers, followed by the normal `edit::plan` evidence refresh and
whole-candidate verifier. Only the current item splice and declared ancestor
evidence spans may change. The locked source is also the full replacement
preimage, preserving noncooperating outside edits by refusing replacement.

The landing helper receives an optional history assertion. After its own preflight,
it proves the supplied tip is on that exact HEAD's first-parent chain and that each
later snapshot preserves target content/IDs/location on the observed branch. This
check applies to commit, skip, adopt and ineligible outcomes before record preparation.
Immediately before calling replacement, HEAD/branch must still equal the preflight
pair recorded as the base. Failure concludes a prepared record as not applied. Normal
post-publication movement checks remain. An external Git race after the final check
can yield applied/skipped/pending. Restoration preconditions do not hold Git
ref locks across document replacement or detect a transient ref movement returning
to the same OID. The commit publication phase acquires ref locks after replacement.

Restore messages carry the selected source OID as ordinary body text, with current
caller attribution. No reserved trailer or Event field is added, and no delete intent
is constructed. Existing pending records retain source-OID message/session and recover
through the normal commit protocol. Restore precondition refusal never implicitly
settles another record. The [usage contract](usage.md#restore-one-live-item) names
request/response fields, diagnostics, policies and exit meanings.

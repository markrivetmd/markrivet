#!/usr/bin/env bash
# The records walkthrough from docs/usage.md, run on a scratch copy of this
# example: a bundle with one record per file, its fields in frontmatter.
# Every step checks its result; any surprise stops the script.
#
#   bash examples/records/walkthrough.sh            # uses `markrivet` from PATH
#   RIVET=target/debug/markrivet bash examples/records/walkthrough.sh
set -euo pipefail

command -v jq >/dev/null || { echo "walkthrough needs jq" >&2; exit 1; }
here=$(cd "$(dirname "$0")" && pwd)
rivet=$(command -v "${RIVET:-markrivet}") || { echo "markrivet not found; set RIVET" >&2; exit 1; }
rivet=$(realpath "$rivet")
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cp -r "$here/." "$work/"
rm "$work/walkthrough.sh"
cd "$work"
C=bundle/metrics/shelf-turnover.md
step() { printf '\n== %s\n' "$*"; }
expect() { jq -e "$1" >/dev/null || { echo "unexpected result for: $1" >&2; exit 1; }; }
body() { sed -n '/^# /,$p' "$1"; }
digest() { find bundle -type f | sort | xargs sha256sum; }

step "0. Diagnose the records; the index and the log are excluded"
echo '{"collection": "concepts"}' | "$rivet" doctor | expect '.ok and .complete
    and .health == "healthy" and .collections[0].coverage.items == 4
    and .collections[0].coverage.valid == 4
    and all(.items[]; .kind == "file" and .id == null)
    and ([.files[].path] | index("bundle/index.md") == null and index("bundle/log.md") == null)'

step "1. Query records gone stale before 2027 and not retired"
stale=$(echo '{"collection": "concepts",
               "where": [{"field": "stale_after", "op": "lt", "value": "2027-01-01T00:00:00Z"},
                         {"field": "status", "op": "ne", "value": "retired"}],
               "fields": ["status", "stale_after"]}' | "$rivet" query)
echo "$stale" | jq -c '.items[] | {path, fields}'
echo "$stale" | expect ".item == \"file\" and .diagnostics == [] and .total == 1
    and .items[0].path == \"$C\"
    and .items[0].fields == {status: \"draft\", stale_after: \"2026-12-31T00:00:00Z\"}"
guard=$(echo "$stale" | jq -r '.items[0].revision')

step "2. map hands a writer the same record guard"
echo "{\"path\": \"$C\"}" | "$rivet" map \
  | expect ".collections[0] | .item == \"file\" and .valid and .revision == \"$guard\""

step "3. Make it current, push stale_after out and record the check, in one write"
cp "$C" "$work/before.md"
# A sentinel keeps trailing newlines, which $(...) would strip.
before=$(cat "$C"; printf x); before=${before%x}
result=$(jq -n --arg path "$C" --arg guard "$guard" '{
  path: $path, collection: "concepts", guard: $guard,
  set: {status: "current", stale_after: "2027-06-30T00:00:00Z"},
  append: {verified: {by: "human:wren", at: "2026-11-28T09:00:00Z"}}}' | "$rivet" update-item)
echo "$result" | jq -c '{status, ops: [.ops[] | {revision}]}'
echo "$result" | expect '.status == "applied" and (.ops[0].revision | startswith("rivet:v1:record:"))'
next=$(echo "$result" | jq -r '.ops[0].revision')
old_entry='  - {by: "human:wren", at: 2026-11-02T10:00:00Z}'
want=${before/$'\n'status: draft$'\n'/$'\n'status: current$'\n'}
want=${want/$'\n'stale_after: 2026-12-31T00:00:00Z$'\n'/$'\n'stale_after: 2027-06-30T00:00:00Z$'\n'}
want=${want/$'\n'$old_entry$'\n'/$'\n'$old_entry$'\n''  - {by: "human:wren", at: 2026-11-28T09:00:00Z}'$'\n'}
printf '%s' "$want" > "$work/want.md"
cmp -s "$work/want.md" "$C" || { echo "the write changed other bytes" >&2; diff "$work/want.md" "$C" >&2; exit 1; }
cmp -s <(body "$work/before.md") <(body "$C") || { echo "the body changed" >&2; exit 1; }

step "4. Query again: nothing is stale, and the record carries both checks"
echo '{"collection": "concepts",
       "where": [{"field": "stale_after", "op": "lt", "value": "2027-01-01T00:00:00Z"},
                 {"field": "status", "op": "ne", "value": "retired"}]}' \
  | "$rivet" query | expect '.total == 0'
echo "{\"collection\": \"concepts\", \"paths\": [\"$C\"], \"fields\": [\"status\", \"verified\"]}" \
  | "$rivet" query | expect ".items[0].revision == \"$next\" and .items[0].fields == {status: \"current\",
      verified: [{by: \"human:wren\", at: \"2026-11-02T10:00:00Z\"},
                 {by: \"human:wren\", at: \"2026-11-28T09:00:00Z\"}]}"

step "5. Replaying the old guard exits 2 with guard_conflict and error.current"
files=$(digest)
set +e
replay=$(jq -n --arg path "$C" --arg guard "$guard" '{
  path: $path, collection: "concepts", guard: $guard, set: {status: "retired"}}' | "$rivet" update-item)
code=$?
set -e
[ "$code" = 2 ] || { echo "expected exit 2, got $code" >&2; exit 1; }
echo "$replay" | jq -c '.error | {code, current}'
echo "$replay" | expect ".error.code == \"guard_conflict\" and .error.current == \"$next\""
[ "$(digest)" = "$files" ] || { echo "a refused write changed bytes" >&2; exit 1; }

step "6. A bare verified mapping reads as one entry; append refuses to rewrite it"
P=bundle/policies/reading-room.md
read=$(echo "{\"collection\": \"concepts\", \"paths\": [\"$P\"], \"fields\": [\"verified\"]}" | "$rivet" query)
echo "$read" | expect '.items[0].fields.verified == [{by: "human:ash", at: "2026-10-20T12:00:00Z"}]'
set +e
refused=$(jq -n --arg path "$P" --arg guard "$(echo "$read" | jq -r '.items[0].revision')" '{
  path: $path, collection: "concepts", guard: $guard,
  append: {verified: {by: "human:wren", at: "2026-11-28T09:00:00Z"}}, set: {}}' | "$rivet" update-item)
code=$?
set -e
[ "$code" = 2 ] || { echo "expected exit 2, got $code" >&2; exit 1; }
echo "$refused" | expect '.error.code == "unsupported_syntax" and (.error.message | contains("bare mapping"))'
[ "$(digest)" = "$files" ] || { echo "a refused write changed bytes" >&2; exit 1; }

printf '\nwalkthrough ok\n'

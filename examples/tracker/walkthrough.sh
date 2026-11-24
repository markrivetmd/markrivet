#!/usr/bin/env bash
# The tracker walkthrough from docs/usage.md, run on a scratch copy of this
# example. Every step checks its result; any surprise stops the script.
#
#   bash examples/tracker/walkthrough.sh            # uses `markrivet` from PATH
#   RIVET=target/debug/markrivet bash examples/tracker/walkthrough.sh
set -euo pipefail

command -v jq >/dev/null || { echo "walkthrough needs jq" >&2; exit 1; }
here=$(cd "$(dirname "$0")" && pwd)
rivet=$(command -v "${RIVET:-markrivet}") || { echo "markrivet not found; set RIVET" >&2; exit 1; }
rivet=$(realpath "$rivet")
work=$(mktemp -d)
unstamped=$(mktemp -d)
trap 'rm -rf "$work" "$unstamped"' EXIT
for dir in "$work" "$unstamped"; do
  cp -r "$here/." "$dir/"
  rm "$dir/walkthrough.sh"
done
cd "$work"
W=tracking/TRACKING_WIDGETS.md
step() { printf '\n== %s\n' "$*"; }
expect() { jq -e "$1" >/dev/null || { echo "unexpected result for: $1" >&2; exit 1; }; }
show() {
  local result
  result=$(cat) || return
  jq -c "${1:-.}" <<< "$result" >&2 || return
  printf '%s\n' "$result"
}

step "0. Diagnose the whole collection without writes"
echo '{"collection":"actions"}' | "$rivet" doctor | expect '.ok and .complete
    and .health == "warning" and .summary.files_parsed == 2
    and .summary.items == 5 and .summary.valid == 5 and .summary.missing_ids == 5'
echo '{}' | "$rivet" doctor | expect '.unique_items == 5'
set +e
strict=$(echo '{"collection":"actions","paths":["tracking/TRACKING_WIDGETS.md"],"strict":true}' | "$rivet" doctor)
code=$?
set -e
[ "$code" = 4 ] || { echo "expected strict doctor exit 4, got $code" >&2; exit 1; }
echo "$strict" | expect '.complete and .ok and .summary.files_requested == 1 and .summary.items == 3'

# Steps 0 to 7 first run on an unstamped copy, where items are addressed by
# place. The stamped run below repeats them on its own copy with ids.
cd "$unstamped"

step "U2. Unstamped: query open actions; each has a position select, no id"
open=$(echo '{"collection": "actions", "where": [{"field": "done", "op": "eq", "value": false}],
              "fields": ["title", "actor"]}' | "$rivet" query)
echo "$open" | jq -c '.items[] | {path, id, addressing, select, fields}'
echo "$open" | expect '.diagnostics == [] and .total == 4
    and all(.items[]; .id == null and .addressing == "position")'
select=$(echo "$open" | jq -c '.items[] | select(.fields.title == "Refresh the fixtures.") | .select')
echo "$select" | expect '. == {kind: "position", heading_path: ["Widgets", "Action"], index: 1}'

step "U2b. Unstamped: find names the same place"
echo "{\"text\": \"Refresh the fixtures\", \"path\": \"$W\"}" \
  | "$rivet" find | show '.matches[] | {kind, addressing, select, line, excerpt}' \
  | expect ".total == 1 and .matches[0].kind == \"item\" and .matches[0].addressing == \"position\"
      and .matches[0].select == $select"
found=$(echo "{\"text\": \"Refresh the fixtures\"}" | "$rivet" find | jq -r '.matches[0].typed.actions')

step "U3. Unstamped: read it by position with its typed guard"
item=$(jq -n --arg path "$W" --argjson select "$select" '{path: $path, select: $select}' | "$rivet" get)
echo "$item" | jq '{content, target: .target.select, typed: .typed.actions.fields}'
action_guard=$(echo "$item" | jq -r .typed.actions.revision)
[ "$found" = "$action_guard" ] || { echo "find and get disagree on the typed guard" >&2; exit 1; }

step "U4. Unstamped: complete it by position and log it under the Log heading path"
log_guard=$(jq -n --arg path "$W" '{path: $path, select: {kind: "heading_path", path: ["Log"]}}' \
  | "$rivet" get | jq -r .revision)
batch=$(jq -n --arg path "$W" --argjson select "$select" --arg guard "$action_guard" --arg log "$log_guard" '{
  path: $path,
  ops: [
    {op: "update_item", collection: "actions", select: $select, guard: $guard,
     set: {done: true, gate: "the fixture diff is empty after regeneration."}},
    {op: "insert", select: {kind: "heading_path", path: ["Log"]}, position: "start", guard: $log,
     content: "### 2026-09-11: fixtures regenerated\n\nRegenerated from the seeded corpus."}
  ]}')
result=$(echo "$batch" | "$rivet" write)
echo "$result" | jq '{status, next_guard: .ops[0].revision, select: .ops[0].select, addressing: .ops[0].addressing}'
echo "$result" | expect ".status == \"applied\" and .ops[0].addressing == \"position\"
    and .ops[0].select == $select"
grep -q 'stay:' "$W" && { echo "the unstamped run minted a stay" >&2; exit 1; }

step "U5. Unstamped: query again; the action is closed"
echo '{"collection": "actions", "where": [{"field": "done", "op": "eq", "value": false}]}' \
  | "$rivet" query | show '[.items[].fields.title]' \
  | expect '[.items[].fields.title] | index("Refresh the fixtures.") == null'

step "U6. Unstamped: a stale guard is refused, with no current revision"
set +e
stale=$(jq -n --arg path "$W" --argjson select "$select" --arg guard "$action_guard" \
  '{path: $path, collection: "actions", select: $select, guard: $guard, set: {done: false}}' \
  | "$rivet" update-item)
code=$?
set -e
echo "$stale" | jq '{status, code: .error.code, message: .error.message}'
[ "$code" = 2 ] || { echo "expected exit 2, got $code" >&2; exit 1; }
echo "$stale" | expect '.error.code == "guard_conflict" and (.error | has("current") | not)'

step "U7. Unstamped: re-read by position, decide, retry with the fresh guard"
fresh=$(jq -n --arg path "$W" --argjson select "$select" '{path: $path, select: $select}' \
  | "$rivet" get | jq -r .typed.actions.revision)
jq -n --arg path "$W" --argjson select "$select" --arg guard "$fresh" \
  '{path: $path, collection: "actions", select: $select, guard: $guard, set: {done: false}}' \
  | "$rivet" update-item | expect '.status == "applied"'

step "U8. Unstamped: moving it to another file needs an id, so it is refused"
item=$(jq -n --arg path "$W" --argjson select "$select" '{path: $path, select: $select}' | "$rivet" get)
set +e
moved=$(jq -n --arg source "$W" --argjson item "$item" '{source: $source,
  destination: "tracking/TRACKING_DOCS.md", id: $item.target.id, guard: $item.revision,
  collection: "actions", typed_guard: $item.typed.actions.revision}' | "$rivet" move)
code=$?
set -e
echo "$moved" | jq '{status, code: .error.code, message: .error.message}'
[ "$code" = 1 ] || { echo "expected exit 1, got $code" >&2; exit 1; }
echo "$moved" | expect '.error.code == "invalid_request"'

cd "$work"

step "1. Map the tracker and stamp it (preview first)"
# A stamp defaults to the sections a collection binds. Every section is
# stamped, so the Log heading is addressable in step 4, but the Notes list is
# left alone: nothing addresses it. `select: {"kind": "document"}` takes all.
map=$(echo "{\"path\": \"$W\"}" | "$rivet" map)
echo "$map" | jq -c '.outline[] | {kind, title, id, line}'
guard=$(echo "$map" | jq -r .revision)
echo "{\"path\": \"$W\", \"guard\": \"$guard\", \"preview\": true}" | "$rivet" stamp \
  | show | expect '.status == "preview"
      and ([.ops[0].minted[] | select(.kind == "section")] | length) == 6
      and ([.ops[0].minted[] | select(.kind == "item")] | length) == 3'
echo "{\"path\": \"$W\", \"guard\": \"$guard\"}" | "$rivet" stamp | expect '.status == "applied"'

step "2. Query open actions across the collection"
open=$(echo '{"collection": "actions", "where": [{"field": "done", "op": "eq", "value": false}],
              "fields": ["title", "actor"]}' | "$rivet" query)
echo "$open" | jq -c '.items[] | {path, id, fields}'
echo "$open" | jq -c '.diagnostics[] | {path, line, code}'
id=$(echo "$open" | jq -r '.items[] | select(.fields.title == "Refresh the fixtures.") | .id')

step "2b. Or find it by its text: the same id, its selector and typed guard, no map first"
echo "{\"text\": \"Refresh the fixtures\", \"path\": \"$W\", \"match_case\": false, \"limit\": 20}" \
  | "$rivet" find | show '.matches[] | {kind, id, select, line, excerpt}' \
  | expect ".total == 1 and .matches[0].kind == \"item\" and .matches[0].id == \"$id\"
      and .matches[0].select == {kind: \"id\", id: \"$id\"}"
found=$(echo "{\"text\": \"Refresh the fixtures\"}" | "$rivet" find | jq -r '.matches[0].typed.actions')

step "3. Read that action with its typed guard"
item=$(echo "{\"path\": \"$W\", \"select\": {\"kind\": \"id\", \"id\": \"$id\"}}" | "$rivet" get)
echo "$item" | jq '{content, typed: .typed.actions.fields}'
action_guard=$(echo "$item" | jq -r .typed.actions.revision)
[ "$found" = "$action_guard" ] || { echo "find and get disagree on the typed guard" >&2; exit 1; }

step "4. Complete it and log the change, in one single-file batch"
log=$(echo "{\"path\": \"$W\"}" | "$rivet" map | jq -c '.outline[] | select(.title == "Log")')
batch=$(jq -n --arg path "$W" --arg id "$id" --arg guard "$action_guard" --argjson log "$log" '{
  path: $path,
  ops: [
    {op: "update_item", collection: "actions", id: $id, guard: $guard,
     set: {done: true, gate: "the fixture diff is empty after regeneration."}},
    {op: "insert", select: {kind: "id", id: $log.id}, position: "start",
     guard: $log.revision, stamp: true,
     content: "### 2026-09-11: fixtures regenerated\n\nRegenerated from the seeded corpus."}
  ]}')
result=$(echo "$batch" | "$rivet" write)
echo "$result" | jq '{status, durable, next_guard: .ops[0].revision, evidence_refreshed, drift}'
echo "$result" | expect '.status == "applied" and .durable == true'
next_guard=$(echo "$result" | jq -r '.ops[0].revision')

step "5. Query again: the action is closed"
echo '{"collection": "actions", "where": [{"field": "done", "op": "eq", "value": false}]}' \
  | "$rivet" query | show '[.items[].fields.title]' \
  | expect "[.items[].id] | index(\"$id\") == null"

step "6. A stale guard is refused with the current revision"
set +e
stale=$(echo "{\"path\": \"$W\", \"collection\": \"actions\", \"id\": \"$id\",
               \"guard\": \"$action_guard\", \"set\": {\"done\": false}}" | "$rivet" update-item)
code=$?
set -e
echo "$stale" | jq '{status, code: .error.code, current: .error.current}'
[ "$code" = 2 ] || { echo "expected exit 2, got $code" >&2; exit 1; }
echo "$stale" | expect ".error.code == \"guard_conflict\" and .error.current == \"$next_guard\""

step "7. Recover: re-read, decide, retry with the fresh guard"
fresh=$(echo "{\"path\": \"$W\", \"select\": {\"kind\": \"id\", \"id\": \"$id\"}}" | "$rivet" get \
  | jq -r .typed.actions.revision)
echo "{\"path\": \"$W\", \"collection\": \"actions\", \"id\": \"$id\",
       \"guard\": \"$fresh\", \"set\": {\"done\": false}}" | "$rivet" update-item \
  | expect '.status == "applied"'
echo "reopened $id"

step "8. Move the action to the docs tracker in one two-path commit"
D=tracking/TRACKING_DOCS.md
dest_guard=$(echo "{\"path\":\"$D\"}" | "$rivet" map | jq -r .revision)
jq -n --arg path "$D" --arg guard "$dest_guard" '{path:$path,guard:$guard}' \
  | "$rivet" stamp | expect '.status == "applied"'
# These Git settings affect only this disposable walkthrough project.
git init -q -b main
git config user.name 'Synthetic Tester'
git config user.email 'tester@example.invalid'
jq '.git = {commit:"per-write",on_dirty:"refuse"}' .rivet/project.json > .rivet/project.next
mv .rivet/project.next .rivet/project.json
git add tracking .rivet/project.json .rivet/schemas
git commit -qm 'Synthetic move preimage'
item=$(jq -n --arg path "$W" --arg id "$id" '{path:$path,select:{kind:"id",id:$id}}' | "$rivet" get)
move_request=$(jq -n --arg source "$W" --arg destination "$D" --arg id "$id" --argjson item "$item" \
  '{source:$source,destination:$destination,id:$id,guard:$item.revision,collection:"actions",typed_guard:$item.typed.actions.revision}')
moved=$(echo "$move_request" | "$rivet" move)
echo "$moved" | expect '.status == "applied" and .commit.status == "committed"'
replayed=$(echo "$move_request" | "$rivet" move)
echo "$replayed" | expect '.status == "unchanged" and .current == "matches"'
[ "$(echo "$moved" | jq -r .commit.sha)" = "$(echo "$replayed" | jq -r .commit.sha)" ]
[ "$(echo "$moved" | jq -r .commit.op)" = "$(echo "$replayed" | jq -r .commit.op)" ]
jq -n --arg path "$D" --arg id "$id" '{path:$path,select:{kind:"id",id:$id}}' \
  | "$rivet" get | expect '.ok and .typed.actions.fields.done == false'

step "9. Move Notes before the destination Log, keeping its heading level"
source_map=$(jq -n --arg path "$W" '{path:$path}' | "$rivet" map)
destination_map=$(jq -n --arg path "$D" '{path:$path}' | "$rivet" map)
section_request=$(jq -n --arg source "$W" --arg destination "$D" \
  --argjson a "$source_map" --argjson b "$destination_map" '{
    source:$source,destination:$destination,
    id:($a.outline[]|select(.title=="Notes")|.id),
    guard:($a.outline[]|select(.title=="Notes")|.revision),
    source_parent:($a.outline[]|select(.title=="Widgets")|.id),
    destination_parent:($b.outline[]|select(.title=="Documentation")|.id),
    before:($b.outline[]|select(.title=="Log")|.id)
  }')
echo "$section_request" | "$rivet" move | expect '.status == "applied" and .commit.status == "committed"'
echo "$section_request" | "$rivet" move | expect '.status == "unchanged" and .current == "matches"'
notes_id=$(echo "$section_request" | jq -r .id)
jq -n --arg path "$D" --arg id "$notes_id" '{path:$path,select:{kind:"id",id:$id}}' \
  | "$rivet" get | expect '.ok and .level == 2 and (.body | contains("widget-cli release"))'
echo
echo "walkthrough ok"

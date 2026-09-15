#!/usr/bin/env bash
#
# Reproduction: a push carrying more than one local revision into a branch head
# that has moved silently drops the other side's work.
#
#   LORE=/path/to/lore LORE_SERVER=lore://127.0.0.1:41337 ./repro-push-reparent-drops-work.sh
#
# Needs a reachable loreserver with no auth and a lore CLI. Creates its own
# repository on that server and three working copies under a temp directory.
#
# What it does:
#   A and B both clone the same head.
#   A commits one file and pushes.                      -> the head moves
#   B, still on the old head, commits TWO files and
#     pushes once with --fast-forward-merge.            -> both land
#   A third clone then shows what the server really holds.
#
# Expected (correct): the third clone has A's file and both of B's.
# Observed (before the fix): A's file is gone, while A's commit is still in
# the history, so nothing looks wrong to anyone reading it.
#
# Why: in lore-revision/src/branch/push.rs the client bends a revision's
# parent pointer onto whatever the previous push returned, keeping its tree:
#
#     if !current_latest.is_zero() && state.parent_self() != current_latest {
#         state.set_parent_self(current_latest);
#     }
#
# That is right when the server merely renumbered the revision we just sent
# (same content under a new signature). After a server-side fast-forward merge
# `current_latest` is a different revision carrying somebody else's work, and
# re-pointing at it makes B's stale tree look like a direct descendant. The
# server then takes the ordinary fast-forward path — no merge, no three-way
# diff, no conflict check — and the branch becomes B's tree.
#
# The guard `!current_latest.is_zero()` is why a single-revision push is safe:
# the block cannot fire for the first revision of a push, only from the second.

set -euo pipefail

LORE=${LORE:-lore}
: "${LORE_SERVER:?set LORE_SERVER, e.g. lore://127.0.0.1:41337}"

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
REPO="${LORE_SERVER%/}/reparent-repro-$$-$(date +%s)"

lore_in() { local wc=$1; shift; "$LORE" --repository "$wc" --no-pager "$@" </dev/null; }

add_commit() { # <wc> <file> <content> <message>
    local wc=$1 file=$2 content=$3 msg=$4
    printf '%s\n' "$content" > "$wc/$file"
    lore_in "$wc" stage "$wc/$file" >/dev/null
    lore_in "$wc" commit "$msg" >/dev/null
}

echo "repository: $REPO"
echo

# ── seed ────────────────────────────────────────────────────────────────────
"$LORE" --no-pager repository create "$REPO" --repository "$WORK/seed" </dev/null >/dev/null
add_commit "$WORK/seed" keep.txt seed "seed"
lore_in "$WORK/seed" push >/dev/null

# ── two clones off the same head ────────────────────────────────────────────
"$LORE" --no-pager clone "$REPO" "$WORK/a" </dev/null >/dev/null
"$LORE" --no-pager clone "$REPO" "$WORK/b" </dev/null >/dev/null

# ── A publishes, moving the head ────────────────────────────────────────────
add_commit "$WORK/a" from-a.txt "A only" "A publishes"
lore_in "$WORK/a" push >/dev/null
echo "A pushed from-a.txt"

# ── B, still on the old head, commits TWICE and pushes once ─────────────────
add_commit "$WORK/b" from-b1.txt "B one" "B one"
add_commit "$WORK/b" from-b2.txt "B two" "B two"
lore_in "$WORK/b" push --fast-forward-merge >/dev/null
echo "B pushed two revisions with --fast-forward-merge"
echo

# ── what does the server actually hold? ─────────────────────────────────────
"$LORE" --no-pager clone "$REPO" "$WORK/c" </dev/null >/dev/null
echo "files in a fresh clone:"
find "$WORK/c" -maxdepth 1 -mindepth 1 -exec basename {} \; | sort | sed 's/^/  /'
echo
echo "history (A's commit is still in it either way):"
lore_in "$WORK/c" history 6 | grep -E '^(Revision|Signature|    )' | sed 's/^/  /'
echo

rc=0
for f in from-b1.txt from-b2.txt; do
    [ -f "$WORK/c/$f" ] || { echo "UNEXPECTED: B's own $f is missing too"; rc=1; }
done
if [ -f "$WORK/c/from-a.txt" ]; then
    echo "RESULT: OK — A's file survived B's push"
else
    echo "RESULT: LOST — A's file is gone from the branch, with no error reported"
    rc=1
fi
exit $rc

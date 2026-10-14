#!/usr/bin/env sh
set -eu

# Build first with the real HOME (cargo needs its registry), then exercise
# the binary against a synthetic home: ccs must list the fake session and
# exit 0; with no ~/.claude/projects it must fail gracefully with exit 1.
cargo build --quiet
BIN="target/debug/ccs"

TMP_ROOT="$(mktemp -d)"
cleanup() { rm -rf "$TMP_ROOT"; }
trap cleanup EXIT INT TERM

# 1) no projects directory -> exit 1, stderr says so
mkdir -p "$TMP_ROOT/empty"
set +e
ERR="$(HOME="$TMP_ROOT/empty" USERPROFILE="$TMP_ROOT/empty" "$BIN" --no-tui 2>&1)"
RC=$?
set -e
[ "$RC" = "1" ] || { echo "expected exit 1 for missing projects dir, got $RC"; exit 1; }
case "$ERR" in
  *"no claude projects directory"*) ;;
  *) echo "unexpected stderr: $ERR"; exit 1 ;;
esac

# 2) synthetic session -> listed with its topic, exit 0
PROJ="$TMP_ROOT/home/.claude/projects/D--ccs-smoke"
mkdir -p "$PROJ"
printf '%s\n%s\n' \
  '{"cwd":"D:/ccs-smoke","sessionId":"00000000-0000-4000-8000-000000000001"}' \
  '{"message":{"role":"user","content":"ccs smoke topic zqxwvb"}}' \
  > "$PROJ/00000000-0000-4000-8000-000000000001.jsonl"

OUT="$(HOME="$TMP_ROOT/home" USERPROFILE="$TMP_ROOT/home" "$BIN" --no-tui -n 5)"
case "$OUT" in
  *zqxwvb*) ;;
  *) echo "listing did not include the fake topic: $OUT"; exit 1 ;;
esac
case "$OUT" in
  *ccs-smoke*) ;;
  *) echo "listing did not include the fake cwd: $OUT"; exit 1 ;;
esac

echo "ccs platform smoke passed"

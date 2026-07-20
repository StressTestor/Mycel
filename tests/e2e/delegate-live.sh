#!/usr/bin/env bash
#
# LIVE proof of governed claude-subagent delegation (needs a Claude subscription
# login; not run in CI). A `mycel-delegate` subagent runs on the subscription,
# and its Bash commands still pass mycel-gate — a learned antibody blocks one.
#
# usage: bash tests/e2e/delegate-live.sh   (requires `claude` logged in, mycel installed)

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
export MYCEL_HOME="$(mktemp -d)/home"
mkdir -p "$MYCEL_HOME/substrate" "$MYCEL_HOME/delegate/bin"
DB="$MYCEL_HOME/substrate/mycel.db"
BIN="$REPO_ROOT/target/release"
FAILED=0
_pass() { printf '\033[1;32mPASS\033[0m %s\n' "$*"; }
_fail() { printf '\033[1;31mFAIL\033[0m %s\n' "$*"; FAILED=1; }

command -v claude >/dev/null 2>&1 || { echo "SKIP: claude not installed"; exit 0; }
cargo build --release -p mycel-gate -p mycel-cli --manifest-path "$REPO_ROOT/Cargo.toml" >/dev/null 2>&1 || { echo "FAIL build"; exit 1; }

# minimal installed layout the delegate expects
mkdir -p "$MYCEL_HOME/bin" "$MYCEL_HOME/delegate"
cp "$BIN/mycel-gate" "$MYCEL_HOME/bin/"; cp "$BIN/mycel-substrate" "$MYCEL_HOME/bin/"
sed "s|\$HOME/.mycel|$MYCEL_HOME|g" "$REPO_ROOT/config/delegate/settings.json.template" > "$MYCEL_HOME/delegate/settings.json"
cp "$REPO_ROOT/config/delegate/subagent-preamble.md" "$MYCEL_HOME/delegate/"

"$MYCEL_HOME/bin/mycel-substrate" list-antibodies --db "$DB" >/dev/null 2>&1
"$MYCEL_HOME/bin/mycel-substrate" antibody-add --db "$DB" --command-pattern "MYCEL_DELEGATE_LIVE_TOKEN" \
  --remediation "learned: this delegate-test pattern is blocked" --severity refuse --refusal-mode hard >/dev/null

echo "=== a governed claude subagent, asked to run the blocked command ==="
OUT="$(MYCEL_HOME="$MYCEL_HOME" "$REPO_ROOT/scripts/mycel-delegate" \
  "Use the Bash tool to run exactly once: echo MYCEL_DELEGATE_LIVE_TOKEN. If it is blocked, reply BLOCKED and the reason. Do not retry." 2>&1)"
echo "$OUT" | grep -qi "BLOCKED\|blocked\|reject" && _pass "delegate subagent's blocked command was gated" || _fail "not blocked: $OUT"
echo "$OUT" | grep -q "learned: this delegate-test pattern" && _pass "learned remediation surfaced to the subagent" || _fail "no remediation: $OUT"

echo "=== a benign delegated task runs (subscription, governed) ==="
OUT2="$(MYCEL_HOME="$MYCEL_HOME" "$REPO_ROOT/scripts/mycel-delegate" \
  "Use the Bash tool to run: echo MYCEL_DELEGATE_LIVE_OK. Report its output." 2>&1)"
echo "$OUT2" | grep -q "MYCEL_DELEGATE_LIVE_OK" && _pass "benign delegated command executed" || _fail "benign failed: $OUT2"

rm -rf "$(dirname "$MYCEL_HOME")"
[ "$FAILED" -eq 0 ] && { printf '\n\033[1;32mDELEGATE LIVE E2E: ALL PASS\033[0m\n'; exit 0; }
printf '\n\033[1;31mDELEGATE LIVE E2E: FAILURES\033[0m\n'; exit 1

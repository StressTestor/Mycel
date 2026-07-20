#!/usr/bin/env bash
#
# mycel installer. builds the rust brain and the ts harness from this repo,
# installs the mycel command + gate into ~/.mycel, verifies everything, and
# refuses to pretend success. every failure names the step and the fix.
#
# usage: bash install.sh
# env:   MYCEL_INSTALL_DIR (default $HOME/.mycel)
#        MYCEL_NO_MODIFY_PATH (skip rc edit when non-empty)

set -euo pipefail

MYCEL_INSTALL_DIR="${MYCEL_INSTALL_DIR:-$HOME/.mycel}"
MYCEL_NO_MODIFY_PATH="${MYCEL_NO_MODIFY_PATH:-}"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
STEP="startup"

_log()  { printf '\033[1;36m==>\033[0m %s\n' "$*"; }
_fail() {
  printf '\033[1;31merror during [%s]:\033[0m %s\n' "$STEP" "$*" >&2
  exit 1
}
trap '_fail "command failed (exit $?). fix the message above and re-run install.sh - it is idempotent."' ERR

# ---------- prerequisites ----------
STEP="prerequisites"
command -v node >/dev/null 2>&1 || _fail "node not found. install node >= 24.15 (brew install node)"
NODE_MAJOR="$(node -p 'process.versions.node.split(".")[0]')"
NODE_MINOR="$(node -p 'process.versions.node.split(".")[1]')"
if [ "$NODE_MAJOR" -lt 24 ] || { [ "$NODE_MAJOR" -eq 24 ] && [ "$NODE_MINOR" -lt 15 ]; }; then
  _fail "node $(node -v) too old, need >= 24.15.0 (brew upgrade node)"
fi
command -v pnpm >/dev/null 2>&1 || _fail "pnpm not found. install with: npm i -g pnpm@10"
command -v cargo >/dev/null 2>&1 || _fail "cargo not found. install rust: https://rustup.rs"
# `command -v cargo` finding a rustup shim is not enough: a rustup with no
# default toolchain resolves the shim but cannot actually run cargo. Prove it
# runs before we commit to a multi-minute build.
CARGO_VER="$(cargo --version 2>&1)" || _fail "cargo is present but cannot run: $CARGO_VER (try: rustup default stable)"
_log "prerequisites ok: node $(node -v), pnpm $(pnpm --version), $CARGO_VER"

# ---------- rust brain ----------
STEP="cargo build"
_log "building mycel-gate + mycel-substrate + mycel-mcp-server + mycel-observe (release)"
cargo build --release -p mycel-gate -p mycel-cli -p mycel-mcp -p mycel-observe --manifest-path "$REPO_ROOT/Cargo.toml"

# ---------- ts harness ----------
STEP="harness install"
_log "installing harness dependencies"
( cd "$REPO_ROOT/harness" && pnpm install --frozen-lockfile )
STEP="harness build"
_log "building harness"
( cd "$REPO_ROOT/harness" && pnpm run build:packages && pnpm -C apps/mycel run build )
[ -f "$REPO_ROOT/harness/apps/mycel/dist/main.mjs" ] || _fail "harness build produced no dist/main.mjs"

# ---------- install ----------
STEP="install binaries"
mkdir -p "$MYCEL_INSTALL_DIR/bin" "$MYCEL_INSTALL_DIR/substrate"
install -m 0755 "$REPO_ROOT/target/release/mycel-gate" "$MYCEL_INSTALL_DIR/bin/mycel-gate"
install -m 0755 "$REPO_ROOT/target/release/mycel-substrate"  "$MYCEL_INSTALL_DIR/bin/mycel-substrate"
install -m 0755 "$REPO_ROOT/target/release/mycel-mcp-server" "$MYCEL_INSTALL_DIR/bin/mycel-mcp-server"
install -m 0755 "$REPO_ROOT/target/release/mycel-observe"     "$MYCEL_INSTALL_DIR/bin/mycel-observe"
install -m 0755 "$REPO_ROOT/scripts/mycel-delegate"          "$MYCEL_INSTALL_DIR/bin/mycel-delegate"

NODE_BIN="$(command -v node)"
cat > "$MYCEL_INSTALL_DIR/bin/mycel" <<SHIM
#!/usr/bin/env bash
# mycel shim - runs the harness build from the repo checkout.
ENTRY="$REPO_ROOT/harness/apps/mycel/dist/main.mjs"
if [ ! -f "\$ENTRY" ]; then
  echo "mycel error: harness build missing at \$ENTRY" >&2
  echo "fix: cd $REPO_ROOT && bash install.sh   (did the repo move or the drive unmount?)" >&2
  exit 1
fi
exec "$NODE_BIN" "\$ENTRY" "\$@"
SHIM
chmod +x "$MYCEL_INSTALL_DIR/bin/mycel"
_log "installed mycel, mycel-gate, mycel-substrate, mycel-mcp-server, mycel-observe, mycel-delegate to $MYCEL_INSTALL_DIR/bin"

STEP="delegate governance scaffold"
# Config for governed `claude -p` subagents (mycel-delegate). Always refreshed
# from the template so a mycel-gate/mycel-mcp path change is picked up; these
# are generated files, not user-edited, so overwriting is safe.
mkdir -p "$MYCEL_INSTALL_DIR/delegate"
sed "s|\$HOME|$HOME|g" "$REPO_ROOT/config/delegate/settings.json.template" > "$MYCEL_INSTALL_DIR/delegate/settings.json"
sed "s|\$HOME|$HOME|g" "$REPO_ROOT/config/delegate/mcp.json.template" > "$MYCEL_INSTALL_DIR/delegate/mcp.json"
cp "$REPO_ROOT/config/delegate/subagent-preamble.md" "$MYCEL_INSTALL_DIR/delegate/subagent-preamble.md"
_log "wrote delegate governance config (subagents run under mycel-gate --claude)"

STEP="agents-md scaffold"
# ~/.mycel/AGENTS.md is injected into Mycel's system prompt; it tells the agent
# to prefer mycel-delegate for substantial subagent work. Never overwrite a
# user-edited one.
if [ -f "$MYCEL_INSTALL_DIR/AGENTS.md" ]; then
  _log "kept existing AGENTS.md (not overwritten)"
else
  cp "$REPO_ROOT/config/AGENTS.md.template" "$MYCEL_INSTALL_DIR/AGENTS.md"
  _log "wrote AGENTS.md - default subagent work routes through mycel-delegate when claude is present"
fi

STEP="config scaffold"
if [ -f "$MYCEL_INSTALL_DIR/config.toml" ]; then
  _log "kept existing config.toml (not overwritten)"
else
  sed "s|\$HOME|$HOME|g" "$REPO_ROOT/config/mycel.config.toml.template" > "$MYCEL_INSTALL_DIR/config.toml"
  _log "wrote config.toml from template - uncomment a provider and set default_model"
fi
if [ -f "$MYCEL_INSTALL_DIR/mcp.json" ]; then
  _log "kept existing mcp.json (not overwritten)"
else
  sed "s|\$HOME|$HOME|g" "$REPO_ROOT/config/mcp.json.template" > "$MYCEL_INSTALL_DIR/mcp.json"
  _log "wrote mcp.json registering the mycel-substrate MCP server"
fi
if [ -d "$HOME/.kimi-code" ] && [ ! -f "$MYCEL_INSTALL_DIR/.migration-hint-shown" ]; then
  _log "found ~/.kimi-code - to migrate sessions/config run:"
  _log "  cp -R ~/.kimi-code/credentials ~/.kimi-code/oauth $MYCEL_INSTALL_DIR/ 2>/dev/null"
  touch "$MYCEL_INSTALL_DIR/.migration-hint-shown"
fi

STEP="path"
if [ -n "$MYCEL_NO_MODIFY_PATH" ]; then
  _log "skipping PATH update (MYCEL_NO_MODIFY_PATH set)"
else
  case ":$PATH:" in
    *":$MYCEL_INSTALL_DIR/bin:"*) _log "PATH already has $MYCEL_INSTALL_DIR/bin" ;;
    *)
      RC="$HOME/.zshrc"
      [ -n "${SHELL:-}" ] && case "$(basename "$SHELL")" in bash) RC="$HOME/.bashrc";; fish) RC="$HOME/.config/fish/config.fish";; esac
      if ! grep -qsF "$MYCEL_INSTALL_DIR/bin" "$RC"; then
        printf '\n# mycel\nexport PATH="%s/bin:$PATH"\n' "$MYCEL_INSTALL_DIR" >> "$RC"
        _log "added $MYCEL_INSTALL_DIR/bin to PATH in $RC"
      fi
      ;;
  esac
fi

# ---------- substrate init ----------
# the gate NEVER creates the db itself: a deleted db must read as "guard
# disarmed -> block everything", not "fresh start -> allow everything".
# the installer is the one place the db gets created.
STEP="substrate init"
DB="$MYCEL_INSTALL_DIR/substrate/mycel.db"
if [ -f "$DB" ]; then
  _log "kept existing substrate db"
else
  "$MYCEL_INSTALL_DIR/bin/mycel-substrate" list-antibodies --db "$DB" >/dev/null
  [ -f "$DB" ] || _fail "substrate init did not create $DB"
  _log "initialized empty substrate at $DB (zero antibodies = allow-by-default)"
fi

# ---------- post-install verification ----------
STEP="verify: mycel --version"
V="$("$MYCEL_INSTALL_DIR/bin/mycel" --version)" || _fail "mycel --version failed"
_log "mycel version: $V"

STEP="verify: gate golden payload (benign allow)"
GATE_OUT="$(printf '{"tool_name":"Bash","tool_input":{"command":"echo mycel-install-check"}}' | "$MYCEL_INSTALL_DIR/bin/mycel-gate" --db "$DB")" || _fail "gate blocked the benign golden payload: $GATE_OUT"
[ "$GATE_OUT" = "{}" ] || _fail "gate golden payload returned unexpected output: $GATE_OUT"
_log "gate verified: benign payload allowed"

STEP="verify: gate fail-closed on missing db"
# An `if` condition is exempt from errexit AND the ERR trap, so the gate's
# intended nonzero (exit 3) is captured cleanly instead of tripping the trap.
if printf '{"tool_name":"Bash","tool_input":{"command":"echo x"}}' | "$MYCEL_INSTALL_DIR/bin/mycel-gate" --db "$MYCEL_INSTALL_DIR/substrate/does-not-exist.db" >/dev/null 2>&1; then
  MISSING_CODE=0
else
  MISSING_CODE=$?
fi
[ "$MISSING_CODE" -eq 3 ] || _fail "gate should exit 3 on missing db, got $MISSING_CODE"
_log "gate verified: missing db blocks (exit 3)"

_log "done. restart your shell or: export PATH=\"$MYCEL_INSTALL_DIR/bin:\$PATH\""
_log "next: edit $MYCEL_INSTALL_DIR/config.toml (pick a provider + default_model), then run: mycel"

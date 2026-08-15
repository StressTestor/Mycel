#!/usr/bin/env bash
#
# mycel installer. builds the native Rust CLI and ecology binaries from this
# repo, installs them into ~/.mycel, verifies everything, and
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
command -v cargo >/dev/null 2>&1 || _fail "cargo not found. install rust: https://rustup.rs"
# `command -v cargo` finding a rustup shim is not enough: a rustup with no
# default toolchain resolves the shim but cannot actually run cargo. Prove it
# runs before we commit to a multi-minute build.
CARGO_VER="$(cargo --version 2>&1)" || _fail "cargo is present but cannot run: $CARGO_VER (try: rustup default stable)"
command -v rg >/dev/null 2>&1 || _log "warning: ripgrep (rg) not found - the Glob/Grep tools need it at runtime (brew install ripgrep / apt install ripgrep)"
_log "prerequisites ok: $CARGO_VER"

# ---------- rust brain ----------
STEP="cargo build"
_log "building mycel + mycel-gate + mycel-substrate + mycel-mcp-server + mycel-observe (release)"
cargo build --release -p mycel-gate -p mycel-cli -p mycel-mcp -p mycel-observe --manifest-path "$REPO_ROOT/Cargo.toml"

# ---------- install ----------
STEP="install binaries"
mkdir -p "$MYCEL_INSTALL_DIR/bin" "$MYCEL_INSTALL_DIR/substrate"
install -m 0755 "$REPO_ROOT/target/release/mycel"           "$MYCEL_INSTALL_DIR/bin/mycel"
install -m 0755 "$REPO_ROOT/target/release/mycel-gate" "$MYCEL_INSTALL_DIR/bin/mycel-gate"
install -m 0755 "$REPO_ROOT/target/release/mycel-substrate"  "$MYCEL_INSTALL_DIR/bin/mycel-substrate"
install -m 0755 "$REPO_ROOT/target/release/mycel-mcp-server" "$MYCEL_INSTALL_DIR/bin/mycel-mcp-server"
install -m 0755 "$REPO_ROOT/target/release/mycel-observe"     "$MYCEL_INSTALL_DIR/bin/mycel-observe"
_log "installed mycel, mycel-gate, mycel-substrate, mycel-mcp-server, and mycel-observe to $MYCEL_INSTALL_DIR/bin"

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

# ---------- upgrade migration (pre-Rust-cutover installs) ----------
# pre-cutover mcp.json entries have no "transport" key; the Rust parser
# requires it and hard-fails at session start with a deliberately redacted
# error. inject "transport": "stdio" into command-based entries that lack it.
STEP="upgrade migration: mcp.json transport"
if command -v python3 >/dev/null 2>&1; then
  python3 - "$MYCEL_INSTALL_DIR/mcp.json" <<'PYEOF'
import json, os, sys
path = sys.argv[1]
try:
    with open(path) as f:
        data = json.load(f)
except (OSError, ValueError) as e:
    sys.stderr.write(f"warning: could not parse {path} ({e}); fix it by hand\n")
    sys.exit(0)
servers = data.get("mcpServers")
if not isinstance(servers, dict):
    sys.exit(0)
migrated, unknown = [], []
for name, entry in servers.items():
    if not isinstance(entry, dict) or "transport" in entry:
        continue
    if "command" in entry:
        entry["transport"] = "stdio"
        migrated.append(name)
    else:
        unknown.append(name)
if migrated:
    # atomic rewrite: a mid-write failure must never truncate the original
    tmp = path + ".migrate.tmp"
    with open(tmp, "w") as f:
        json.dump(data, f, indent=2)
        f.write("\n")
    os.replace(tmp, path)
    print(f"migrated mcp.json: added \"transport\": \"stdio\" to {', '.join(migrated)}")
for name in unknown:
    sys.stderr.write(
        f"warning: mcp.json entry \"{name}\" has no \"transport\" key and no "
        f"\"command\"; mycel will refuse to start until you add one "
        f"(\"stdio\", \"sse\", or \"streamable_http\") in {path}\n")
PYEOF
else
  if grep -qs '"command"' "$MYCEL_INSTALL_DIR/mcp.json" && ! grep -qs '"transport"' "$MYCEL_INSTALL_DIR/mcp.json"; then
    _fail "existing mcp.json predates the Rust cutover (no \"transport\" key) and python3 is unavailable to migrate it. add \"transport\": \"stdio\" to each server entry in $MYCEL_INSTALL_DIR/mcp.json, then re-run install.sh"
  fi
fi

# the external claude -p delegation was retired in the Rust cutover; its
# installed artifacts must not survive an upgrade. a leftover AGENTS.md that
# steers the agent to mycel-delegate is quarantined (never silently deleted -
# the user may have added their own directives to it).
STEP="upgrade migration: retired delegate surfaces"
if [ -f "$MYCEL_INSTALL_DIR/bin/mycel-delegate" ]; then
  rm -f "$MYCEL_INSTALL_DIR/bin/mycel-delegate"
  _log "removed retired mycel-delegate binary"
fi
if [ -d "$MYCEL_INSTALL_DIR/delegate" ]; then
  mv "$MYCEL_INSTALL_DIR/delegate" "$MYCEL_INSTALL_DIR/delegate.retired"
  _log "moved retired delegate/ governance scaffold to delegate.retired/ (delete it when ready)"
fi
if [ -f "$MYCEL_INSTALL_DIR/AGENTS.md" ] && grep -qs "mycel-delegate" "$MYCEL_INSTALL_DIR/AGENTS.md"; then
  mv "$MYCEL_INSTALL_DIR/AGENTS.md" "$MYCEL_INSTALL_DIR/AGENTS.md.pre-rust-bak"
  _log "quarantined stale AGENTS.md (steered the agent to the retired mycel-delegate) to AGENTS.md.pre-rust-bak - port any of your own directives to a fresh AGENTS.md"
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

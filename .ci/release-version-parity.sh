#!/usr/bin/env bash
# Guard: every surface that states the release version must state the same one.
#
# WHY THIS EXISTS
# ---------------
# The drift already happened. At the 1.0.1 cut the workspace manifest still said
# 1.0.0 while the docs said 1.0.1, and the published binary reported 1.0.0 — so
# an operator reading the docs, an operator reading `--version`, and the registry
# disagreed about which release they had. A tag is not fixable after the fact:
# it gets superseded, not corrected. So this runs BEFORE the tag.
#
# `license-version-parity.sh` already covers the BUSL surfaces (LICENSE,
# manifest, change-dates table, NOTICE). This one covers the RELEASE surfaces.
#
# THE FIVE SURFACES, AND WHY ONLY TWO CAN DRIFT
# ---------------------------------------------
# Two are DERIVED from the manifest and cannot disagree with it:
#   - `xyzdb-server --version`     clap `version` -> CARGO_PKG_VERSION
#   - the MCP handshake            Implementation::new(_, env!("CARGO_PKG_VERSION"))
# Checking their values would be theatre. What can break is someone replacing a
# derivation with a literal, so checks C and D assert the DERIVATION is still in
# place rather than comparing numbers.
#
# Two are written by hand and are the real exposure:
#   - server.json  "version"
#   - server.json  every ghcr.io/…/xyzdb-mcp:<tag> image identifier (there are
#     two package entries; the memory of the 1.0.1 cut is exactly a tag that did
#     not match the binary inside it)
#
# Checks:
#   A  Cargo.toml [workspace.package] version == server.json .version
#   B  every xyzdb-mcp image tag in server.json == that version
#   C  the server binary still derives --version from the manifest
#   D  the MCP handshake still derives its version from the manifest
#
#   sh .ci/release-version-parity.sh
#   sh .ci/release-version-parity.sh --self-test   # forges each failure
set -uo pipefail

root="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
FAILED=0
fail() { printf 'ERROR: %s\n' "$1" >&2; FAILED=1; }
ok()   { printf '  ok   %s\n' "$1"; }

ws_version() {
  awk '
    /^\[workspace\.package\]/ { f = 1; next }
    /^\[/                     { f = 0 }
    f && /^version[ \t]*=/ { gsub(/[^0-9.]/, ""); print; exit }
  ' "$1/Cargo.toml"
}

check() {   # $1 = repo root to check
  local r=$1 ws sj tags t
  ws="$(ws_version "$r")"
  [ -n "$ws" ] || { fail "cannot read [workspace.package] version from Cargo.toml"; return; }

  # A — server.json's own version.
  sj="$(sed -n 's/^  "version"[ ]*:[ ]*"\([0-9][0-9.]*\)".*/\1/p' "$r/server.json" | head -1)"
  if [ -z "$sj" ]; then
    fail "cannot read .version from server.json"
  elif [ "$sj" != "$ws" ]; then
    fail "server.json version $sj != manifest $ws"
  else
    ok "server.json version == manifest ($ws)"
  fi

  # B — every published image tag. A tag that does not match the binary inside
  # it is the failure that already shipped once.
  tags="$(grep -o 'xyzdb-mcp:[0-9][0-9.]*' "$r/server.json" | sed 's/.*://' | sort -u)"
  if [ -z "$tags" ]; then
    fail "no xyzdb-mcp image tag found in server.json"
  else
    for t in $tags; do
      if [ "$t" != "$ws" ]; then
        fail "server.json image tag xyzdb-mcp:$t != manifest $ws"
      else
        ok "image tag xyzdb-mcp:$t == manifest"
      fi
    done
  fi

  # C / D — the two derived surfaces stay derived. Comparing their values would
  # always pass; what needs guarding is that nobody hardcodes a literal.
  if grep -q 'version,' "$r/crates/server/src/main.rs" \
     && grep -q '#\[command(' "$r/crates/server/src/main.rs"; then
    ok "xyzdb-server --version is derived from the manifest (clap version)"
  else
    fail "xyzdb-server no longer derives --version from the manifest"
  fi

  if grep -q 'Implementation::new("xyzdb-mcp", env!("CARGO_PKG_VERSION"))' \
       "$r/crates/mcp/src/main.rs"; then
    ok "MCP handshake version is derived from the manifest"
  else
    fail "MCP handshake no longer derives its version from the manifest"
  fi
}

self_test() {
  tmp="$(mktemp -d)"
  # Minimal tree: only what the checks read.
  mkdir -p "$tmp/crates/server/src" "$tmp/crates/mcp/src"
  printf '[workspace.package]\nversion = "1.1.0"\n' > "$tmp/Cargo.toml"
  printf '{\n  "version": "1.1.0",\n  "packages": [{"identifier": "ghcr.io/tunolabs/xyzdb-mcp:1.1.0"}]\n}\n' > "$tmp/server.json"
  printf '#[command(name = "xyzdb-server", version, about = "x")]\n' > "$tmp/crates/server/src/main.rs"
  printf 'Implementation::new("xyzdb-mcp", env!("CARGO_PKG_VERSION"))\n' > "$tmp/crates/mcp/src/main.rs"

  FAILED=0; check "$tmp" >/dev/null 2>&1
  [ "$FAILED" -eq 0 ] && ok "a consistent tree passes" || fail "NOT caught: healthy tree rejected"

  echo "negative control 1 — server.json version behind the manifest"
  printf '{\n  "version": "1.0.0",\n  "packages": [{"identifier": "ghcr.io/tunolabs/xyzdb-mcp:1.1.0"}]\n}\n' > "$tmp/server.json"
  FAILED=0; check "$tmp" >/dev/null 2>&1; r1=$FAILED
  [ "$r1" -eq 1 ] && ok "caught" || fail "NOT caught — the surface that already drifted"

  echo "negative control 2 — image tag behind the manifest (the 1.0.1 shape)"
  printf '{\n  "version": "1.1.0",\n  "packages": [{"identifier": "ghcr.io/tunolabs/xyzdb-mcp:1.0.1"}]\n}\n' > "$tmp/server.json"
  FAILED=0; check "$tmp" >/dev/null 2>&1; r2=$FAILED
  [ "$r2" -eq 1 ] && ok "caught" || fail "NOT caught — a tag naming a version the binary is not"

  echo "negative control 3 — --version replaced by a literal"
  printf '{\n  "version": "1.1.0",\n  "packages": [{"identifier": "ghcr.io/tunolabs/xyzdb-mcp:1.1.0"}]\n}\n' > "$tmp/server.json"
  printf '#[command(name = "xyzdb-server", about = "x")]\n' > "$tmp/crates/server/src/main.rs"
  FAILED=0; check "$tmp" >/dev/null 2>&1; r3=$FAILED
  [ "$r3" -eq 1 ] && ok "caught" || fail "NOT caught — a derivation silently removed"

  echo "negative control 4 — MCP handshake hardcoded"
  printf '#[command(name = "xyzdb-server", version, about = "x")]\n' > "$tmp/crates/server/src/main.rs"
  printf 'Implementation::new("xyzdb-mcp", "1.0.0")\n' > "$tmp/crates/mcp/src/main.rs"
  FAILED=0; check "$tmp" >/dev/null 2>&1; r4=$FAILED
  [ "$r4" -eq 1 ] && ok "caught" || fail "NOT caught — a derivation replaced by a literal"

  rm -rf "$tmp"
  if [ "$r1" -eq 1 ] && [ "$r2" -eq 1 ] && [ "$r3" -eq 1 ] && [ "$r4" -eq 1 ]; then
    echo "all four controls fired"; return 0
  fi
  echo "A CONTROL DID NOT FIRE"; return 1
}

if [ "${1:-}" = "--self-test" ]; then
  self_test; exit $?
fi

echo "== release version parity"
check "$root"
if [ "$FAILED" -eq 0 ]; then
  echo "release-version-parity OK: every stated version agrees with the manifest"
else
  echo "VERSION DRIFT — do not tag"
fi
exit $FAILED

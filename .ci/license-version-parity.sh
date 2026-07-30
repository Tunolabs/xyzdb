#!/usr/bin/env bash
# Guard: the version and Change Date carried by the LICENSE, the workspace
# manifest, docs/license-change-dates.md, and NOTICE must all agree. The BUSL
# parameters are set by hand and each lives in a different file, so drift is
# easy and silent — a release could ship a LICENSE that says "Version 1.0"
# converting on one date while the change-dates table or NOTICE say another.
# This turns that convention into an enforced invariant.
#
# Checks (for the workspace's current major.minor line):
#   A  Cargo.toml [workspace.package] version  ==  LICENSE "Licensed Work" version
#   B  LICENSE "Change Date"                    ==  license-change-dates.md row
#   C  LICENSE "Change License"                 ==  license-change-dates.md row
#   D  NOTICE names the same version, Change Date, and Change License
#
# Wire into CI once a runner exists; runnable standalone today:
#   sh .ci/license-version-parity.sh
set -euo pipefail

root="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
fail() { echo "ERROR: $*" >&2; exit 1; }

# --- workspace version (major.minor of [workspace.package] version) ----------
ws_full="$(awk '
  /^\[workspace\.package\]/ { f = 1; next }
  /^\[/                     { f = 0 }
  f && /^version[ \t]*=/ {
    gsub(/[^0-9.]/, ""); print; exit
  }
' "$root/Cargo.toml")"
[ -n "$ws_full" ] || fail "could not read [workspace.package] version from Cargo.toml"
ws_ver="$(printf '%s\n' "$ws_full" | cut -d. -f1-2)"

# --- LICENSE parameters ------------------------------------------------------
lic_ver="$(awk -F'Version ' '/Licensed Work:/ { gsub(/[ \t].*/, "", $2); print $2; exit }' "$root/LICENSE")"
lic_date="$(awk -F': *' '/^Change Date:/ { gsub(/[ \t]/, "", $2); print $2; exit }' "$root/LICENSE")"
lic_chlic="$(awk -F': *' '/^Change License:/ { sub(/[ \t]+$/, "", $2); print $2; exit }' "$root/LICENSE")"
[ -n "$lic_ver" ]   || fail "could not read 'Licensed Work' version from LICENSE"
[ -n "$lic_date" ]  || fail "could not read 'Change Date' from LICENSE"
[ -n "$lic_chlic" ] || fail "could not read 'Change License' from LICENSE"

# --- change-dates table row for this version ---------------------------------
# Row form: | 1.0 (and the 1.0.x line) | 2026-07-30 | 2029-08-01 | Apache ... |
row="$(awk -F'|' -v v="$lic_ver" '
  $2 ~ v {
    cd = $4; cl = $5
    gsub(/^[ \t]+|[ \t]+$/, "", cd)
    gsub(/^[ \t]+|[ \t]+$/, "", cl)
    print cd "\t" cl
    exit
  }
' "$root/docs/license-change-dates.md")"
[ -n "$row" ] || fail "docs/license-change-dates.md has no row for version $lic_ver"
tbl_date="$(printf '%s' "$row" | cut -f1)"
tbl_chlic="$(printf '%s' "$row" | cut -f2)"

# --- A: workspace version vs LICENSE version ---------------------------------
[ "$ws_ver" = "$lic_ver" ] || fail "version mismatch: Cargo.toml $ws_ver (from $ws_full) != LICENSE $lic_ver"

# --- B/C: LICENSE vs change-dates table --------------------------------------
[ "$lic_date" = "$tbl_date" ]   || fail "Change Date mismatch: LICENSE $lic_date != change-dates.md $tbl_date"
[ "$lic_chlic" = "$tbl_chlic" ] || fail "Change License mismatch: LICENSE '$lic_chlic' != change-dates.md '$tbl_chlic'"

# --- D: NOTICE names the same version, date, and change license --------------
# The sentence wraps across lines in NOTICE, so collapse whitespace first.
notice="$(tr '\n' ' ' < "$root/NOTICE" | tr -s ' ')"
printf '%s' "$notice" | grep -q "xyzDB $lic_ver converts to the $lic_chlic on $lic_date" \
  || fail "NOTICE does not state 'xyzDB $lic_ver converts to the $lic_chlic on $lic_date'"

echo "license-version-parity OK: version $lic_ver, Change Date $lic_date -> $lic_chlic (LICENSE == Cargo.toml == change-dates.md == NOTICE)"

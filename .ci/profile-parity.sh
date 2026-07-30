#!/usr/bin/env bash
# Guard: benchmarks/native's [profile.release] must match the product root's, so
# the xyzdb driver in the cross-engine bench is built in production config (fat
# LTO). Cargo profiles only apply from a workspace root, so the block is a
# deliberate duplication (root + native); this turns that duplication into an
# enforced invariant instead of a convention that erodes on the next edit.
#
# Compares SETTINGS only (key = value), ignoring comments/blanks — the root block
# carries an explanatory comment the native block does not.
#
# Wire into CI once a runner exists; runnable standalone today:  sh .ci/profile-parity.sh
set -euo pipefail

root="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"

extract() {
  awk '
    /^\[profile\.release\]/ { f = 1; next }
    /^\[/                    { f = 0 }
    f && /=/ {
      sub(/[ \t]*#.*/, "")            # strip trailing comment
      gsub(/^[ \t]+|[ \t]+$/, "")     # trim
      if (length) print
    }
  ' "$1" | sort
}

a="$(extract "$root/Cargo.toml")"
b="$(extract "$root/benchmarks/native/Cargo.toml")"

if [ "$a" != "$b" ]; then
  echo "ERROR: [profile.release] diverged between product root and benchmarks/native." >&2
  echo "--- root ---" >&2; printf '%s\n' "$a" >&2
  echo "--- benchmarks/native ---" >&2; printf '%s\n' "$b" >&2
  exit 1
fi

echo "profile-parity OK: product root == benchmarks/native [profile.release]"

#!/usr/bin/env bash
# The tripwires' own gate.
#
# `require_clean_engine_tree`, `require_containerised_engine` and `run_step` are
# shell: `cargo test` does not reach them, so their negative controls are the only
# thing standing between a tripwire and a comment that looks like one. Running
# those controls once by hand is how a tripwire quietly stops firing three months
# later.
#
# Every case here asserts the FAILING direction — that the check can fail — because
# a tripwire that has never been seen to fire proves nothing. Where a passing
# direction is cheap it is asserted too.
#
#   ./test_tripwires.sh     # exits non-zero if any control does not behave
set -u
cd "$(dirname "$0")"
. ./lib_docker.sh >/dev/null 2>&1

FAILED=0
ok(){   printf '  ok   %s\n' "$1"; }
bad(){  printf '  FAIL %s\n' "$1"; FAILED=1; }
check(){ # $1=label $2=expected $3=actual
    [ "$2" = "$3" ] && ok "$1" || bad "$1 (expected $2, got $3)"
}

echo "T2 · run_step — full capture, REAL_EXIT on the next line"
run_step /tmp/tw_t2.log false >/dev/null 2>&1; check "a failing command returns its own code" 1 $?
run_step /tmp/tw_t2.log true  >/dev/null 2>&1; check "a passing command returns 0"            0 $?
# The variant that turned a green tree into a reported red on 2026-08-01: a
# trailing check command replaces the status of the one that mattered.
run_step /tmp/tw_t2b.log echo hola >/dev/null 2>&1; SAVED=$?
grep -c "NOTHING" /tmp/tw_t2b.log >/dev/null 2>&1   # exits 1: "no match", not "failure"
check "a trailing grep cannot overwrite the saved code" 0 "$SAVED"
# And the capture is COMPLETE — no tail, no head, no filter in between.
run_step /tmp/tw_t2c.log printf 'a\nb\nc\n' >/dev/null 2>&1
check "the log holds every line" 3 "$(wc -l < /tmp/tw_t2c.log | tr -d ' ')"

echo "T1 · require_containerised_engine — a host process is not the artefact"
docker rm -f bench-tripwireprobe >/dev/null 2>&1
require_containerised_engine xyzdb >/dev/null 2>&1
CODE=$?
if docker inspect -f '{{.State.Running}}' bench-xyzdb 2>/dev/null | grep -q true; then
    check "passes while the cell's container is up" 0 "$CODE"
    echo "  note the failing direction was NOT exercised: a bench-xyzdb container is"
    echo "       running. Re-run with no cell up to see it refuse."
else
    check "refuses when no container serves the port" 1 "$CODE"
    echo "  note the passing direction was NOT exercised: it needs a live cell,"
    echo "       which every real run provides."
fi

echo "T3 · require_clean_engine_tree — a modified engine does not measure"
require_clean_engine_tree >/dev/null 2>&1
check "passes on a clean tree" 0 $?
# An UNTRACKED file is enough and touches no real source. The first attempt at
# this control used `touch` on an existing file, which changes mtime and not
# content — git saw nothing and the control passed while proving nothing.
PROBE="$(git rev-parse --show-toplevel)/crates/.tripwire_probe"
: > "$PROBE"
require_clean_engine_tree >/dev/null 2>&1
check "refuses on a dirty tree" 1 $?
rm -f "$PROBE"
require_clean_engine_tree >/dev/null 2>&1
check "clean again once the probe is gone" 0 $?

echo
[ "$FAILED" -eq 0 ] && echo "all tripwire controls behaved" || echo "SOME CONTROLS DID NOT BEHAVE"
exit $FAILED

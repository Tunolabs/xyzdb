#!/bin/bash
# Shared cell-validation helper for the bench runners.
#
# Sourced by smoke_phase5b.sh / run_aws_4engines.sh / run_mac_hdd_3engines.sh.
# Provides:
#
#   validate_cell_queries <engine> <cell_json_path>
#     → exit 0 if all cold-phase queries returned n_runs > 0 (or if
#       (engine, query) is in the declared-deferrals list); 1 otherwise.
#       Prints the silenced query list to stderr on failure.
#
# Phase 5.b forensic (refinement #15) caught that Phase 5.b smoke #4
# silently passed cells where 1+ queries had n_runs=0 — the orchestrator
# captured the per-call error as a `warn!` and emitted P50=0.00ms n=0
# without surfacing the cell as a failure. Cross-engine reports built on
# top of those n=0 cells have contaminated coverage.
#
# Hardening rule: any cold-phase query with n_runs=0 that is NOT in the
# declared-deferrals list FAILS the cell. Declared deferrals as of
# v0.3.3 are documented per refinement chain:
#
#   xyzdb:Q10TransactionalCascade   (Inv.7 Scenario W, §13.4 Entry 13)
#   mongo:Q10TransactionalCascade   (Anomaly 3b replicaSet, §13.4 Entry 12)
#
# Anything else with n=0 is a silenced bug surface. This is the gate.
#
# Requires `jq` on PATH.

DEFERRALS_LIST=(
    # Format: <engine>:<query>
    "xyzdb:Q10TransactionalCascade"     # refinement #7-rev (Inv.7 Scenario W)
    "mongo:Q10TransactionalCascade"     # refinement #11 (Anomaly 3b replicaSet)
)

# _is_deferral <engine> <query>
# Returns 0 if (engine, query) matches any deferral entry.
_is_deferral() {
    local engine="$1" query="$2" d
    for d in "${DEFERRALS_LIST[@]}"; do
        [ "$d" = "${engine}:${query}" ] && return 0
    done
    return 1
}

# validate_cell_queries <engine> <cell_json_path>
validate_cell_queries() {
    local engine="$1" cell_json="$2"

    if [ ! -f "$cell_json" ]; then
        echo "!!! validate: cell json missing: $cell_json" >&2
        return 1
    fi

    if ! command -v jq >/dev/null 2>&1; then
        echo "!!! validate: jq not on PATH; cannot enforce gate (skipping)" >&2
        return 0
    fi

    local fails=()
    while IFS=, read -r q n; do
        if [ "$n" = "0" ]; then
            if ! _is_deferral "$engine" "$q"; then
                fails+=("$q")
            fi
        fi
    done < <(jq -r '.cold_queries[] | "\(.query),\(.n_runs)"' "$cell_json" 2>/dev/null)

    if [ ${#fails[@]} -gt 0 ]; then
        echo "!!! GATE FAIL: ${engine} cell silenced queries (n_runs=0): ${fails[*]}" >&2
        echo "!!! cell_json: $cell_json" >&2
        return 1
    fi

    # Refinement #16 (v0.3.4 cleanup cycle): WARN — not FAIL — when a Q
    # ran without errors (n_runs > 0) but returned zero records on every
    # cold repetition (avg_records == 0). Surfaces the gate gap that
    # silenced Surreal Q5 (Phase 5.b post-#14 smoke). Reads either the
    # explicit `empty_result_set` flag emitted by the orchestrator
    # (post-#16 builds) OR computes it directly from `n_runs > 0 AND
    # avg_records == 0` so legacy JSON without the flag still surfaces.
    local warns=()
    while IFS=, read -r q is_empty; do
        [ "$is_empty" = "true" ] && warns+=("$q")
    done < <(jq -r '.cold_queries[] | "\(.query),\((.empty_result_set // ((.n_runs > 0) and ((.avg_records | tonumber) == 0))) | tostring)"' "$cell_json" 2>/dev/null)

    if [ ${#warns[@]} -gt 0 ]; then
        echo "    WARN refinement #16: ${engine} cell empty_result_set queries (n_runs>0, avg_records=0): ${warns[*]}" >&2
        echo "    cell_json: $cell_json" >&2
        # Does NOT return 1 — caller continues. Surfaces the gap for
        # human review per Phase A.5 STOP gate (Surreal Q5 the canonical
        # case as of 2026-05-04).
    fi
    return 0
}

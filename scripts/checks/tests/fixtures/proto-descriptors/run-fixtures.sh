#!/usr/bin/env bash
# run-fixtures.sh — self-test for the linkage check proto-descriptors.
#
# Runs scripts/checks/proto-descriptors.sh against each fixture dir next to this file
# and asserts:
#   pass/    exit 0, prints PASS:
#   fail/    exit 1, prints FAIL:, names every line of fail/EXPECT_OFFENDER
#   exempt/  exit 0, prints PASS:, names every line of exempt/EXPECT_EXEMPT
#
# Each fixture dir holds a fixture.env: KEY=VALUE lines (the check's env
# overrides); "{fixture}" in a value is replaced by that fixture dir's
# absolute path. Example:
#   A_FILE={fixture}/defs.txt
#   B_DIR={fixture}/use
#
# Invoke via bash (no exec bit needed):
#   bash scripts/checks/tests/fixtures/proto-descriptors/run-fixtures.sh
# Exit: 0 all fixtures behave · 1 a fixture misbehaved · 2 misconfiguration
# (missing check, unfilled fixture.env / EXPECT_* files).

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CHECK="${CHECK:-$HERE/../../../proto-descriptors.sh}"
die() { echo "run-fixtures(proto-descriptors): error: $*" >&2; exit 2; }

[[ -f "$CHECK" ]] || die "check script not found: $CHECK"

failures=0
bad() { echo "  NOT OK $1: $2"; failures=$((failures + 1)); }

load_env() {  # $1 = fixture dir; fills the global array ENVV
    local fx="$1" line key
    ENVV=()
    [[ -f "$fx/fixture.env" ]] || die "missing $fx/fixture.env"
    while IFS= read -r line || [[ -n "$line" ]]; do
        [[ -z "$line" || "$line" == \#* ]] && continue
        [[ "$line" == *FIL[L]:* ]] && die "unfilled $fx/fixture.env: $line"
        key="${line%%=*}"
        [[ "$line" == *=* && "$key" =~ ^[A-Za-z_][A-Za-z0-9_]*$ ]] \
            || die "bad line in $fx/fixture.env: $line"
        ENVV+=("$key=${line#*=}")
        ENVV[-1]="${ENVV[-1]//\{fixture\}/$fx}"
    done < "$fx/fixture.env"
    [[ ${#ENVV[@]} -gt 0 ]] || die "empty $fx/fixture.env"
}

expect_lines() {  # $1 = file; prints non-empty, non-comment lines
    [[ -f "$1" ]] || die "missing $1"
    grep -q 'FIL[L]:' "$1" && die "unfilled $1"
    grep -v -e '^[[:space:]]*$' -e '^#' "$1" || die "empty $1"
}

run_one() {  # $1 = fixture name, $2 = expected exit, $3 = expected header
    local fx="$HERE/$1" out rc=0 name names
    [[ -d "$fx" ]] || die "missing fixture dir $fx"
    load_env "$fx"
    out="$(env "${ENVV[@]}" bash "$CHECK" 2>&1)" || rc=$?
    if [[ "$rc" -ne "$2" ]]; then
        bad "$1" "exit $rc, expected $2"; printf '%s\n' "$out" | sed 's/^/    | /'
        return
    fi
    grep -q "^$3" <<<"$out" || { bad "$1" "no '$3' line"; return; }
    # Read EXPECT_* in this shell (not a process substitution) so that an
    # unfilled/missing expectation file aborts with exit 2 instead of
    # silently asserting nothing.
    case "$1" in
        fail)   names="$(expect_lines "$fx/EXPECT_OFFENDER")"
                while IFS= read -r name; do
                    grep -qF -- "- $name" <<<"$out" || bad fail "offender '$name' not named"
                done <<<"$names" ;;
        exempt) names="$(expect_lines "$fx/EXPECT_EXEMPT")"
                while IFS= read -r name; do
                    grep -qF -- "exempt: $name" <<<"$out" || bad exempt "'$name' not listed as exempt"
                done <<<"$names" ;;
    esac
    echo "  ok $1 (exit $rc)"
}

echo "run-fixtures: proto-descriptors"
run_one pass 0 "PASS:"
run_one fail 1 "FAIL:"
run_one exempt 0 "PASS:"

if [[ "$failures" -gt 0 ]]; then
    echo "FAIL: $failures fixture assertion(s) for proto-descriptors"
    exit 1
fi
echo "PASS: 3 fixtures for proto-descriptors"

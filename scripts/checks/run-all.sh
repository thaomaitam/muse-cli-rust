#!/usr/bin/env bash
# run-all.sh — run every linkage check in this directory, plus each
# check's fixture self-test. Local validation only: there is no CI in
# this repo, and this script does not install hooks or change CI,
# merge, or branch-protection settings.
#
# Usage: bash scripts/checks/run-all.sh
# Exit: 0 only if every check and every fixture runner passes;
# 1 if any check reports offenders or any fixture runner fails;
# 2 on misconfiguration (missing script, missing binary, bad env).

set -u

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
failures=0
misconfig=0

for check in "$HERE"/*.sh; do
    name="$(basename "$check" .sh)"
    [[ "$name" == "run-all" ]] && continue
    echo "== check: $name =="
    bash "$check"
    rc=$?
    case "$rc" in
        0) echo "  ok $name" ;;
        1) echo "  FAIL $name (offenders)"; failures=$((failures + 1)) ;;
        *) echo "  ERROR $name (exit $rc)"; misconfig=1 ;;
    esac
    runner="$HERE/tests/fixtures/$name/run-fixtures.sh"
    if [[ -f "$runner" ]]; then
        echo "== fixtures: $name =="
        bash "$runner"
        rc=$?
        case "$rc" in
            0) echo "  ok fixtures $name" ;;
            1) echo "  FAIL fixtures $name"; failures=$((failures + 1)) ;;
            *) echo "  ERROR fixtures $name (exit $rc)"; misconfig=1 ;;
        esac
    else
        echo "  ERROR: no fixture runner for $name"; misconfig=1
    fi
done

if [[ "$misconfig" -ne 0 ]]; then
    echo "run-all: ERROR (misconfiguration)"
    exit 2
fi
if [[ "$failures" -gt 0 ]]; then
    echo "run-all: FAIL ($failures failing step(s))"
    exit 1
fi
echo "run-all: PASS (all checks and fixtures)"
exit 0

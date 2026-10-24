#!/usr/bin/env bash
# routes-called — every gateway method the code actually calls must be
# defined in assets/routes.json, and routes.json must not define the
# same method twice.
#
# A (definition site): assets/routes.json, one entry per method name.
# B (call sites): string literals passed as the method argument to
# call_json / open_stream / FsClient::call in src/*.rs, with test
# modules (everything from the first `#[cfg(test)]` in a file onward)
# excluded — tests deliberately call unknown methods to check the
# error path, and those are not production calls.
# Direction is b2a ONLY, deliberately. The reverse (every route must
# be called) is NOT a valid invariant here and is not shipped: only
# 27 of the 258 routes appear as literals in src/, because the rest
# are reachable through the `raw` command, which takes any method
# name from the user and resolves it through routes.json at runtime.
# Shipping a2b would fail on 231 routes that are correct by design.
# Runtime breakage on drift: calling an undefined method fails at
# runtime through routes::lookup with "unknown method" (exit 2 for
# `raw`, a gateway error for other commands), instead of failing
# here at check time.
#
# Special cases, verified against the real code:
# - Dynamic calls `call_json(&format!("session.{kind}"))` cannot be
#   resolved statically. The check expands them from the literal
#   `kind` values passed to cmd_session_op in the same file
#   (rename/pin/unpin/archive/unarchive/delete); a format call with
#   no discoverable kinds is a misconfiguration (exit 2), never a
#   silent pass.
# - `media.raw` is synthetic and is verified ABSENT from routes.json:
#   routes.rs documents that it is built by the caller through
#   Gateway::request_with_route, not through lookup. It is a stream
#   label in wait_response, not a route lookup, so the call-pattern
#   extraction below does not treat it as a route call; the PASS
#   report notes this explicitly. If it ever appears in routes.json,
#   that is fine too — the note then reports it as present.
#
# Escape hatch (b2a): a call site whose attached comment block (the
# contiguous `//` lines directly above the line holding the call, no
# blank line in between) contains "NO_ROUTE: <reason>" is exempted.
# The reason is required and is echoed in the PASS report. A bare
# NO_ROUTE marker without a reason is a misconfiguration (exit 2).
#
# Test hooks (environment overrides; defaults shown):
#   LINKAGE_ROUTES   assets/routes.json under the repo root
#   LINKAGE_SRC      src/ under the repo root
#   LINKAGE_DIRECTION  b2a only (any other value is exit 2, see above)
#   LINKAGE_DEBUG=1  show Python tracebacks on internal errors
#
# Exit codes: 0 pass · 1 offenders found · 2 usage / misconfiguration /
# internal error (one-line diagnostic on stderr). Exit 1 means only
# real offenders; nothing else exits 1.

set -euo pipefail

SELF="${BASH_SOURCE[0]}"
CHECK_NAME="$(basename "$SELF" .sh)"
die() { echo "$CHECK_NAME: error: $*" >&2; exit 2; }
usage() { echo "usage: $CHECK_NAME.sh [--direction b2a] [--debug]" >&2; exit 2; }

while [[ $# -gt 0 ]]; do
    case "$1" in
        --direction) [[ $# -ge 2 && -n "$2" ]] || usage; LINKAGE_DIRECTION="$2"; shift 2 ;;
        --debug) LINKAGE_DEBUG=1; shift ;;
        -h|--help) usage ;;
        *) usage ;;
    esac
done

REPO_ROOT="$(cd "$(dirname "$SELF")/../.." && pwd)"
LINKAGE_ROUTES="${LINKAGE_ROUTES:-$REPO_ROOT/assets/routes.json}"
LINKAGE_SRC="${LINKAGE_SRC:-$REPO_ROOT/src}"
LINKAGE_DIRECTION="${LINKAGE_DIRECTION:-b2a}"
LINKAGE_DEBUG="${LINKAGE_DEBUG:-}"

command -v python3 >/dev/null 2>&1 || die "python3 not found on PATH"
[[ -f "$LINKAGE_ROUTES" ]] || die "LINKAGE_ROUTES is not a file: $LINKAGE_ROUTES"
[[ -d "$LINKAGE_SRC" ]] || die "LINKAGE_SRC is not a directory: $LINKAGE_SRC"

export LINKAGE_ROUTES LINKAGE_SRC LINKAGE_DIRECTION LINKAGE_DEBUG CHECK_NAME

rc=0
python3 - <<'PYEOF' || rc=$?
import json
import os
import re
import sys
import traceback

name = os.environ["CHECK_NAME"]

def misconfig(msg):
    print("%s: error: %s" % (name, msg), file=sys.stderr)
    sys.exit(2)

try:
    direction = os.environ["LINKAGE_DIRECTION"]
    if direction != "b2a":
        misconfig("routes-called only supports direction b2a (a2b is invalid here: "
                  "routes are reachable via `raw`; see header)")
    routes_path = os.environ["LINKAGE_ROUTES"]
    src_dir = os.environ["LINKAGE_SRC"]

    with open(routes_path, encoding="utf-8") as f:
        try:
            entries = json.load(f)
        except json.JSONDecodeError as e:
            misconfig("routes JSON does not parse: %s" % e)
    if not isinstance(entries, list):
        misconfig("routes JSON must be a list of entries")
    defined = set()
    duplicates = []
    for e in entries:
        if not isinstance(e, dict) or not e.get("method") or not e.get("http") or not e.get("path"):
            misconfig("routes entry missing method/http/path: %r" % (e,))
        if e["method"] in defined:
            duplicates.append(e["method"])
        defined.add(e["method"])

    # --- Scan B: production call sites only. ---
    call_re = re.compile(r'(?:call_json|open_stream|\.call)\s*\(\s*"([A-Za-z0-9_.-]+)"')
    dyn_re = re.compile(r'call_json\s*\(\s*&format!\s*"session\.\{kind\}"')
    kind_re = re.compile(r'cmd_session_op\s*\(\s*gw\s*,\s*"([a-z]+)"')
    calls = []  # (method, relpath, lineno, exempt_reason or None)
    for dirpath, dirnames, files in os.walk(src_dir):
        dirnames.sort()
        for fn in sorted(files):
            if not fn.endswith(".rs"):
                continue
            path = os.path.join(dirpath, fn)
            rel = os.path.relpath(path, src_dir)
            with open(path, encoding="utf-8", errors="replace") as f:
                text = f.read()
            # Test modules live at the end of each file; their
            # deliberate unknown-method calls are not production calls.
            text = text.split("#[cfg(test)]", 1)[0]
            lines = text.splitlines()
            kinds = sorted(set(kind_re.findall(text)))
            for i, line in enumerate(lines):
                for m in call_re.finditer(line):
                    method = m.group(1)
                    reason = None
                    k = i - 1
                    while k >= 0 and re.match(r'\s*//', lines[k]):
                        mm = re.search(r'NO_ROUTE:\s*(\S.*)$', lines[k])
                        if mm:
                            reason = mm.group(1).strip()
                        elif re.search(r'\bNO_ROUTE\b', lines[k]):
                            misconfig("%s:%d: NO_ROUTE marker without a reason" % (rel, i + 1))
                        k -= 1
                    calls.append((method, rel, i + 1, reason))
                if dyn_re.search(line):
                    if not kinds:
                        misconfig("%s:%d: dynamic session.{kind} call with no "
                                  "cmd_session_op kinds discoverable" % (rel, i + 1))
                    for kind in kinds:
                        calls.append(("session." + kind, rel, i + 1, None))
            # Multi-line call_json("method", ...) where the string is on
            # the line after the call: the single-line regex above misses
            # it, so scan the joined text for call + newline + string.
            for m in re.finditer(r'(?:call_json|open_stream)\s*\(\s*\n\s*"([A-Za-z0-9_.-]+)"', text):
                lineno = text.count("\n", 0, m.start()) + 1
                method = m.group(1)
                if not any(c[0] == method and c[1] == rel and c[2] == lineno for c in calls):
                    calls.append((method, rel, lineno, None))

    offenders = []  # (method, [locations])
    exempted = []
    seen_offender = {}
    for method, rel, lineno, reason in calls:
        if method in defined:
            continue
        if reason:
            exempted.append((method, reason))
            continue
        seen_offender.setdefault(method, []).append("%s:%d" % (rel, lineno))
    if duplicates:
        for d in sorted(set(duplicates)):
            seen_offender.setdefault("duplicate route: " + d, []).append(routes_path)
    if seen_offender:
        print("FAIL: %d route references with no routes.json definition:" % len(seen_offender))
        for method in sorted(seen_offender):
            print("  - %s (%s)" % (method, ", ".join(seen_offender[method])))
        sys.exit(1)

    media_note = ("media.raw present in routes.json" if "media.raw" in defined
                  else "media.raw synthetic, absent from routes.json by design (routes.rs module doc)")
    print("PASS: %d routes defined, %d call sites checked (direction b2a); "
          "%d marked NO_ROUTE, 0 offenders; %s"
          % (len(defined), len(calls), len(exempted), media_note))
    for method, reason in sorted(set(exempted)):
        print("  exempt: %s (%s)" % (method, reason))
except SystemExit:
    raise
except Exception as e:
    if os.environ.get("LINKAGE_DEBUG"):
        traceback.print_exc()
    print("%s: error: %s: %s" % (name, type(e).__name__, e), file=sys.stderr)
    sys.exit(2)
PYEOF
case "$rc" in 0|1) ;; *) rc=2 ;; esac
exit "$rc"

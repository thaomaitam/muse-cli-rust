#!/usr/bin/env bash
# commands-help — every leaf command defined in src/cli.rs must appear in
# the built binary's --help output, and vice versa.
#
# A (definition site): the arms of Command::leaf_name() in src/cli.rs.
# That function is the single source of leaf display names in the crate
# (37 regular leaves + "auth export" + 12 "fs ..." leaves = 50). The
# bare-parent arm `None => "fs"` is NOT a leaf (bare `fs` is rejected
# with exit 2 before connecting) and is skipped explicitly.
# B (appearance sites): the real binary's help text — top-level --help,
# `auth --help`, `fs --help` — plus each leaf's own `<leaf> --help`
# (which must exit 0), and docs/cli-surface.md when that file exists.
# If docs/cli-surface.md is absent, the docs part is SKIPPED (not a
# failure) and the skip is printed; as soon as the file appears, the
# same script enforces it with no flag change.
# Runtime breakage on drift: a leaf missing from help means clap and
# leaf_name() disagree about the CLI surface; a leaf missing from docs
# means the published surface lies about what the binary offers.
#
# Directions (default both; override with --direction or
# LINKAGE_DIRECTION): a2b = every cli.rs leaf in help (+docs);
# b2a = every help leaf in cli.rs; both = a2b and b2a.
#
# Escape hatch (a2b only): a leaf whose attached comment block (the
# contiguous `//` lines directly above its leaf_name arm, no blank line
# in between) contains "NO_HELP: <reason>" is exempted. The reason is
# required and is echoed in the PASS report. A bare NO_HELP marker
# without a reason is a misconfiguration (exit 2).
#
# Test hooks (environment overrides; defaults shown):
#   LINKAGE_CLI_RS   src/cli.rs under the repo root
#   LINKAGE_BIN      target/debug/muse-cli under the repo root
#   LINKAGE_DOCS     docs/cli-surface.md under the repo root
#   LINKAGE_DIRECTION  a2b | b2a | both (default: both)
#   LINKAGE_DEBUG=1  show Python tracebacks on internal errors
#
# Exit codes: 0 pass · 1 offenders found · 2 usage / misconfiguration /
# internal error (one-line diagnostic on stderr). Exit 1 means only
# real offenders; nothing else exits 1.

set -euo pipefail

SELF="${BASH_SOURCE[0]}"
CHECK_NAME="$(basename "$SELF" .sh)"
die() { echo "$CHECK_NAME: error: $*" >&2; exit 2; }
usage() { echo "usage: $CHECK_NAME.sh [--direction a2b|b2a|both] [--debug]" >&2; exit 2; }

while [[ $# -gt 0 ]]; do
    case "$1" in
        --direction) [[ $# -ge 2 && -n "$2" ]] || usage; LINKAGE_DIRECTION="$2"; shift 2 ;;
        --debug) LINKAGE_DEBUG=1; shift ;;
        -h|--help) usage ;;
        *) usage ;;
    esac
done

REPO_ROOT="$(cd "$(dirname "$SELF")/../.." && pwd)"
LINKAGE_CLI_RS="${LINKAGE_CLI_RS:-$REPO_ROOT/src/cli.rs}"
LINKAGE_BIN="${LINKAGE_BIN:-$REPO_ROOT/target/debug/muse-cli}"
LINKAGE_DOCS="${LINKAGE_DOCS:-$REPO_ROOT/docs/cli-surface.md}"
LINKAGE_DIRECTION="${LINKAGE_DIRECTION:-both}"
LINKAGE_DEBUG="${LINKAGE_DEBUG:-}"

command -v python3 >/dev/null 2>&1 || die "python3 not found on PATH"
[[ -f "$LINKAGE_CLI_RS" ]] || die "LINKAGE_CLI_RS is not a file: $LINKAGE_CLI_RS"
[[ -x "$LINKAGE_BIN" ]] || die "LINKAGE_BIN is not executable (run cargo build first): $LINKAGE_BIN"

export LINKAGE_CLI_RS LINKAGE_BIN LINKAGE_DOCS LINKAGE_DIRECTION LINKAGE_DEBUG CHECK_NAME

rc=0
python3 - <<'PYEOF' || rc=$?
import os
import re
import subprocess
import sys
import traceback

name = os.environ["CHECK_NAME"]

def misconfig(msg):
    print("%s: error: %s" % (name, msg), file=sys.stderr)
    sys.exit(2)

try:
    direction = os.environ["LINKAGE_DIRECTION"]
    if direction not in ("a2b", "b2a", "both"):
        misconfig("direction must be a2b, b2a or both (got %r)" % direction)
    cli_rs = os.environ["LINKAGE_CLI_RS"]
    binary = os.environ["LINKAGE_BIN"]
    docs_path = os.environ["LINKAGE_DOCS"]

    with open(cli_rs, encoding="utf-8") as f:
        src_lines = f.read().splitlines()

    # --- Parse A: arms of leaf_name() only (not other match arms). ---
    start = None
    for i, line in enumerate(src_lines):
        if "pub fn leaf_name" in line:
            start = i
            break
    if start is None:
        misconfig("leaf_name() not found in %s" % cli_rs)
    end = None
    for i in range(start, len(src_lines)):
        if src_lines[i] == "    }":
            end = i
            break
    if end is None:
        misconfig("end of leaf_name() not found in %s" % cli_rs)
    body = src_lines[start:end + 1]

    defs = []  # (leaf, exempt_reason or None)
    for j, line in enumerate(body):
        m = re.search(r'=>\s*"([^"]+)"', line)
        if not m:
            continue
        leaf = m.group(1)
        if "None =>" in line and leaf == "fs":
            continue  # bare parent, not a leaf (see header)
        reason = None
        k = j - 1
        while k >= 0 and re.match(r'\s*//', body[k]):
            mm = re.search(r'NO_HELP:\s*(\S.*)$', body[k])
            if mm:
                reason = mm.group(1).strip()
            elif re.search(r'\bNO_HELP\b', body[k]):
                misconfig("%s:%d: NO_HELP marker without a reason" % (cli_rs, start + j + 1))
            k -= 1
        defs.append((leaf, reason))
    if not defs:
        misconfig("no leaf definitions parsed from leaf_name() in %s" % cli_rs)
    cli_leaves = {leaf for leaf, _ in defs}
    exempted = [(leaf, r) for leaf, r in defs if r]

    # --- Parse B: real help output (never the source text alone). ---
    def run_help(argv):
        try:
            proc = subprocess.run(argv, capture_output=True, text=True, timeout=30)
        except (OSError, subprocess.TimeoutExpired) as e:
            misconfig("cannot run %s: %s" % (" ".join(argv), e))
        return proc

    def commands_section(text):
        names = []
        in_section = False
        for line in text.splitlines():
            if line.strip() == "Commands:":
                in_section = True
                continue
            if in_section:
                if line.strip().startswith("Options:"):
                    break
                if line.strip() == "":
                    continue
                m = re.match(r'^  ([a-z0-9][a-z0-9-]*)(\s|$)', line)
                if m:
                    names.append(m.group(1))
                elif line.startswith("  "):
                    continue
                else:
                    break
        return names

    top = run_help([binary, "--help"])
    if top.returncode != 0:
        misconfig("%s --help exited %d: %s" % (binary, top.returncode, top.stderr.strip()))
    top_names = commands_section(top.stdout)
    if not top_names:
        misconfig("no Commands section parsed from %s --help" % binary)
    auth = run_help([binary, "auth", "--help"])
    fs = run_help([binary, "fs", "--help"])
    if auth.returncode != 0 or fs.returncode != 0:
        misconfig("auth/fs --help failed (auth=%d fs=%d)" % (auth.returncode, fs.returncode))
    help_leaves = set()
    for n in top_names:
        if n not in ("auth", "fs"):
            help_leaves.add(n)
    for n in commands_section(auth.stdout):
        help_leaves.add("auth " + n)
    for n in commands_section(fs.stdout):
        help_leaves.add("fs " + n)

    # Each cli leaf's own --help must also exit 0.
    help_ok = {}
    for leaf in sorted(cli_leaves):
        argv = [binary] + leaf.split(" ") + ["--help"]
        help_ok[leaf] = run_help(argv).returncode == 0

    # --- Docs (only when the file exists). ---
    docs_present = os.path.isfile(docs_path)
    docs_text = ""
    if docs_present:
        with open(docs_path, encoding="utf-8") as f:
            docs_text = f.read()

    def in_docs(leaf):
        parts = leaf.split(" ")
        pat = r'(?<![A-Za-z0-9_-])' + r'\s+'.join(re.escape(p) for p in parts) + r'(?![A-Za-z0-9_-])'
        return re.search(pat, docs_text) is not None

    missing_help, missing_docs, dangling = [], [], []
    if direction in ("a2b", "both"):
        for leaf, reason in defs:
            if leaf in help_leaves and help_ok.get(leaf):
                pass
            elif reason:
                continue  # exempted; listed below
            else:
                missing_help.append(leaf)
            if docs_present and not in_docs(leaf) and not reason:
                missing_docs.append(leaf)
    if direction in ("b2a", "both"):
        dangling = sorted(n for n in help_leaves if n not in cli_leaves)

    if missing_help or missing_docs or dangling:
        if missing_help:
            print("FAIL: %d leaf commands missing from --help:" % len(missing_help))
            for leaf in sorted(missing_help):
                print("  - %s (cli.rs leaf_name, not in help or own --help failed)" % leaf)
        if missing_docs:
            print("FAIL: %d leaf commands missing from docs:" % len(missing_docs))
            for leaf in sorted(missing_docs):
                print("  - %s (cli.rs leaf_name, not in %s)" % (leaf, docs_path))
        if dangling:
            print("FAIL: %d help leaves with no cli.rs definition:" % len(dangling))
            for leaf in dangling:
                print("  - %s (in --help, not in cli.rs leaf_name)" % leaf)
        sys.exit(1)

    if docs_present:
        docs_note = "docs checked (%d leaves)" % len(cli_leaves)
    else:
        docs_note = "docs SKIPPED (%s absent; enforced automatically when it appears)" % docs_path
    print("PASS: %d leaf commands, %d help leaves (direction %s); %d marked NO_HELP, 0 offenders; %s"
          % (len(cli_leaves), len(help_leaves), direction, len(exempted), docs_note))
    for leaf, reason in sorted(exempted):
        print("  exempt: %s (%s)" % (leaf, reason))
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

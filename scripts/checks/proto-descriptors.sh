#!/usr/bin/env bash
# proto-descriptors — every field number and enum value in the protobuf
# descriptors assets/desc0.bin and assets/desc1.bin must match
# src/proto.rs, and vice versa.
#
# A (definition site): the two descriptor files, decoded here with a
# small pure-Python protobuf wire parser (no python protobuf package
# is installed on this machine, and the Rust test that does the same
# check with prost_types lives in src/proto.rs itself, so a standalone
# check must not depend on cargo). desc0 is noise_transport.proto
# (package ingress_rev_proxy), desc1 is noise_envelope.proto
# (package hatch.noise). Messages, nested messages, and nested enums
# (Reset.Code) are all collected, exactly as the proto.rs test does.
# B (appearance site): src/proto.rs — `#[prost(..., tag = "N")]`
# field attributes, oneof variant tags in ServiceFrameKind, and
# enum discriminants (`Variant = N`).
# Direction default both: descriptor -> Rust (a missing or wrongly
# numbered Rust field breaks the wire format) and Rust -> descriptor
# (a Rust field the descriptor never defines is dead wire data).
# Runtime breakage on drift: a field-number mismatch silently
# mis-decodes gateway frames instead of failing loudly.
#
# Enum value mapping, verified against the real descriptors and
# proto.rs (not guessed): ServiceType values all carry the SERVICE_
# prefix (SERVICE_AUTHD -> Authd); Code values carry no prefix except
# the first, which is CODE_UNSPECIFIED -> CodeUnspecified, while the
# rest map directly (CANCELLED -> Cancelled, PROTOCOL_ERROR ->
# ProtocolError). An enum with no mapping in this script is reported
# as an offender, not silently skipped, so a new descriptor enum
# forces a mapping decision here.
#
# Escape hatch: descriptors are binary, so no comment can be attached
# to a definition the way the other checks do it. Instead, an optional
# exemption file (LINKAGE_EXEMPT_FILE, absent by default) lists
# `Message.field: reason` or `Enum.Variant: reason` lines; a listed
# descriptor field/value is exempted and echoed in the PASS report.
# A line without a reason is a misconfiguration (exit 2). The real
# repo has no exemption file and no exemptions.
#
# Test hooks (environment overrides; defaults shown):
#   LINKAGE_DESC0        assets/desc0.bin under the repo root
#   LINKAGE_DESC1        assets/desc1.bin under the repo root
#   LINKAGE_PROTO_RS     src/proto.rs under the repo root
#   LINKAGE_EXEMPT_FILE  (unset = no exemptions)
#   LINKAGE_DIRECTION    a2b | b2a | both (default: both)
#   LINKAGE_DEBUG=1      show Python tracebacks on internal errors
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
LINKAGE_DESC0="${LINKAGE_DESC0:-$REPO_ROOT/assets/desc0.bin}"
LINKAGE_DESC1="${LINKAGE_DESC1:-$REPO_ROOT/assets/desc1.bin}"
LINKAGE_PROTO_RS="${LINKAGE_PROTO_RS:-$REPO_ROOT/src/proto.rs}"
LINKAGE_DIRECTION="${LINKAGE_DIRECTION:-both}"
LINKAGE_DEBUG="${LINKAGE_DEBUG:-}"

command -v python3 >/dev/null 2>&1 || die "python3 not found on PATH"
[[ -f "$LINKAGE_DESC0" ]] || die "LINKAGE_DESC0 is not a file: $LINKAGE_DESC0"
[[ -f "$LINKAGE_DESC1" ]] || die "LINKAGE_DESC1 is not a file: $LINKAGE_DESC1"
[[ -f "$LINKAGE_PROTO_RS" ]] || die "LINKAGE_PROTO_RS is not a file: $LINKAGE_PROTO_RS"

export LINKAGE_DESC0 LINKAGE_DESC1 LINKAGE_PROTO_RS LINKAGE_EXEMPT_FILE LINKAGE_DIRECTION LINKAGE_DEBUG CHECK_NAME

rc=0
python3 - <<'PYEOF' || rc=$?
import os
import re
import sys
import traceback

name = os.environ["CHECK_NAME"]

def misconfig(msg):
    print("%s: error: %s" % (name, msg), file=sys.stderr)
    sys.exit(2)

# --- Minimal protobuf wire parser (varint + length-delimited only). ---
def parse_fields(buf, pos, end):
    out = []
    while pos < end:
        tag, pos = read_varint(buf, pos)
        field_no, wire = tag >> 3, tag & 7
        if wire == 0:
            value, pos = read_varint(buf, pos)
            out.append((field_no, value))
        elif wire == 2:
            length, pos = read_varint(buf, pos)
            out.append((field_no, buf[pos:pos + length]))
            pos += length
        elif wire == 5:
            out.append((field_no, buf[pos:pos + 4]))
            pos += 4
        elif wire == 1:
            out.append((field_no, buf[pos:pos + 8]))
            pos += 8
        else:
            misconfig("unsupported wire type %d" % wire)
    return out

def read_varint(buf, pos):
    result = 0
    shift = 0
    while True:
        if pos >= len(buf):
            misconfig("truncated varint in descriptor")
        b = buf[pos]
        pos += 1
        result |= (b & 0x7F) << shift
        if not (b & 0x80):
            return result, pos
        shift += 7

def text_value(v):
    return v.decode("utf-8", "replace") if isinstance(v, bytes) else v

def parse_file_descriptor(data):
    fd = {"name": None, "messages": {}, "enums": {}}
    for no, v in parse_fields(data, 0, len(data)):
        if no == 1:
            fd["name"] = text_value(v)
        elif no == 4:
            parse_message(v, fd)
        elif no == 5:
            parse_enum(v, fd)
    return fd

def parse_message(data, fd):
    msg = {"name": None, "fields": {}}
    nested = []
    nested_enums = []
    for no, v in parse_fields(data, 0, len(data)):
        if no == 1:
            msg["name"] = text_value(v)
        elif no == 2:
            fname, number = None, None
            for fno, fv in parse_fields(v, 0, len(v)):
                if fno == 1:
                    fname = text_value(fv)
                elif fno == 3:
                    number = fv
            if fname is not None:
                msg["fields"][fname] = number
        elif no == 3:
            nested.append(v)
        elif no == 4:
            nested_enums.append(v)
    if msg["name"] is None:
        misconfig("descriptor message without a name")
    fd["messages"][msg["name"]] = msg["fields"]
    for n in nested:
        parse_message(n, fd)
    for e in nested_enums:
        parse_enum(e, fd)

def parse_enum(data, fd):
    ename, values = None, {}
    for no, v in parse_fields(data, 0, len(data)):
        if no == 1:
            ename = text_value(v)
        elif no == 2:
            vname, number = None, None
            for fno, fv in parse_fields(v, 0, len(v)):
                if fno == 1:
                    vname = text_value(fv)
                elif fno == 2:
                    number = fv
            if vname is not None:
                values[vname] = number
    if ename is None:
        misconfig("descriptor enum without a name")
    fd["enums"][ename] = values

def snake_case(camel):
    out = []
    for i, ch in enumerate(camel):
        if ch.isupper() and i > 0 and (not camel[i - 1].isupper() or
                                       (i + 1 < len(camel) and camel[i + 1].islower())):
            out.append("_")
        out.append(ch.lower())
    return "".join(out)

def screaming_to_pascal(s):
    return "".join(part[:1].upper() + part[1:].lower() for part in s.split("_") if part)

def map_enum_value(ename, vname):
    """Descriptor enum value -> Rust variant name, or None if unmapped."""
    if ename == "ServiceType":
        if vname.startswith("SERVICE_"):
            return screaming_to_pascal(vname[len("SERVICE_"):])
        return None
    if ename == "Code":
        if vname == "CODE_UNSPECIFIED":
            return "CodeUnspecified"
        return screaming_to_pascal(vname)
    return None

try:
    direction = os.environ["LINKAGE_DIRECTION"]
    if direction not in ("a2b", "b2a", "both"):
        misconfig("direction must be a2b, b2a or both (got %r)" % direction)
    with open(os.environ["LINKAGE_DESC0"], "rb") as f:
        desc0 = parse_file_descriptor(f.read())
    with open(os.environ["LINKAGE_DESC1"], "rb") as f:
        desc1 = parse_file_descriptor(f.read())
    if desc0["name"] != "noise_transport.proto":
        misconfig("desc0 is not noise_transport.proto (got %r)" % desc0["name"])
    if desc1["name"] != "noise_envelope.proto":
        misconfig("desc1 is not noise_envelope.proto (got %r)" % desc1["name"])
    desc_messages = {}
    desc_messages.update(desc0["messages"])
    desc_messages.update(desc1["messages"])
    desc_enums = {}
    desc_enums.update(desc0["enums"])
    desc_enums.update(desc1["enums"])
    if not desc_messages:
        misconfig("no messages parsed from descriptors")

    with open(os.environ["LINKAGE_PROTO_RS"], encoding="utf-8") as f:
        rust = f.read()
    # Strip the test module: its assertions duplicate the mapping and
    # must not be mistaken for struct/enum definitions.
    rust = rust.split("#[cfg(test)]", 1)[0]

    rust_fields = {}  # message -> {field: tag}
    for m in re.finditer(r'pub struct (\w+) \{(.*?)\n\}', rust, re.S):
        sname, body = m.group(1), m.group(2)
        fields = {}
        for attr in re.finditer(r'#\[prost\(([^)]*)\)\]\s*pub (\w+):', body):
            args, fname = attr.group(1), attr.group(2)
            tag = re.search(r'tag\s*=\s*"(\d+)"', args)
            if tag:
                fields[fname] = int(tag.group(1))
        rust_fields[sname] = fields
    # Oneof variants live in their own enum; fold them into the owning
    # message's field map (ServiceFrame) by snake-cased variant name.
    oneof_owner = {"ServiceFrameKind": "ServiceFrame"}
    for m in re.finditer(r'pub enum (\w+) \{(.*?)\n\}', rust, re.S):
        ename, body = m.group(1), m.group(2)
        if ename in oneof_owner:
            owner = oneof_owner[ename]
            for vm in re.finditer(r'#\[prost\([^)]*tag\s*=\s*"(\d+)"[^)]*\)\]\s*(\w+)', body):
                rust_fields.setdefault(owner, {})[snake_case(vm.group(2))] = int(vm.group(1))

    rust_enums = {}  # enum -> {variant: value}
    for m in re.finditer(r'pub enum (\w+) \{(.*?)\n\}', rust, re.S):
        ename, body = m.group(1), m.group(2)
        if ename in oneof_owner:
            continue
        variants = {}
        for vm in re.finditer(r'^\s*(\w+)\s*=\s*(\d+)\s*,', body, re.M):
            variants[vm.group(1)] = int(vm.group(2))
        rust_enums[ename] = variants

    # Exemptions (file-based; see header for why).
    exempt = {}
    exempt_path = os.environ.get("LINKAGE_EXEMPT_FILE") or ""
    if exempt_path:
        if not os.path.isfile(exempt_path):
            misconfig("LINKAGE_EXEMPT_FILE is not a file: %s" % exempt_path)
        with open(exempt_path, encoding="utf-8") as f:
            for line in f:
                line = line.strip()
                if not line or line.startswith("#"):
                    continue
                mm = re.match(r'^([\w.]+):\s*(\S.*)$', line)
                if not mm:
                    misconfig("exemption line without a reason: %r" % line)
                exempt[mm.group(1)] = mm.group(2).strip()

    offenders = []
    exempted = []

    def check_field(msg, field, dtag, rtag, where):
        key = "%s.%s" % (msg, field)
        if key in exempt:
            exempted.append((key, exempt[key]))
            return
        if dtag != rtag:
            offenders.append("%s (descriptor tag %s, rust tag %s, %s)" % (key, dtag, rtag, where))

    if direction in ("a2b", "both"):
        for msg in sorted(desc_messages):
            for field in sorted(desc_messages[msg]):
                key = "%s.%s" % (msg, field)
                if msg not in rust_fields:
                    if key in exempt:
                        exempted.append((key, exempt[key]))
                    else:
                        offenders.append("%s (descriptor tag %d, no such struct in proto.rs)"
                                         % (key, desc_messages[msg][field]))
                    continue
                if field not in rust_fields[msg]:
                    if key in exempt:
                        exempted.append((key, exempt[key]))
                    else:
                        offenders.append("%s (descriptor tag %d, missing in proto.rs)"
                                         % (key, desc_messages[msg][field]))
                else:
                    check_field(msg, field, desc_messages[msg][field], rust_fields[msg][field], "field")
        for ename in sorted(desc_enums):
            if ename not in ("ServiceType", "Code"):
                offenders.append("enum %s (no value mapping in check; extend map_enum_value)" % ename)
                continue
            # Descriptor enum Code (nested in Reset) is ResetCode in Rust.
            rust_ename = {"Code": "ResetCode"}.get(ename, ename)
            rvariants = rust_enums.get(rust_ename, {})
            for vname in sorted(desc_enums[ename]):
                variant = map_enum_value(ename, vname)
                if variant is None:
                    offenders.append("enum %s value %s (no value mapping in check)" % (ename, vname))
                    continue
                key = "%s.%s" % (ename, variant)
                if variant not in rvariants:
                    if key in exempt:
                        exempted.append((key, exempt[key]))
                    else:
                        offenders.append("%s (descriptor value %d, missing in proto.rs)"
                                         % (key, desc_enums[ename][vname]))
                elif rvariants[variant] != desc_enums[ename][vname]:
                    if key in exempt:
                        exempted.append((key, exempt[key]))
                    else:
                        offenders.append("%s (descriptor value %d, rust value %d)"
                                         % (key, desc_enums[ename][vname], rvariants[variant]))
    if direction in ("b2a", "both"):
        for msg in sorted(rust_fields):
            for field in sorted(rust_fields[msg]):
                if msg not in desc_messages or field not in desc_messages.get(msg, {}):
                    key = "%s.%s" % (msg, field)
                    if key in exempt:
                        exempted.append((key, exempt[key]))
                    else:
                        offenders.append("%s (rust tag %d, not in descriptor)"
                                         % (key, rust_fields[msg][field]))
        for ename in sorted(desc_enums):
            if ename not in ("ServiceType", "Code"):
                continue  # already reported (or unmapped) in the a2b pass
            rust_ename = {"Code": "ResetCode"}.get(ename, ename)
            mapped = {}
            for v in desc_enums[ename]:
                mv = map_enum_value(ename, v)
                if mv is not None:
                    mapped[mv] = v
            for variant in sorted(rust_enums.get(rust_ename, {})):
                if variant not in mapped:
                    key = "%s.%s" % (ename, variant)
                    if key in exempt:
                        exempted.append((key, exempt[key]))
                    else:
                        offenders.append("%s (rust value %d, not in descriptor)"
                                         % (key, rust_enums[rust_ename][variant]))

    if offenders:
        print("FAIL: %d descriptor/proto mismatches:" % len(offenders))
        for o in sorted(set(offenders)):
            print("  - %s" % o)
        sys.exit(1)

    nfields = sum(len(v) for v in desc_messages.values())
    nvals = sum(len(v) for v in desc_enums.values())
    print("PASS: %d messages (%d fields) and %d enums (%d values) match proto.rs "
          "(direction %s); %d marked exempt, 0 offenders"
          % (len(desc_messages), nfields, len(desc_enums), nvals, direction, len(set(exempted))))
    for key, reason in sorted(set(exempted)):
        print("  exempt: %s (%s)" % (key, reason))
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

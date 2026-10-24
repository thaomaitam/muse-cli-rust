# Linkage checks

Invariants of the form "defined in A must appear in B", as runnable
scripts. Run them before committing:

```sh
bash scripts/checks/run-all.sh   # every check + every fixture runner
# or one check at a time:
bash scripts/checks/commands-help.sh
bash scripts/checks/routes-called.sh
bash scripts/checks/proto-descriptors.sh
```

Exit codes for every check: `0` pass · `1` offenders found ·
`2` usage / misconfiguration / internal error. A check's stdout ends
with a `PASS:` or `FAIL:` line naming the offenders. Every check
accepts environment overrides for its inputs (see its header), so the
fixture runners can point it at miniature trees.

## Checks

| Script | A (definition) | B (must appear in) | Direction |
|---|---|---|---|
| `commands-help.sh` | Leaf commands in `Command::leaf_name()` (`src/cli.rs`, 50 leaves) | Real binary `--help` (top, `auth`, `fs`, plus each leaf's own `--help`) and `docs/cli-surface.md` | both |
| `routes-called.sh` | `assets/routes.json` (258 methods) | Gateway methods the code calls (`call_json` / `open_stream` / `FsClient::call` in `src/*.rs`) | b2a only (see below) |
| `proto-descriptors.sh` | `assets/desc0.bin` + `assets/desc1.bin` (protobuf descriptors) | Field tags, oneof tags, enum values in `src/proto.rs` | both |

## Deliberately not shipped

- **routes a2b (every route must be called): not a valid invariant.**
  Only 27 of the 258 routes appear as literals in `src/`; the rest are
  reachable through the `raw` command, which takes any method name
  from the user and resolves it at runtime through `routes.json`.
  Shipping a2b would fail on 231 routes that are correct by design.
  `routes-called.sh` therefore checks only b2a (a called method must
  be defined), plus duplicate method names in `routes.json`.
- **routes-called special cases:** dynamic
  `call_json(&format!("session.{kind}"))` is expanded from the
  literal kinds passed to `cmd_session_op` in the same file
  (rename/pin/unpin/archive/unarchive/delete); a format call with no
  discoverable kinds is exit 2, never a silent pass. Test modules
  (from the first `#[cfg(test)]` in a file onward) are excluded,
  because tests deliberately call unknown methods to check the error
  path. `media.raw` is synthetic and verified absent from
  `routes.json` by design (built via `Gateway::request_with_route`,
  see `src/routes.rs`); it is a stream label, not a route lookup.
- **commands-help docs part:** if `docs/cli-surface.md` is absent,
  that part is SKIPPED (printed, not a failure); as soon as the file
  appears, the same script enforces it with no flag change. The bare
  `fs` arm (`None => "fs"`) is not a leaf and is skipped explicitly.
- **proto-descriptors enum mapping:** descriptor enum `Code`
  (nested in `Reset`) is `ResetCode` in Rust; `ServiceType` values
  strip the `SERVICE_` prefix, while `Code` values map directly
  except `CODE_UNSPECIFIED` -> `CodeUnspecified`. These mappings were
  verified against the real descriptors and `src/proto.rs`, not
  guessed. An enum with no mapping is reported as an offender.

## Escape hatches

Every hatch requires a reason, which is echoed in the PASS report.
A marker without a reason is exit 2.

- `commands-help.sh`: `NO_HELP: <reason>` in the contiguous `//`
  comment block directly above a `leaf_name()` arm.
- `routes-called.sh`: `NO_ROUTE: <reason>` in the contiguous `//`
  comment block directly above a call site.
- `proto-descriptors.sh`: descriptors are binary, so no comment can
  attach to a definition. Instead, `LINKAGE_EXEMPT_FILE` lists
  `Message.field: reason` / `Enum.Variant: reason` lines. The real
  repo has no exemption file and no exemptions.

## Fixtures

Each check has `tests/fixtures/<name>/{pass,fail,exempt}/` plus
`run-fixtures.sh`, which must print `PASS: 3 fixtures`:

- `pass/`: miniature A/B trees that pass.
- `fail/`: one synthetic offender, named in `EXPECT_OFFENDER`;
  the runner asserts the check exits 1 and names it.
- `exempt/`: the offender carrying the escape hatch, named in
  `EXPECT_EXEMPT`; the runner asserts exit 0 and the exempt listing.

The `commands-help` fixture runner was additionally verified to go
red when its fail fixture was deliberately broken (offender added to
the fake help/docs, so the check passed but the runner expected
exit 1), then green again after restoring the fixture.

## Enforcement levels

- Local validation: these scripts, run by hand via `run-all.sh`.
  That is the validation owner in this repo.
- Hook: none installed (not authorised, not needed).
- CI invocation: none — this repo has no CI.
- Branch protection: unverified / not configured by this change.

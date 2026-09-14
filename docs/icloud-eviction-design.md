# Space Lens iCloud Rust MVP

## Goal

Add a macOS-only Rust core and CLI flow that can inspect iCloud Drive local residency, produce a conservative dry-run eviction plan, and—only after an explicit execution flag—request removal of local copies without falling back to deletion.

This design also fixes the local `ICloudFree.app` bundle metadata so the generated Space Lens icon is the icon shown by Dock.

## Current-repository boundary

The current checkout is `main` at `d7b38b2`. It contains the `space-lens` Rust core, the NAPI addon, and the Rust CLI. It does not contain the `kunkun-ext` Web service or the Web/Kunkun files described by the reference document in `~/Downloads`; those integrations are not part of this MVP.

The existing cleanup implementation remains unchanged. iCloud eviction is a separate module and never calls `remove_file`, `remove_dir_all`, Trash, `brctl`, xattrs, or private FileProvider APIs.

## Architecture

`packages/space-lens/src/cloud/` owns platform-neutral models, eligibility policy, metadata-only traversal, fingerprint checks, and the execution state machine. On macOS, `platform.rs` wraps public Foundation APIs through `objc2-foundation`; other platforms return `UnsupportedPlatform`.

The CLI adds an `icloud` namespace with `inspect`, `plan`, and `evict` commands. `inspect` and `plan` are read-only. `evict` is dry-run unless `--execute` is supplied, and execution re-inspects each candidate before calling Foundation. No real user iCloud path is used by automated tests.

## Safety invariants

- Only ordinary regular files are candidates; directories, packages, symlinks, hard-link duplicates, and unknown item types are excluded.
- Missing or unrecognized iCloud metadata is `Unknown`, never an implicit safe state.
- Candidates must be iCloud items in a current, uploaded, non-uploading, non-downloading, conflict-free state with known non-zero local allocation.
- Plans record fingerprints and cannot execute if the file changed since planning.
- Execution is sequential and reports per-item outcomes; a Foundation failure is not converted into deletion.
- Byte counts remain Rust `u64`; JSON-facing counts are decimal strings where a new wire type is needed.
- Automated verification stops at compile/tests/dry-run. Real eviction/download requires a separate disposable iCloud test folder and explicit user authorization.

## Verification boundary

Run Rust formatting, workspace tests, checks and clippy. On macOS also compile the Objective-C binding target and exercise CLI help plus a dry-run against a temporary local directory. Do not claim real iCloud eviction or download has been verified until a disposable synced test file has been authorized and observed before and after the operation.

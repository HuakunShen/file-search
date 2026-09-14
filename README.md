# Kuntu

A local-first file & storage engine. One embeddable workspace for finding,
indexing, scanning, and operating on files across local disks, cloud-backed
folders, and whatever a host product adds on top.

Kuntu is a library, not a service: it is intentionally independent from
Electron, Tauri, Node runtimes, any desktop app database, and **any network
transport** — reaching a peer is the host product's business. The Node surface
is one optional NAPI package. CI enforces both boundaries (no tantivy/mimalloc
in the shipped addon's graph; no transport crate anywhere in the workspace
graph).

History: the search half was extracted from Kunkun's `crates/file-search` with
full history (see `MIGRATION.md`), briefly lived inside space-lens as
`packages/kfs-*` (plan 0064 returned it to a standalone repo), and the
scan/clean/iCloud-eviction half moved here from
[space-lens](https://github.com/HuakunShen/space-lens) `packages/space-lens`
(plan 0066). Consumers pin a commit SHA (via tag), never a floating branch.

## Consumers

| Product | Path | Uses |
|---|---|---|
| [Kunkun](https://github.com/kunkunsh/kunkun) | `crates/file-search` (submodule) | `kuntu-napi` (npm `@kunkunsh/file-search-native`) |
| [Xross](https://github.com/HuakunShen/xross) | `vendors/file-search` (submodule) | `kuntu-core`/`kuntu-crawler`/`kuntu-index`/`kuntu-watcher` (wiring: plan 0062) |
| [space-lens](https://github.com/HuakunShen/space-lens) | `vendors/kuntu` (submodule) | `kuntu-scan` (ffi → SwiftUI, napi, cli) |

## Crates

Search:

- `kuntu-core`: shared search types, path policy, explanation, and ranking.
- `kuntu-crawler`: explicit-root filesystem crawler that applies `kuntu-core` policy.
- `kuntu-index`: persistent metadata index on the turso engine, incremental refresh, repair, and search over crawled entries. The schema carries a monotonic version (`PRAGMA user_version`, `kuntu_index::SCHEMA_VERSION`, currently 2); databases from a newer release are refused with a distinct error, pre-versioning databases with the complete schema are recreated from a rescan, and a file that carries unrelated user tables is refused untouched.
- `kuntu-watcher`: platform-neutral watcher event model with bounded polling maintenance.
- `kuntu-provider-spotlight`: macOS Spotlight provider backed by `mdfind`; other platforms return `Unsupported` rather than falling back.

Scan & operate (moved from space-lens):

- `kuntu-scan`: parallel disk-usage scanner, snapshot envelope, cleanup-candidate finder with plan→confirm→execute removal, and cloud eviction planning (iCloud local-copy eviction with fingerprint revalidation — an eviction failure never falls back to deletion; see `docs/icloud-eviction-design.md`).

Surfaces:

- `kuntu-napi`: Node-API package (`@kunkunsh/file-search-native`, addon binary name `kfs-native`).
- `kuntu-cli`: CLI adapter (binary `kfs`) for search, explain, index, watch, daemon, and benchmark commands.
- `kuntu-daemon`: framework-free HTTP JSON service adapter.

## Safety

Search roots are explicit. Do not run broad full-disk searches while developing this workspace. Manual smoke tests should stay within:

- `~/Desktop`
- `~/Downloads`
- `~/Dev`

Sensitive paths such as `.ssh`, `.aws`, `.gcloud`, `.kube`, `.docker`, `.env`, private keys, credentials, and secrets are denied by default. `kuntu-scan`'s `delete_path` is a permanent, unrestricted delete — product shells are expected to wrap it with their own confirmation; the engine's removal plans are dry-run until executed.

## CLI examples

```bash
cargo run -p kuntu-cli -- explain ~/Dev --root ~/Dev
cargo run -p kuntu-cli -- search "package json" --root ~/Dev --limit 5 --json
cargo run -p kuntu-cli -- index rebuild --root . --db /tmp/kfs.sqlite
cargo run -p kuntu-cli -- index refresh --root . --db /tmp/kfs.sqlite
cargo run -p kuntu-cli -- watch --root . --db /tmp/kfs.sqlite --duration-ms 1000
cargo run -p kuntu-cli -- bench "Cargo toml" --root . --provider sqlite --db /tmp/kfs.sqlite
cargo run -p kuntu-cli -- daemon --root . --db /tmp/kfs.sqlite --addr 127.0.0.1:47865
cargo run -p kuntu-cli -- search "Cargo toml" --root . --provider sqlite --db /tmp/kfs.sqlite --json
```

## Local TypeScript Package

Build the local NAPI package before consuming it from Node or Electron:

```bash
pnpm --dir crates/kuntu-napi install --frozen-lockfile
pnpm --dir crates/kuntu-napi build
pnpm --dir crates/kuntu-napi test
```

The build produces `crates/kuntu-napi/index.js`, `index.d.ts`, and a platform-specific `kfs-native.<platform>-<arch>.node` file. Rebuild on each packaging target; the native binary is not cross-platform and is never committed. `index.js` and `index.d.ts` are generated too: they stay tracked because the package resolves through them before a consumer build, and CI regenerates both and fails on any diff.

Example usage from a Node/Electron main-process runtime:

```js
const { FileSearchIndex } = require("@kunkunsh/file-search-native");

const index = new FileSearchIndex("/tmp/kfs.sqlite");
await index.rebuild([{ path: "/Users/hk/Dev" }]);
const outcome = await index.search({
  roots: [{ path: "/Users/hk/Dev" }],
  query: "package json",
  limit: 10,
});
```

If Spotlight returns an empty array for a scoped search, the provider path is still functioning; it usually means that macOS has not indexed that root or has no matching filename/path metadata for the query. The core filter/ranker can still be tested through unit tests and provider fixtures.

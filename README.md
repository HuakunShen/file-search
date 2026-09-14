# file-search (KFS)

Rust-only local file search workspace. This repository is intentionally
independent from Electron, Tauri, Node runtimes, and any desktop app database
— the Node surface is one optional NAPI package. Extracted from Kunkun's
`crates/file-search` with full history; see `MIGRATION.md`.

Consumed by [Kunkun](https://github.com/kunkunsh/kunkun) (at
`crates/file-search`) and Xross (at `vendors/file-search`) as an exactly
pinned git submodule. Consumers pin a commit SHA, never a floating branch.

## Consumer compatibility

| Shared revision | Schema | Kunkun pin | Xross pin |
|---|---|---|---|
| `v0.1.3` (this repository) | 1 | branch `file-search-submodule` | branch `file-search-vendor` |

Provenance: extracted from Kunkun's `crates/file-search` (nine capability
commits, `22a27b45d..401423928`); briefly merged into space-lens as
`packages/kfs-*` (tag `kfs-v0.1.2` on HuakunShen/space-lens) and returned
to a standalone repository in plan 0064. See `MIGRATION.md`. See
`CONTRIBUTING.md` for the update protocol and rollback.

## Crates

- `kfs-core`: shared search types, path policy, explanation, and ranking.
- `kfs-crawler`: explicit-root filesystem crawler that applies `kfs-core` policy.
- `kfs-index`: persistent metadata index on the turso engine, incremental refresh, repair, and search over crawled entries. The schema carries a monotonic version (`PRAGMA user_version`, `kfs_index::SCHEMA_VERSION`, currently 2); databases from a newer release are refused with a distinct error, pre-versioning databases with the complete schema are recreated from a rescan, and a file that carries unrelated user tables is refused untouched.
- `kfs-daemon`: framework-free HTTP JSON service adapter.
- `kfs-watcher`: platform-neutral watcher event model with bounded polling maintenance.
- `kfs-provider-spotlight`: macOS Spotlight provider backed by `mdfind`; other platforms return `Unsupported` rather than falling back.
- `kfs-napi`: local Node-API package for consuming the SQLite index from TypeScript/Node/Electron.
- `kfs-cli`: local CLI adapter for search, explain, index, watch, and benchmark commands.

## Safety

Search roots are explicit. Do not run broad full-disk searches while developing this workspace. Manual smoke tests should stay within:

- `~/Desktop`
- `~/Downloads`
- `~/Dev`

Sensitive paths such as `.ssh`, `.aws`, `.gcloud`, `.kube`, `.docker`, `.env`, private keys, credentials, and secrets are denied by default.

## Examples

```bash
cargo run -p kfs-cli -- explain ~/Dev --root ~/Dev
cargo run -p kfs-cli -- search "package json" --root ~/Dev --limit 5 --json
cargo run -p kfs-cli -- index rebuild --root . --db /tmp/kfs.sqlite
cargo run -p kfs-cli -- index refresh --root . --db /tmp/kfs.sqlite
cargo run -p kfs-cli -- watch --root . --db /tmp/kfs.sqlite --duration-ms 1000
cargo run -p kfs-cli -- bench "Cargo toml" --root . --provider sqlite --db /tmp/kfs.sqlite
cargo run -p kfs-cli -- daemon --root . --db /tmp/kfs.sqlite --addr 127.0.0.1:47865
cargo run -p kfs-cli -- search "Cargo toml" --root . --provider sqlite --db /tmp/kfs.sqlite --json
```

## Local TypeScript Package

Build the local NAPI package before consuming it from Node or Electron:

```bash
pnpm --dir crates/kfs-napi install --frozen-lockfile
pnpm --dir crates/kfs-napi build
pnpm --dir crates/kfs-napi test
```

The build produces `crates/kfs-napi/index.js`, `index.d.ts`, and a platform-specific `kfs-native.<platform>-<arch>.node` file. Rebuild on each packaging target; the native binary is not cross-platform and is never committed. `index.js` and `index.d.ts` are generated too: they stay tracked because the package resolves through them before a consumer build, and CI regenerates both and fails on any diff.

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

# Contributing

This repository is consumed by Kunkun (`crates/file-search`) and Xross
(`vendors/file-search`) as git submodules pinned to an exact commit. The pin
is the contract: a consumer only moves its gitlink after the shared revision
is proven.

## Editing the shared library

Both consumers check the submodule out on a detached HEAD. Before editing,
switch the submodule checkout to `main` explicitly:

```bash
# In Kunkun's clone
git -C crates/file-search switch main
# In Xross's clone
git -C vendors/file-search switch main
```

Then, in the submodule: edit, test, commit, **push to this repository
first**, and record the new full SHA:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
git push origin main
git rev-parse HEAD   # the SHA both parents will pin
```

Only after the shared revision is pushed and proven do you move a parent's
gitlink, in its own consumer commit:

```bash
# Kunkun
git add crates/file-search
git commit -m "build(file-search): pin the tested upstream revision"

# Xross
git add vendors/file-search
git commit -m "build(file-search): pin the tested upstream revision"
```

Never pin a floating branch, and never use `git submodule update --remote`:
`.gitmodules` deliberately carries `path` and `url` only.

## What must be proven before a pin moves

- Shared Rust matrix (fmt, clippy, tests) plus the NAPI build, Node smoke
  tests and TypeScript declarations — CI does this on every push.
- Consumer-specific tests on top: Kunkun's file-search service, native
  driver, Electron RPC and Hono/OpenAPI suites; Xross's adapter tests and
  the dependency-direction gate.
- A Kunkun-only adapter change does not force Xross to move its pin. A
  shared public-API or schema change must be proven in both.

## Tags and references

Tag an immutable `vX.Y.Z` for every revision a consumer is expected to pin,
and name the exact full SHA in each parent commit description. The tag is a
lookup aid; the gitlink records the commit.

## Schema changes

`kuntu-index` carries a monotonic `SCHEMA_VERSION` (`PRAGMA user_version`,
currently 2 on the turso engine). A schema change bumps it; version 1
databases stamp forward in place (identical schema), version 0 databases
carrying the complete schema are deleted and recreated from a rescan, a
version-0 file with unrelated user tables is refused untouched, and
newer releases are refused with a distinct error — nothing ever tries to
downgrade user data.

## Rollback

A bad shared revision is rolled back by reverting the parent gitlink to the
previous SHA. If a rolled-forward schema is not readable by the older
revision, the consumer creates a new index file and rebuilds from source
directories; the index is derived data and source data is never touched.

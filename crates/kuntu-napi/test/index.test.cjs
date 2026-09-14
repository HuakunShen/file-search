// End-to-end tests for the local NAPI package consumed from Node/Electron runtimes.
const assert = require("node:assert/strict");
const fs = require("node:fs/promises");
const os = require("node:os");
const path = require("node:path");
const { test } = require("node:test");

const { FileSearchIndex } = require("../index.js");

async function withFixture(run) {
  const dir = await fs.mkdtemp(path.join(os.tmpdir(), "kfs-napi-"));
  try {
    return await run(dir);
  } finally {
    await fs.rm(dir, { recursive: true, force: true });
  }
}

test("FileSearchIndex rebuilds, reports status, and searches indexed files", async () => {
  await withFixture(async (dir) => {
    const root = path.join(dir, "root");
    const src = path.join(root, "src");
    await fs.mkdir(src, { recursive: true });
    const canonicalRoot = await fs.realpath(root);
    await fs.writeFile(path.join(root, "package.json"), "{}\n");
    await fs.writeFile(path.join(src, "notes.txt"), "notes\n");

    const index = new FileSearchIndex(path.join(dir, "kfs.sqlite"));
    const rebuild = await index.rebuild([{ path: root }]);
    assert.equal(rebuild.roots, 1);
    assert.ok(rebuild.entries >= 2);
    assert.equal(rebuild.errors.length, 0);

    const status = await index.status();
    assert.equal(status.length, 1);
    assert.equal(status[0].path, canonicalRoot);
    assert.ok(status[0].entryCount >= 2);
    assert.equal(status[0].dirty, false);

    const outcome = await index.search({
      roots: [{ path: root }],
      query: "package json",
      limit: 5,
    });

    assert.ok(outcome.candidateCount >= 1);
    assert.equal(outcome.results[0].path, path.join(canonicalRoot, "package.json"));
    assert.equal(outcome.results[0].provider, "sqlite");
    assert.ok(outcome.results[0].score > 0);
    assert.ok(outcome.results[0].matches.includes("BasenameToken"));
  });
});

test("FileSearchIndex refresh updates indexed results", async () => {
  await withFixture(async (dir) => {
    const root = path.join(dir, "root");
    await fs.mkdir(root, { recursive: true });
    const canonicalRoot = await fs.realpath(root);

    const index = new FileSearchIndex(path.join(dir, "kfs.sqlite"));
    await index.rebuild([{ path: root }]);

    await fs.writeFile(path.join(root, "readme.md"), "hello\n");
    const refresh = await index.refresh([{ path: root }]);
    assert.equal(refresh.roots, 1);
    assert.equal(refresh.inserted, 1);
    assert.equal(refresh.errors.length, 0);

    const outcome = await index.search({
      roots: [{ path: root }],
      query: "readme",
      extensions: ["md"],
      limit: 5,
    });

    assert.equal(outcome.results.length, 1);
    assert.equal(outcome.results[0].path, path.join(canonicalRoot, "readme.md"));
  });
});

// Type-level smoke test for the generated local NAPI package declarations.
import {
  FileSearchIndex,
  type IndexedSearchOutcomeDto,
  type IndexRefreshStatsDto,
  type IndexRebuildStatsDto,
  type IndexRepairStatsDto,
  type IndexRootStatusDto,
} from "../index.js";

const index = new FileSearchIndex("/tmp/kfs.sqlite");

const rebuildPromise: Promise<IndexRebuildStatsDto> = index.rebuild([
  { path: "/tmp/kfs-root" },
]);
const refreshPromise: Promise<IndexRefreshStatsDto> = index.refresh([
  { path: "/tmp/kfs-root" },
]);
const repairPromise: Promise<IndexRepairStatsDto> = index.repair([
  { path: "/tmp/kfs-root" },
]);
const statusPromise: Promise<Array<IndexRootStatusDto>> = index.status();
const searchPromise: Promise<IndexedSearchOutcomeDto> = index.search({
  roots: [{ path: "/tmp/kfs-root" }],
  query: "readme",
  limit: 5,
});

void rebuildPromise;
void refreshPromise;
void repairPromise;
void statusPromise;
void searchPromise;

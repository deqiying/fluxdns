import { http, HttpResponse } from "msw";
import { beforeEach, expect, it } from "vitest";
import { server } from "@/mocks/server";
import { setMockAuthenticated } from "@/mocks/handlers";
import { restoreFilesAndSettle, retryPersistenceAndSettle } from "./operation";

const request = {
  operation_id: "file-operation-1",
  expected: { active_revision: "active-1", observed_file_revision: "file-1" },
  discard_external_changes: true,
};

beforeEach(() => setMockAuthenticated(true));

it("还原结果丢失后只回读 operation，不会再次覆盖文件", async () => {
  let restores = 0;
  let reads = 0;
  server.use(
    http.post("/api/v2/config/files/restore", () => {
      restores += 1;
      return HttpResponse.error();
    }),
    http.get("/api/v2/config/operations/file-operation-1", () => {
      reads += 1;
      return HttpResponse.json({
        operation_id: "file-operation-1",
        status: { state: "applied_synced", active_revision: "active-1", persisted_revision: "active-1" },
      });
    }),
  );
  await expect(restoreFilesAndSettle(request, { pollIntervalMs: 0 })).resolves.toMatchObject({ kind: "settled" });
  expect(restores).toBe(1);
  expect(reads).toBe(1);
});

it("未同步重试只调用专用文件 endpoint", async () => {
  let retries = 0;
  server.use(http.post("/api/v2/config/files/retry", async ({ request: incoming }) => {
    retries += 1;
    expect(await incoming.json()).toEqual({ ...request, discard_external_changes: false });
    return HttpResponse.json({
      operation_id: "file-operation-1",
      status: { state: "applied_synced", active_revision: "active-1", persisted_revision: "active-1" },
    });
  }));
  await expect(retryPersistenceAndSettle({ ...request, discard_external_changes: false })).resolves.toMatchObject({
    kind: "settled",
  });
  expect(retries).toBe(1);
});

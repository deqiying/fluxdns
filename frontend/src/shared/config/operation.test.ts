import { http, HttpResponse } from "msw";
import { beforeEach, describe, expect, it } from "vitest";
import { server } from "@/mocks/server";
import { setMockAuthenticated } from "@/mocks/handlers";
import type { ApplyRequest, ConfigState } from "./api";
import { applyAndSettle, settleOperation } from "./operation";

const expected = { active_revision: "active-1", observed_file_revision: "file-1" };
const state: ConfigState = {
  active_revision: "active-1",
  runtime_revision: "runtime-1",
  persisted_revision: "active-1",
  observed_file_revision: "file-1",
  files: { source: "unchanged", derived: null },
  synchronization: "synced",
  operation_id: null,
};

function applyRequest(operationId: string): ApplyRequest {
  return {
    operation_id: operationId,
    candidate: {
      expected,
      discard_external_changes: false,
      changes: [{ module: "logs", change: { enable: true, level: "info", path: "logs/fluxdns.log" } }],
    },
    validation_token: "validation-1",
    confirmations: [],
  };
}

describe("配置操作回读", () => {
  beforeEach(() => setMockAuthenticated(true));

  it("网络结果未知时不重放 apply，只按同一 operation_id 回读", async () => {
    let applies = 0;
    let reads = 0;
    server.use(
      http.post("/api/v2/config/apply", () => {
        applies += 1;
        return HttpResponse.error();
      }),
      http.get("/api/v2/config/operations/operation-1", () => {
        reads += 1;
        return HttpResponse.json({
          operation_id: "operation-1",
          status: { state: "applied_synced", active_revision: "active-2", persisted_revision: "active-2" },
        });
      }),
    );

    await expect(applyAndSettle(applyRequest("operation-1"), { pollIntervalMs: 0 })).resolves.toMatchObject({
      kind: "settled",
      operation: { status: { state: "applied_synced" } },
    });
    expect(applies).toBe(1);
    expect(reads).toBe(1);
  });

  it("进行中状态有界轮询，unknown 回读活动配置而不推断未执行", async () => {
    let reads = 0;
    server.use(
      http.get("/api/v2/config/operations/operation-2", () => {
        reads += 1;
        return HttpResponse.json({ operation_id: "operation-2", status: { state: "unknown" } });
      }),
      http.get("/api/v2/config/state", () => HttpResponse.json(state)),
    );

    await expect(settleOperation(
      { operation_id: "operation-2", status: { state: "applying" } },
      { maxPolls: 1, pollIntervalMs: 0 },
    )).resolves.toEqual({
      kind: "unknown",
      operation: { operation_id: "operation-2", status: { state: "unknown" } },
      state,
    });
    expect(reads).toBe(1);
  });

  it("响应 operation_id 不匹配时拒绝接受结果", async () => {
    server.use(http.get("/api/v2/config/operations/operation-3", () => HttpResponse.json({
      operation_id: "different-operation",
      status: { state: "rejected", error: "APPLY_FAILED" },
    })));

    await expect(settleOperation(
      { operation_id: "operation-3", status: { state: "preparing" } },
      { maxPolls: 1, pollIntervalMs: 0 },
    )).rejects.toMatchObject({ code: "INVALID_RESPONSE" });
  });
});

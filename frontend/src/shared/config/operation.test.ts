import { http, HttpResponse } from "msw";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { server } from "@/mocks/server";
import { setMockAuthenticated } from "@/mocks/handlers";
import type { ApplyRequest, ConfigState } from "./api";
import { applyAndSettle, createOperationId, getOperationMessage, operationError, PendingOperationError, settleOperation, resumeOperation, restoreFilesAndSettle, retryPersistenceAndSettle } from "./operation";
import { QueryClient } from "@tanstack/react-query";
import { assertNoPendingOperation, pendingOperationKey, trackConfigOperation } from "./pending-operation";
import * as uuid from "@/shared/uuid";
import { ApiError, getSafeErrorMessage } from "@/shared/api/errors";

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
    changes: [{ module: "logs", change: { enable: true, level: "info", path: "logs/fluxdns.log" } }],
    confirmations: [],
  };
}

describe("配置操作回读", () => {
  beforeEach(() => setMockAuthenticated(true));

  it("操作 ID 复用共享 UUID v4 生成器，每次调用仅生成一次", () => {
    const id = "00000000-0000-4000-8000-000000000000";
    const generate = vi.spyOn(uuid, "generateUuidV4").mockReturnValue(id);
    expect(createOperationId()).toBe(id);
    expect(generate).toHaveBeenCalledExactlyOnceWith();
  });

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
    )).rejects.toMatchObject({ code: "CONFIG_OPERATION_PENDING", operationId: "operation-3", cause: { code: "INVALID_RESPONSE" } });
  });

  it("短暂忙不终止操作查询，持续忙消耗预算并保留原操作", async () => {
    let reads = 0;
    server.use(http.get("/api/v2/config/operations/busy-operation", () => {
      reads += 1;
      if (reads <= 2) return HttpResponse.json({ code: "OPERATION_BUSY", request_id: "busy-read", message: "busy", retryable: false, field_errors: [] }, { status: 409 });
      return HttpResponse.json({ operation_id: "busy-operation", status: { state: "applied_synced", active_revision: "active-2", persisted_revision: "active-2" } });
    }));
    const initial = { operation_id: "busy-operation", status: { state: "applying" as const } };
    await expect(settleOperation(initial, { maxPolls: 2, pollIntervalMs: 0 })).resolves.toEqual({ kind: "pending", operation: initial });
    expect(reads).toBe(2);
    await expect(settleOperation(initial, { maxPolls: 1, pollIntervalMs: 0 })).resolves.toMatchObject({ kind: "settled" });
    expect(reads).toBe(3);
  });

  it("响应丢失且首次回读忙时仍只发送一次 apply", async () => {
    let applies = 0;
    let reads = 0;
    server.use(
      http.post("/api/v2/config/apply", () => { applies += 1; return HttpResponse.error(); }),
      http.get("/api/v2/config/operations/recover-busy", () => {
        reads += 1;
        return reads === 1
          ? HttpResponse.json({ code: "OPERATION_BUSY", request_id: "busy-read", message: "busy", retryable: false, field_errors: [] }, { status: 409 })
          : HttpResponse.json({ operation_id: "recover-busy", status: { state: "applied_synced", active_revision: "active-2", persisted_revision: "active-2" } });
      }),
    );
    await expect(applyAndSettle(applyRequest("recover-busy"), { maxPolls: 2, pollIntervalMs: 0 })).resolves.toMatchObject({ kind: "settled" });
    expect(applies).toBe(1);
    expect(reads).toBe(2);
  });
  it("恢复预算耗尽保留 ID，后续查询成功且不重放写入", async () => {
    const client = new QueryClient();
    let writes = 0;
    let available = false;
    server.use(
      http.post("/api/v2/config/apply", () => { writes += 1; return HttpResponse.error(); }),
      http.get("/api/v2/config/operations/pending-id", () => available
        ? HttpResponse.json({ operation_id: "pending-id", status: { state: "applied_synced", active_revision: "a2", persisted_revision: "a2" } })
        : HttpResponse.error()),
    );
    await expect(trackConfigOperation(client, () => applyAndSettle(applyRequest("pending-id"), { maxPolls: 2, pollIntervalMs: 0 })))
      .rejects.toMatchObject({ operationId: "pending-id" });
    expect(client.getQueryData(pendingOperationKey)).toBe("pending-id");
    expect(() => assertNoPendingOperation(client)).toThrow();
    available = true;
    await expect(resumeOperation("pending-id", { maxPolls: 2, pollIntervalMs: 0 })).resolves.toMatchObject({ kind: "settled" });
    expect(writes).toBe(1);
  });

  it.each(["apply", "restore", "retry"] as const)("%s 恢复和后续轮询共用两次预算", async (kind) => {
    let reads = 0;
    let writes = 0;
    const path = kind === "apply" ? "/api/v2/config/apply" : `/api/v2/config/files/${kind}`;
    server.use(
      http.post(path, () => { writes += 1; return HttpResponse.error(); }),
      http.get("/api/v2/config/operations/shared-budget", () => {
        reads += 1;
        return reads === 1 ? HttpResponse.error() : HttpResponse.json({ operation_id: "shared-budget", status: { state: "applying" } });
      }),
    );
    const options = { maxPolls: 2, pollIntervalMs: 0 };
    const request = { operation_id: "shared-budget", observed_file_revision: "file-1" };
    const result = kind === "apply" ? applyAndSettle(applyRequest(request.operation_id), options)
      : kind === "restore" ? restoreFilesAndSettle(request, options) : retryPersistenceAndSettle(request, options);
    await expect(result).resolves.toMatchObject({ kind: "pending", operation: { operation_id: request.operation_id } });
    expect(reads).toBe(2);
    expect(writes).toBe(1);
  });

  it("拒绝与部分成功保留诊断，unknown 永不宣称拒绝或成功", () => {
    const diagnostic = { stage: "persist", code: "FILE_WRITE_FAILED", message: "受管文件无法写入", resource: "config" };
    const rejected = { operation_id: "rejected-id", status: { state: "rejected" as const, error: "APPLY_FAILED" as const }, diagnostic };
    expect(getSafeErrorMessage(operationError(rejected))).toMatch(/运行状态保持不变.*persist.*FILE_WRITE_FAILED.*rejected-id/);
    const partial = { operation_id: "partial-id", status: { state: "applied_unpersisted" as const, active_revision: "a2", persisted_revision: null, error: "PERSISTENCE_FAILED" as const }, diagnostic };
    expect(getOperationMessage(partial)).toMatch(/运行配置已生效.*文件尚未同步.*FILE_WRITE_FAILED.*partial-id/);
    expect(getOperationMessage({ operation_id: "unknown-id", status: { state: "unknown" } })).toMatch(/尚未确认.*unknown-id/);
    expect(getOperationMessage({ operation_id: "unknown-id", status: { state: "unknown" } })).not.toMatch(/保持不变|已应用并同步/);
  });

  it("异步 apply 已受理后取消轮询仍保留未决 ID，而不是声称已拒绝", async () => {
    const controller = new AbortController();
    server.use(http.post("/api/v2/config/apply", () => { controller.abort(); return HttpResponse.json({ operation_id: "cancelled-id", status: { state: "applying" } }); }));
    const client = new QueryClient();
    const error = await trackConfigOperation(client, () => applyAndSettle(applyRequest("cancelled-id"), { signal: controller.signal, maxPolls: 1 })).catch((value: unknown) => value);
    expect(error).toBeInstanceOf(PendingOperationError);
    expect(getSafeErrorMessage(error)).toMatch(/尚未确认.*cancelled-id/);
    expect(getSafeErrorMessage(error)).not.toContain("运行状态保持不变");
    expect(client.getQueryData(pendingOperationKey)).toBe("cancelled-id");
  });

  it("未决查询保留安全请求诊断与两个 ID，不回显底层网络凭据", () => {
    const error = new PendingOperationError("pending-safe", new ApiError({ code: "FORBIDDEN", message: "https://user:secret@example.com", kind: "http", requestId: "read-request", diagnostic: { stage: "read", code: "READ_DENIED", message: "无法读取操作记录" } }));
    expect(getSafeErrorMessage(error)).toMatch(/尚未确认.*READ_DENIED.*read-request.*pending-safe/);
    expect(getSafeErrorMessage(error)).not.toContain("secret");
  });

  it("恢复查询耗尽预算仍保留最后一次读取失败的诊断与请求 ID", async () => {
    server.use(http.get("/api/v2/config/operations/budget-exhausted", () => HttpResponse.json({
      code: "SERVICE_UNAVAILABLE", request_id: "last-read-request", message: "读取服务暂不可用", retryable: true, field_errors: [],
      diagnostic: { stage: "configuration", code: "CONFIGURATION_UNAVAILABLE", message: "无法读取配置快照" },
    }, { status: 503 })));
    const error = await resumeOperation("budget-exhausted", { maxPolls: 1, pollIntervalMs: 0 }).catch((value: unknown) => value);
    expect(error).toBeInstanceOf(PendingOperationError);
    expect(getSafeErrorMessage(error)).toMatch(/尚未确认.*CONFIGURATION_UNAVAILABLE.*last-read-request.*budget-exhausted/);
  });
});

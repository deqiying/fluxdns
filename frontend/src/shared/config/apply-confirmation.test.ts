import { QueryClient } from "@tanstack/react-query";
import { http, HttpResponse } from "msw";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { setMockAuthenticated } from "@/mocks/handlers";
import { server } from "@/mocks/server";
import type { ApplyRequest, ValidationResult } from "./api";
import { applyWithConfirmation } from "./operation";
import { pendingOperationKey, trackConfigOperation } from "./pending-operation";

const request: ApplyRequest = {
  operation_id: "save-1",
  changes: [{ module: "logs", change: { enable: true, level: "info", path: "fluxdns.log" } }],
};
const success = () => HttpResponse.json({ operation_id: request.operation_id, status: { state: "applied_synced", active_revision: "active-2", persisted_revision: "active-2" } });
const reject = (code: string) => HttpResponse.json({ code, request_id: "confirmation-check", message: code, retryable: false, field_errors: [] }, { status: 409 });
const validation: ValidationResult = { observed_file_revision: "files-2", required_confirmations: ["retention_shortening"], affected_names: [] };

beforeEach(() => setMockAuthenticated(true));

describe("单次保存和受理前风险确认", () => {
  it("普通保存只 apply 一次，不 validate、不发送版本或票据", async () => {
    const bodies: unknown[] = [];
    let validates = 0;
    server.use(
      http.post("/api/v2/config/modules/logs/apply", async ({ request: incoming }) => { bodies.push(await incoming.json()); return success(); }),
      http.post("/api/v2/config/modules/logs/validate", () => { validates += 1; return HttpResponse.error(); }),
    );
    const confirm = vi.fn();
    await expect(applyWithConfirmation(request, confirm, { module: "logs" })).resolves.toMatchObject({ kind: "settled" });
    expect(bodies).toEqual([request]);
    expect(validates).toBe(0);
    expect(confirm).not.toHaveBeenCalled();
  });

  it.each(["CONFIRMATION_REQUIRED", "EXTERNAL_CHANGES_REQUIRE_CONFIRMATION"])("%s 后才读取影响，并以同一 ID 确认一次", async (code) => {
    const bodies: ApplyRequest[] = [];
    const preview = { ...validation, required_confirmations: code === "CONFIRMATION_REQUIRED" ? ["retention_shortening" as const] : ["discard_external_changes" as const] };
    let validates = 0;
    server.use(
      http.post("/api/v2/config/modules/logs/apply", async ({ request: incoming }) => { bodies.push(await incoming.json() as ApplyRequest); return bodies.length === 1 ? reject(code) : success(); }),
      http.post("/api/v2/config/modules/logs/validate", async ({ request: incoming }) => {
        validates += 1;
        expect(bodies).toHaveLength(1);
        expect(await incoming.json()).toEqual({ changes: request.changes });
        return HttpResponse.json(preview);
      }),
    );
    const confirm = vi.fn(async () => true);
    await expect(applyWithConfirmation(request, confirm, { module: "logs" })).resolves.toMatchObject({ kind: "settled" });
    expect(bodies).toEqual([request, {
      ...request,
      confirmations: preview.required_confirmations,
      ...(code === "EXTERNAL_CHANGES_REQUIRE_CONFIRMATION" ? { observed_file_revision: "files-2" } : {}),
    }]);
    expect(validates).toBe(1);
    expect(confirm).toHaveBeenCalledExactlyOnceWith(preview);
  });

  it("取消确认不再次 apply，也不标记操作 pending", async () => {
    let applies = 0;
    const client = new QueryClient();
    server.use(
      http.post("/api/v2/config/apply", () => { applies += 1; return reject("CONFIRMATION_REQUIRED"); }),
      http.post("/api/v2/config/validate", () => HttpResponse.json(validation)),
    );
    await expect(trackConfigOperation(client, () => applyWithConfirmation(request, async () => false))).resolves.toBeNull();
    expect(applies).toBe(1);
    expect(client.getQueryData(pendingOperationKey)).toBeUndefined();
  });

  it.each(["FILE_REVISION_CONFLICT", "CONFIRMATION_REQUIRED", "EXTERNAL_CHANGES_REQUIRE_CONFIRMATION"])("第二次提交返回 %s 时停止，不自动覆盖或循环确认", async (code) => {
    let applies = 0;
    let validates = 0;
    const confirm = vi.fn(async () => true);
    const client = new QueryClient();
    server.use(
      http.post("/api/v2/config/apply", () => { applies += 1; return reject(applies === 1 ? "CONFIRMATION_REQUIRED" : code); }),
      http.post("/api/v2/config/validate", () => { validates += 1; return HttpResponse.json(validation); }),
    );
    await expect(trackConfigOperation(client, () => applyWithConfirmation(request, confirm))).rejects.toMatchObject({ code });
    expect(applies).toBe(2);
    expect(validates).toBe(1);
    expect(confirm).toHaveBeenCalledTimes(1);
    expect(client.getQueryData(pendingOperationKey)).toBeUndefined();
  });

  it("外部采用从首次 apply 绑定旧 diff 文件版本；validate 不能偷偷换成新版本", async () => {
    const bodies: ApplyRequest[] = [];
    const confirm = vi.fn(async () => true);
    const adoption: ApplyRequest = { ...request, confirmations: ["discard_external_changes"], observed_file_revision: "files-1" };
    server.use(
      http.post("/api/v2/config/apply", async ({ request: incoming }) => { bodies.push(await incoming.json() as ApplyRequest); return reject("CONFIRMATION_REQUIRED"); }),
      http.post("/api/v2/config/validate", () => HttpResponse.json(validation)),
    );
    await expect(applyWithConfirmation(adoption, confirm)).rejects.toMatchObject({ code: "FILE_REVISION_CONFLICT" });
    expect(bodies).toEqual([adoption]);
    expect(confirm).not.toHaveBeenCalled();
  });

  it("外部采用补充危险确认时保留首次 diff 版本和覆盖确认", async () => {
    const bodies: ApplyRequest[] = [];
    const adoption: ApplyRequest = { ...request, confirmations: ["discard_external_changes"], observed_file_revision: "files-2" };
    server.use(
      http.post("/api/v2/config/apply", async ({ request: incoming }) => { bodies.push(await incoming.json() as ApplyRequest); return bodies.length === 1 ? reject("CONFIRMATION_REQUIRED") : success(); }),
      http.post("/api/v2/config/validate", () => HttpResponse.json(validation)),
    );
    await expect(applyWithConfirmation(adoption, async () => true)).resolves.toMatchObject({ kind: "settled" });
    expect(bodies).toEqual([adoption, { ...adoption, confirmations: ["discard_external_changes", "retention_shortening"] }]);
  });

  it("网络结果不确定时只查原 operation，不 validate 或重放", async () => {
    let applies = 0;
    let validates = 0;
    let reads = 0;
    server.use(
      http.post("/api/v2/config/apply", () => { applies += 1; return HttpResponse.error(); }),
      http.post("/api/v2/config/validate", () => { validates += 1; return HttpResponse.json(validation); }),
      http.get("/api/v2/config/operations/save-1", () => { reads += 1; return success(); }),
    );
    const confirm = vi.fn();
    await expect(applyWithConfirmation(request, confirm, { pollIntervalMs: 0 })).resolves.toMatchObject({ kind: "settled" });
    expect([applies, validates, reads]).toEqual([1, 0, 1]);
    expect(confirm).not.toHaveBeenCalled();
  });
});

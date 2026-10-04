import { describe, expect, it } from "vitest";
import { ApiError } from "@/shared/api/errors";
import { createAppQueryClient } from "@/app/query-client";
import { configReadRetryDelay, retryConfigRead } from "./read-retry";

const busy = new ApiError({ code: "OPERATION_BUSY", kind: "http", status: 409, message: "busy", retryable: false });

describe("配置只读恢复", () => {
  it("短暂忙可恢复，最多重试三次且不重试取消或认证错误", () => {
    expect(retryConfigRead(0, busy)).toBe(true);
    expect(retryConfigRead(2, busy)).toBe(true);
    expect(retryConfigRead(3, busy)).toBe(false);
    for (const status of [401, 403]) expect(retryConfigRead(0, new ApiError({ code: "OPERATION_BUSY", kind: "http", status, message: "denied" }))).toBe(false);
    expect(retryConfigRead(0, new ApiError({ code: "OPERATION_BUSY", kind: "cancelled", message: "cancelled" }))).toBe(false);
    expect(retryConfigRead(0, new ApiError({ code: "VALIDATION_FAILED", kind: "http", message: "invalid" }))).toBe(false);
    expect(configReadRetryDelay(0, busy)).toBeGreaterThanOrEqual(250);
    expect(configReadRetryDelay(2, busy)).toBeLessThan(1100);
  });

  it("QueryClient配置读取恢复不扩展到mutation", async () => {
    const client = createAppQueryClient();
    client.setQueryDefaults(["config-v2"], { retry: retryConfigRead, retryDelay: 0 });
    let reads = 0;
    await expect(client.fetchQuery({ queryKey: ["config-v2", "state"], queryFn: async () => { if (++reads < 3) throw busy; return "synced"; } })).resolves.toBe("synced");
    expect(reads).toBe(3);
    expect(client.getDefaultOptions().mutations?.retry).toBe(false);
    client.clear();
  });
});

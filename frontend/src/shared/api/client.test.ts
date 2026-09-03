import { http, HttpResponse, delay } from "msw";
import { describe, expect, it, vi } from "vitest";
import { server } from "@/mocks/server";
import { ApiError, getSafeErrorMessage } from "./errors";
import { apiRequest, createSearchParams, onUnauthorized } from "./client";

describe("apiRequest", () => {
  it("固定使用同源 Cookie 和 JSON", async () => {
    server.use(
      http.get("/api/v1/probe", ({ request }) =>
        HttpResponse.json({ credentials: request.credentials, accept: request.headers.get("accept") }),
      ),
    );

    await expect(apiRequest<{ credentials: string; accept: string }>("/probe")).resolves.toEqual({
      credentials: "same-origin",
      accept: "application/json",
    });
  });

  it.each([
    [403, "FORBIDDEN"],
    [429, "RATE_LIMITED"],
    [503, "SERVICE_UNAVAILABLE"],
  ])("保留 HTTP %s 的稳定错误分类", async (status, code) => {
    server.use(
      http.get("/api/v1/failure", () =>
        HttpResponse.json({ code, message: "server detail", request_id: `req-${status}`, retryable: status >= 429 }, { status }),
      ),
    );

    await expect(apiRequest("/failure")).rejects.toMatchObject({ status, code, requestId: `req-${status}` });
  });

  it("拒绝将 HTML 错误页当作 API 成功", async () => {
    server.use(http.get("/api/v1/html", () => new HttpResponse("<html>fallback</html>", { headers: { "Content-Type": "text/html" } })));
    await expect(apiRequest("/html")).rejects.toMatchObject({ code: "INVALID_RESPONSE", kind: "invalid-response" });
  });

  it("区分超时和调用方取消", async () => {
    server.use(http.get("/api/v1/slow", async () => { await delay("infinite"); return HttpResponse.json({}); }));
    await expect(apiRequest("/slow", { timeoutMs: 5 })).rejects.toMatchObject({ code: "REQUEST_TIMEOUT", kind: "timeout" });

    const controller = new AbortController();
    const request = apiRequest("/slow", { signal: controller.signal, timeoutMs: 1_000 });
    controller.abort();
    await expect(request).rejects.toMatchObject({ code: "REQUEST_CANCELLED", kind: "cancelled" });
  });

  it("只对普通接口的 401 广播 session 过期", async () => {
    const listener = vi.fn();
    const unsubscribe = onUnauthorized(listener);
    server.use(http.get("/api/v1/private", () => HttpResponse.json({ code: "AUTH_REQUIRED", message: "required", request_id: "req-401", retryable: false }, { status: 401 })));

    await expect(apiRequest("/private")).rejects.toBeInstanceOf(ApiError);
    expect(listener).toHaveBeenCalledOnce();

    listener.mockClear();
    await expect(apiRequest("/private", { handleUnauthorized: false })).rejects.toBeInstanceOf(ApiError);
    expect(listener).not.toHaveBeenCalled();
    unsubscribe();
  });

  it("未知后端 message 不会直接进入用户文案", () => {
    const error = new ApiError({ code: "UNKNOWN_INTERNAL", message: "sensitive backend detail", kind: "http", status: 500 });
    expect(getSafeErrorMessage(error)).toBe("管理服务暂时不可用，请稍后重试。");
    expect(getSafeErrorMessage(error)).not.toContain("sensitive backend detail");
  });
});

describe("createSearchParams", () => {
  it("忽略 undefined 并稳定保留服务端参数", () => {
    expect(createSearchParams({ page: 2, transport: undefined, order: "desc" })).toBe("page=2&order=desc");
  });
});

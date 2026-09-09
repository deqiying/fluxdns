import { http, HttpResponse } from "msw";
import { expect, it, vi } from "vitest";
import { sessionFixture } from "@/mocks/fixtures";
import { server } from "@/mocks/server";
import { acceptAuthSession, apiRequest, clearAccessSession, onUnauthorized } from "./client";

function authentication(token = "A".repeat(43)) {
  return { session: sessionFixture, access_token: token, token_type: "Bearer" as const, access_expires_at_ms: Date.now() + 300_000 };
}

function deferred() {
  let resolve!: () => void;
  const promise = new Promise<void>((done) => { resolve = done; });
  return { promise, resolve };
}

it("并发业务请求共享一次刷新，各自用 Bearer 且不携带 Cookie", async () => {
  const gate = deferred();
  let refreshes = 0;
  server.use(
    http.post("/api/v2/auth/refresh", async ({ request }) => {
      refreshes++;
      expect(request.credentials).toBe("same-origin");
      expect(request.headers.has("authorization")).toBe(false);
      await gate.promise;
      return HttpResponse.json(authentication());
    }),
    http.get("/api/v2/probe", ({ request }) => HttpResponse.json({ auth: request.headers.get("authorization"), credentials: request.credentials })),
  );
  const first = apiRequest("/probe");
  const second = apiRequest("/probe");
  await vi.waitFor(() => expect(refreshes).toBe(1));
  gate.resolve();
  for (const result of await Promise.all([first, second])) {
    expect(result).toEqual({ auth: `Bearer ${"A".repeat(43)}`, credentials: "omit" });
  }
  expect(refreshes).toBe(1);
});

it("单个等待方取消不取消共享刷新，登出则拒绝迟到结果恢复登录", async () => {
  const gate = deferred();
  let refreshes = 0;
  let requests = 0;
  server.use(
    http.post("/api/v2/auth/refresh", async () => { refreshes++; await gate.promise; return HttpResponse.json(authentication()); }),
    http.get("/api/v2/probe", () => { requests++; return HttpResponse.json({ ok: true }); }),
  );
  const controller = new AbortController();
  const cancelled = expect(apiRequest("/probe", { signal: controller.signal })).rejects.toMatchObject({ kind: "cancelled" });
  const surviving = apiRequest("/probe");
  await vi.waitFor(() => expect(refreshes).toBe(1));
  controller.abort();
  await cancelled;
  gate.resolve();
  await expect(surviving).resolves.toEqual({ ok: true });
  expect(requests).toBe(1);

  clearAccessSession(true);
  const secondGate = deferred();
  server.use(http.post("/api/v2/auth/refresh", async () => { refreshes++; await secondGate.promise; return HttpResponse.json(authentication()); }));
  const stale = expect(apiRequest("/probe")).rejects.toMatchObject({ kind: "cancelled" });
  await vi.waitFor(() => expect(refreshes).toBe(2));
  clearAccessSession();
  secondGate.resolve();
  await stale;
  await expect(apiRequest("/probe")).rejects.toMatchObject({ status: 401 });
  expect(requests).toBe(1);
});

it("旧会话迟到的 401 不清除后来登录的新 Bearer", async () => {
  acceptAuthSession(authentication());
  const gate = deferred();
  let started = false;
  const listener = vi.fn();
  const unsubscribe = onUnauthorized(listener);
  server.use(
    http.get("/api/v2/old", async () => { started = true; await gate.promise; return HttpResponse.json({}, { status: 401 }); }),
    http.get("/api/v2/probe", ({ request }) => HttpResponse.json({ auth: request.headers.get("authorization") })),
  );
  try {
    const old = expect(apiRequest("/old")).rejects.toMatchObject({ status: 401 });
    await vi.waitFor(() => expect(started).toBe(true));
    acceptAuthSession(authentication("B".repeat(43)));
    gate.resolve();
    await old;
    expect(listener).not.toHaveBeenCalled();
    await expect(apiRequest("/probe")).resolves.toEqual({ auth: `Bearer ${"B".repeat(43)}` });
  } finally { unsubscribe(); }
});

it("已取消请求不启动刷新或业务请求", async () => {
  const fetchSpy = vi.spyOn(globalThis, "fetch");
  try {
    const controller = new AbortController();
    controller.abort();
    await expect(apiRequest("/probe", { signal: controller.signal })).rejects.toMatchObject({ kind: "cancelled" });
    expect(fetchSpy).not.toHaveBeenCalled();
  } finally { fetchSpy.mockRestore(); }
});

it("业务 deadline 包含刷新等待，超时不终止其他等待方", async () => {
  const gate = deferred();
  let started = false;
  let requests = 0;
  server.use(
    http.post("/api/v2/auth/refresh", async () => { started = true; await gate.promise; return HttpResponse.json(authentication()); }),
    http.get("/api/v2/probe", () => { requests++; return HttpResponse.json({ ok: true }); }),
  );
  const timedOut = expect(apiRequest("/probe", { timeoutMs: 20 })).rejects.toMatchObject({ kind: "timeout" });
  const surviving = apiRequest("/probe");
  await vi.waitFor(() => expect(started).toBe(true));
  await timedOut;
  gate.resolve();
  await expect(surviving).resolves.toEqual({ ok: true });
  expect(requests).toBe(1);
});

it.each([401, 500])("业务写请求返回 %s 后不刷新或自动重放", async (status) => {
  acceptAuthSession(authentication());
  let writes = 0;
  let refreshes = 0;
  server.use(
    http.post("/api/v2/write", () => { writes++; return HttpResponse.json({}, { status }); }),
    http.post("/api/v2/auth/refresh", () => { refreshes++; return HttpResponse.json(authentication()); }),
  );
  await expect(apiRequest("/write", { method: "POST", body: { operation_id: "test-operation" } })).rejects.toMatchObject({ status });
  expect(writes).toBe(1);
  expect(refreshes).toBe(0);
});

it("认证专用响应只投影白名单 session 到缓存，拒绝非法访问凭据", () => {
  const value = authentication();
  const projected = acceptAuthSession({ ...value, session: { ...sessionFixture, token: "not-a-session-field" } } as typeof value);
  expect(projected).toEqual(sessionFixture);
  expect(JSON.stringify(projected)).not.toContain(value.access_token);
  expect(() => acceptAuthSession({ ...value, access_token: "invalid" })).toThrow();
  expect(window.localStorage.length).toBe(0);
  expect(window.sessionStorage.length).toBe(0);
});

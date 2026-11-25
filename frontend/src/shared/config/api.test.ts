import { http, HttpResponse } from "msw";
import { beforeEach, expect, it, vi } from "vitest";
import { server } from "@/mocks/server";
import { setMockAuthenticated } from "@/mocks/handlers";
import { configStateFixture } from "@/mocks/fixtures";
import { persistConfig, reloadConfig, saveConfig, type SaveRequest } from "./api";

const body: SaveRequest = { changes: [{ module: "logs", change: { enable: false, level: "warn", path: "logs/fluxdns.log" } }] };

beforeEach(() => setMockAuthenticated(true));

it("保存以默认开关发送 changes 并同步返回最终状态", async () => {
  const result = { state: configStateFixture, persisted: true };
  server.use(http.post("/api/v2/config/save", async ({ request }) => {
    expect(await request.json()).toEqual(body);
    return HttpResponse.json(result);
  }));
  const timeout = vi.spyOn(window, "setTimeout");
  try {
    await expect(saveConfig(body)).resolves.toEqual(result);
    expect(timeout).toHaveBeenCalledWith(expect.any(Function), 40_000);
  } finally {
    timeout.mockRestore();
  }
});

it.each([["reload", reloadConfig, 40_000], ["persist", persistConfig, 10_000]] as const)("%s 不发送请求体", async (path, call, timeoutMs) => {
  const result = { state: configStateFixture };
  server.use(http.post(`/api/v2/config/${path}`, async ({ request }) => {
    expect(request.headers.get("Content-Type")).toBeNull();
    expect(await request.text()).toBe("");
    return HttpResponse.json(result);
  }));
  const timeout = vi.spyOn(window, "setTimeout");
  try {
    await expect(call()).resolves.toEqual(result);
    expect(timeout).toHaveBeenCalledWith(expect.any(Function), timeoutMs);
  } finally {
    timeout.mockRestore();
  }
});

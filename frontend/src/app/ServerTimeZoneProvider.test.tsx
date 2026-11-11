import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { http, HttpResponse } from "msw";
import { describe, expect, it } from "vitest";
import { processMetricsFixture, sessionFixture } from "@/mocks/fixtures";
import { server } from "@/mocks/server";
import { acceptAuthSession } from "@/shared/api/client";
import { useServerTimeZone } from "@/shared/time/TimeZoneContext";
import { processMetricsKey } from "@/modules/system/api";
import { ServerTimeZoneProvider } from "./ServerTimeZoneProvider";

function TimeZoneConsumer() {
  const { timeZone } = useServerTimeZone();
  return <><output aria-label="当前时区">{timeZone}</output><input aria-label="页面草稿" /></>;
}

function renderProvider() {
  acceptAuthSession({ session: sessionFixture, access_token: "A".repeat(43), token_type: "Bearer", access_expires_at_ms: Date.now() + 300_000 });
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  render(<QueryClientProvider client={client}><ServerTimeZoneProvider><TimeZoneConsumer /></ServerTimeZoneProvider></QueryClientProvider>);
  return client;
}

describe("ServerTimeZoneProvider", () => {
  it("元数据尚未返回时不初始化页面，读取成功后提供服务端时区", async () => {
    let release!: () => void;
    const ready = new Promise<void>((resolve) => { release = resolve; });
    server.use(http.get("/api/v2/system/runtime", async () => {
      await ready;
      return HttpResponse.json({ ...processMetricsFixture, timezone: "Asia/Shanghai" });
    }));
    renderProvider();
    expect(screen.getByRole("status", { name: "正在读取服务端时区" })).toBeInTheDocument();
    expect(screen.queryByLabelText("当前时区")).not.toBeInTheDocument();
    release();
    expect(await screen.findByLabelText("当前时区")).toHaveTextContent("Asia/Shanghai");
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  });

  it.each([
    [{ ...processMetricsFixture, timezone: "UTC", timezone_fallback_reason: "invalid_tz" }, "invalid_tz"],
    [{ ...processMetricsFixture, timezone: "Not/AZone" }, "unsupported_timezone"],
    [{ ...processMetricsFixture, timezone: undefined }, "missing_timezone"],
  ])("服务端降级或浏览器不支持时显式提示 UTC", async (data, reason) => {
    server.use(http.get("/api/v2/system/runtime", () => HttpResponse.json(data)));
    renderProvider();
    expect(await screen.findByLabelText("当前时区")).toHaveTextContent("UTC");
    expect(screen.getByRole("alert")).toHaveTextContent(`当前使用 UTC（${reason}）`);
  });

  it("读取失败后可重试恢复，不把失败当作正常 UTC 配置", async () => {
    const user = userEvent.setup();
    server.use(http.get("/api/v2/system/runtime", () => new HttpResponse(null, { status: 503 })));
    renderProvider();
    expect(await screen.findByRole("alert")).toHaveTextContent("timezone_request_failed");
    expect(screen.getByLabelText("当前时区")).toHaveTextContent("UTC");
    const draft = screen.getByLabelText("页面草稿");
    await user.type(draft, "保留未保存配置");
    let release!: () => void;
    const ready = new Promise<void>((resolve) => { release = resolve; });
    server.use(http.get("/api/v2/system/runtime", async () => {
      await ready;
      return HttpResponse.json({ ...processMetricsFixture, timezone: "Asia/Shanghai" });
    }));
    try {
      await user.click(screen.getByRole("button", { name: "重试时区读取" }));
      expect(screen.getByLabelText("页面草稿")).toBe(draft);
      expect(draft).toHaveValue("保留未保存配置");
    } finally { release(); }
    await waitFor(() => expect(screen.getByLabelText("当前时区")).toHaveTextContent("Asia/Shanghai"));
    expect(screen.getByLabelText("页面草稿")).toBe(draft);
    expect(draft).toHaveValue("保留未保存配置");
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  });

  it("共享 runtime 缓存更新时同步时区，但后台失败保留最后一次有效元数据", async () => {
    server.use(http.get("/api/v2/system/runtime", () => HttpResponse.json({ ...processMetricsFixture, timezone: "Asia/Shanghai" })));
    const client = renderProvider();
    expect(await screen.findByLabelText("当前时区")).toHaveTextContent("Asia/Shanghai");
    server.use(http.get("/api/v2/system/runtime", () => new HttpResponse(null, { status: 503 })));
    await act(async () => { await client.refetchQueries({ queryKey: processMetricsKey }); });
    expect(screen.getByLabelText("当前时区")).toHaveTextContent("Asia/Shanghai");
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    act(() => client.setQueryData(processMetricsKey, { ...processMetricsFixture, timezone: "UTC" }));
    await waitFor(() => expect(screen.getByLabelText("当前时区")).toHaveTextContent("UTC"));
  });
});

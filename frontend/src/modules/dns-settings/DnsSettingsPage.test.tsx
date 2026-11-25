import { QueryClient } from "@tanstack/react-query";
import { act, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { ConfigProvider } from "antd";
import { http, HttpResponse } from "msw";
import { describe, expect, it } from "vitest";
import { App } from "@/app/App";
import { AppProviders } from "@/app/providers";
import { setMockAuthenticated } from "@/mocks/handlers";
import { configStateFixture, dnsConfigReadFixture } from "@/mocks/fixtures";
import { server } from "@/mocks/server";
import { configKeys } from "@/shared/config/query-keys";
import type { SaveRequest } from "@/shared/config/api";

function mount() {
  setMockAuthenticated(true);
  window.history.replaceState({}, "", "/dns-settings");
  const client = new QueryClient({ defaultOptions: { queries: { retry: false }, mutations: { retry: false } } });
  render(<ConfigProvider theme={{ token: { motion: false } }}><AppProviders queryClient={client}><App /></AppProviders></ConfigProvider>);
  return client;
}

const validationFailure = () => HttpResponse.json({
  code: "VALIDATION_FAILED", message: "request could not be completed", request_id: "dns-validation", retryable: false, field_errors: [],
}, { status: 422 });

describe("DNS 配置读取恢复与草稿", () => {
  it("后台刷新提示摘要变化，不覆盖草稿或提交版本条件", async () => {
    const user = userEvent.setup();
    const client = mount();
    await user.click(await screen.findByRole("button", { name: "编辑 DNS" }, { timeout: 10_000 }));
    const dialog = await screen.findByRole("dialog", { name: "编辑 DNS 配置" });
    const ttl = within(dialog).getByLabelText("失败 TTL");
    await user.clear(ttl);
    await user.type(ttl, "17");
    const updated = structuredClone(dnsConfigReadFixture);
    updated.state.config_digest = "b".repeat(64);
    updated.values[0].value.cache.failure_ttl = "9000000000ns";
    let submitted: SaveRequest | undefined;
    server.use(
      http.get("/api/v2/config/modules/dns", () => HttpResponse.json(updated)),
      http.post("/api/v2/config/save", async ({ request }) => {
        submitted = await request.json() as SaveRequest;
        return validationFailure();
      }),
    );
    await act(async () => { await client.invalidateQueries({ queryKey: configKeys.moduleRoot("dns") }); });
    expect(await within(dialog).findByText("配置已更新，保存时将以当前内容替换本条目")).toBeInTheDocument();
    expect(ttl).toHaveValue("17");
    await user.click(within(dialog).getByRole("button", { name: "保存" }));
    expect(await within(dialog).findByText(/dns-validation/)).toBeInTheDocument();
    expect(Object.keys(submitted!)).toEqual(["changes"]);
    expect(submitted?.changes[0]).toMatchObject({ module: "dns", change: { cache: { failure_ttl: "17s" } } });
    expect(ttl).toHaveValue("17");
  });

  it("刷新失败仍可保存已打开的草稿", async () => {
    const user = userEvent.setup();
    const client = mount();
    await user.click(await screen.findByRole("button", { name: "编辑 DNS" }, { timeout: 10_000 }));
    const dialog = await screen.findByRole("dialog", { name: "编辑 DNS 配置" });
    const ttl = within(dialog).getByLabelText("失败 TTL");
    await user.clear(ttl);
    await user.type(ttl, "19");
    const busy = () => HttpResponse.json({ code: "OPERATION_BUSY", message: "busy", request_id: "read-busy", retryable: false, field_errors: [] }, { status: 409 });
    const writes: SaveRequest[] = [];
    server.use(http.get("/api/v2/config/modules/dns", busy), http.get("/api/v2/config/state", busy),
      http.post("/api/v2/config/save", async ({ request }) => {
        writes.push(await request.json() as SaveRequest);
        return HttpResponse.json({ state: configStateFixture, persisted: true });
      }));
    await act(async () => { await client.invalidateQueries({ queryKey: configKeys.all }); });
    expect(await screen.findByText("刷新失败，当前显示上次读取的内容")).toBeInTheDocument();
    expect(ttl).toHaveValue("19");
    await user.click(within(dialog).getByRole("button", { name: "保存" }));
    expect(await screen.findByText("配置已应用并同步到文件")).toBeInTheDocument();
    expect(writes[0].changes[0]).toMatchObject({ module: "dns", change: { cache: { failure_ttl: "19s" } } });
  });

  it("统计保留缩短在保存响应要求确认后带开关重发同一请求", async () => {
    const user = userEvent.setup();
    const writes: SaveRequest[] = [];
    server.use(http.post("/api/v2/config/save", async ({ request }) => {
      writes.push(await request.json() as SaveRequest);
      return writes.length === 1
        ? HttpResponse.json({ code: "CONFIRMATION_REQUIRED", request_id: "retention-confirm", message: "confirm", retryable: false, field_errors: [] }, { status: 409 })
        : HttpResponse.json({ state: configStateFixture, persisted: true });
    }));
    mount();
    await user.click(await screen.findByRole("button", { name: "编辑保留策略" }));
    const dialog = await screen.findByRole("dialog", { name: "编辑数据保留" });
    const days = within(dialog).getByLabelText("R：基础保留天数");
    await user.clear(days);
    await user.type(days, "1");
    await user.click(within(dialog).getByRole("button", { name: "保存" }));
    const confirm = await screen.findByRole("button", { name: "确认缩短" });
    expect(writes).toEqual([{ changes: [{ module: "statistics", change: { retention: { days: 1, grace_days: 3, reference_size_bytes: 1_073_741_824 } } }] }]);
    await user.click(confirm);
    expect(await screen.findByText("配置已应用并同步到文件")).toBeInTheDocument();
    await waitFor(() => expect(dialog).not.toBeVisible(), { timeout: 5_000 });
    expect(writes).toEqual([writes[0], { ...writes[0], confirm_retention_shortening: true }]);
  });
});

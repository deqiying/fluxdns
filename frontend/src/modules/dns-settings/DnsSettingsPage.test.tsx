import { QueryClient } from "@tanstack/react-query";
import { act, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { http, HttpResponse } from "msw";
import { describe, expect, it } from "vitest";
import { App } from "@/app/App";
import { AppProviders } from "@/app/providers";
import { setMockAuthenticated } from "@/mocks/handlers";
import { dnsConfigReadFixture } from "@/mocks/fixtures";
import { server } from "@/mocks/server";
import { configKeys } from "@/shared/config/query-keys";
import type { Candidate } from "@/shared/config/api";

function mount() {
  setMockAuthenticated(true);
  window.history.replaceState({}, "", "/dns-settings");
  const client = new QueryClient({ defaultOptions: { queries: { retry: false }, mutations: { retry: false } } });
  render(<AppProviders queryClient={client}><App /></AppProviders>);
  return client;
}

describe("DNS 配置读取恢复与草稿", () => {
  it("后台刷新不覆盖草稿，保存固定打开时的版本", async () => {
    const user = userEvent.setup();
    const client = mount();
    await user.click(await screen.findByRole("button", { name: "编辑 DNS" }, { timeout: 10_000 }));
    const dialog = await screen.findByRole("dialog", { name: "编辑 DNS 配置" });
    const ttl = within(dialog).getByLabelText("失败 TTL");
    await user.clear(ttl);
    await user.type(ttl, "17");
    const updated = structuredClone(dnsConfigReadFixture);
    updated.state.active_revision = "new-active";
    updated.state.observed_file_revision = "new-files";
    updated.values[0].value.cache.failure_ttl = "9000000000ns";
    let candidate: Candidate | undefined;
    server.use(
      http.get("/api/v2/config/modules/dns", () => HttpResponse.json(updated)),
      http.post("/api/v2/config/modules/dns/validate", async ({ request }) => {
        candidate = await request.json() as Candidate;
        return HttpResponse.json({ code: "VALIDATION_FAILED", message: "test rejection", request_id: "draft-check", retryable: false, field_errors: [] }, { status: 400 });
      }),
    );
    await act(async () => { await client.invalidateQueries({ queryKey: configKeys.moduleRoot("dns") }); });
    expect(ttl).toHaveValue("17");
    await user.click(within(dialog).getByRole("button", { name: "保存" }));
    await waitFor(() => expect(candidate).toBeDefined());
    expect(candidate?.expected).toEqual({ active_revision: dnsConfigReadFixture.state.active_revision, observed_file_revision: dnsConfigReadFixture.state.observed_file_revision });
    expect(candidate?.changes[0]).toMatchObject({ module: "dns", change: { cache: { failure_ttl: "17s" } } });
    expect(ttl).toHaveValue("17");
  });

  it("刷新失败保留内容和草稿，状态降级且保存禁用", async () => {
    const user = userEvent.setup();
    const client = mount();
    await user.click(await screen.findByRole("button", { name: "编辑 DNS" }, { timeout: 10_000 }));
    const dialog = await screen.findByRole("dialog", { name: "编辑 DNS 配置" });
    const ttl = within(dialog).getByLabelText("失败 TTL");
    await user.clear(ttl);
    await user.type(ttl, "19");
    const busy = () => HttpResponse.json({ code: "OPERATION_BUSY", message: "busy", request_id: "read-busy", retryable: false, field_errors: [] }, { status: 409 });
    server.use(http.get("/api/v2/config/modules/dns", busy), http.get("/api/v2/config/state", busy));
    await act(async () => { await client.invalidateQueries({ queryKey: configKeys.all }); });
    expect(await screen.findByText("刷新失败，当前显示上次读取的内容")).toBeInTheDocument();
    expect(screen.getByText("同步状态暂不可用")).toBeInTheDocument();
    expect(within(dialog).getByRole("button", { name: "保存" })).toBeDisabled();
    expect(ttl).toHaveValue("19");
  });
});

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
import type { ApplyRequest } from "@/shared/config/api";

function mount() {
  setMockAuthenticated(true);
  window.history.replaceState({}, "", "/dns-settings");
  const client = new QueryClient({ defaultOptions: { queries: { retry: false }, mutations: { retry: false } } });
  render(<AppProviders queryClient={client}><App /></AppProviders>);
  return client;
}

describe("DNS 配置读取恢复与草稿", () => {
  it("后台刷新不覆盖草稿，保存不回传打开时的全局版本", async () => {
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
    let candidate: ApplyRequest | undefined;
    server.use(
      http.get("/api/v2/config/modules/dns", () => HttpResponse.json(updated)),
      http.post("/api/v2/config/modules/dns/apply", async ({ request }) => {
        candidate = await request.json() as ApplyRequest;
        return HttpResponse.json({ code: "VALIDATION_FAILED", message: "test rejection", request_id: "draft-check", retryable: false, field_errors: [] }, { status: 400 });
      }),
    );
    await act(async () => { await client.invalidateQueries({ queryKey: configKeys.moduleRoot("dns") }); });
    expect(ttl).toHaveValue("17");
    await user.click(within(dialog).getByRole("button", { name: "保存" }));
    await waitFor(() => expect(candidate).toBeDefined());
    expect(Object.keys(candidate!).sort()).toEqual(["changes", "operation_id"]);
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
  it("保留缩短先 apply 拒绝再确认；预览和确认提交均不携带 active revision", async () => {
    const user = userEvent.setup();
    const applies: ApplyRequest[] = [];
    let validates = 0;
    server.use(
      http.post("/api/v2/retention/preview", async ({ request }) => {
        expect(Object.keys(await request.json() as object)).toEqual(["policy"]);
        return HttpResponse.json({ sampled_at_ms: 1, detail_bytes: "0", proposed_cutoff_utc_date: "2026-11-01", shortens_history: true });
      }),
      http.post("/api/v2/config/modules/statistics/apply", async ({ request }) => {
        const body = await request.json() as ApplyRequest;
        applies.push(body);
        return applies.length === 1
          ? HttpResponse.json({ code: "CONFIRMATION_REQUIRED", request_id: "retention-confirm", message: "confirm", retryable: false, field_errors: [] }, { status: 409 })
          : HttpResponse.json({ operation_id: body.operation_id, status: { state: "applied_synced", active_revision: "a2", persisted_revision: "a2" } });
      }),
      http.post("/api/v2/config/modules/statistics/validate", async ({ request }) => {
        validates += 1;
        expect(applies).toHaveLength(1);
        expect(await request.json()).toEqual({ changes: applies[0].changes });
        return HttpResponse.json({ observed_file_revision: "files-new", required_confirmations: ["retention_shortening"], affected_names: [] });
      }),
    );
    mount();
    await user.click(await screen.findByRole("button", { name: "编辑保留策略" }));
    const dialog = await screen.findByRole("dialog", { name: "编辑数据保留" });
    const days = within(dialog).getByLabelText("R：基础保留天数");
    await user.clear(days);
    await user.type(days, "1");
    await user.click(within(dialog).getByRole("button", { name: "保存" }));
    expect(await screen.findByText(/候选截止日期 2026-11-01/)).toBeInTheDocument();
    expect(applies).toHaveLength(1);
    await user.click(screen.getByRole("button", { name: "确认应用" }));
    await waitFor(() => expect(applies).toHaveLength(2));
    expect(applies[1]).toEqual({ ...applies[0], confirmations: ["retention_shortening"] });
    expect(Object.keys(applies[0]).sort()).toEqual(["changes", "operation_id"]);
    expect(validates).toBe(1);
    expect(await screen.findByText("配置已应用并同步到文件")).toBeInTheDocument();
  });

  it.each(["cancel", "conflict"] as const)("外改确认 %s 保留 DNS 草稿，不循环重试", async (action) => {
    const user = userEvent.setup();
    const applies: ApplyRequest[] = [];
    let validates = 0;
    server.use(
      http.post("/api/v2/config/modules/dns/apply", async ({ request }) => {
        applies.push(await request.json() as ApplyRequest);
        return HttpResponse.json({ code: applies.length === 1 ? "EXTERNAL_CHANGES_REQUIRE_CONFIRMATION" : "FILE_REVISION_CONFLICT", request_id: "file-confirm", message: "confirm", retryable: false, field_errors: [] }, { status: 409 });
      }),
      http.post("/api/v2/config/modules/dns/validate", () => {
        validates += 1;
        return HttpResponse.json({ observed_file_revision: "files-new", required_confirmations: ["discard_external_changes"], affected_names: [] });
      }),
    );
    mount();
    await user.click(await screen.findByRole("button", { name: "编辑 DNS" }));
    const dialog = await screen.findByRole("dialog", { name: "编辑 DNS 配置" });
    const ttl = within(dialog).getByLabelText("失败 TTL");
    await user.clear(ttl);
    await user.type(ttl, "23");
    await user.click(within(dialog).getByRole("button", { name: "保存" }));
    expect(await screen.findByText("尚未采用的外部文件变化将被覆盖")).toBeInTheDocument();
    expect(applies).toHaveLength(1);
    await user.click(screen.getByRole("button", { name: action === "cancel" ? "返回编辑" : "确认应用" }));
    if (action === "conflict") {
      expect(await within(dialog).findByText(/配置文件已再次变化，请重新读取差异。.*file-confirm/)).toBeInTheDocument();
      expect(applies).toEqual([applies[0], { ...applies[0], confirmations: ["discard_external_changes"], observed_file_revision: "files-new" }]);
    } else {
      await waitFor(() => expect(within(dialog).getByRole("button", { name: "保存" })).not.toBeDisabled());
      expect(applies).toHaveLength(1);
    }
    expect(validates).toBe(1);
    expect(ttl).toHaveValue("23");
    expect(dialog).toBeInTheDocument();
    expect(screen.queryByText("配置操作结果尚未确认")).not.toBeInTheDocument();
  });

  it("保留预览失败显示安全诊断和字段错误，保留草稿且不提交 apply", async () => {
    const user = userEvent.setup();
    let applies = 0;
    server.use(
      http.post("/api/v2/retention/preview", () => HttpResponse.json({ code: "VALIDATION_FAILED", request_id: "preview-request", message: "request could not be completed", retryable: false, field_errors: [{ path: "/policy/retention/days", code: "INVALID_ARGUMENT" }], diagnostic: { stage: "preview", code: "RETENTION_INVALID", message: "保留策略无效", resource: "statistics" } }, { status: 422 })),
      http.post("/api/v2/config/modules/statistics/apply", () => { applies += 1; return HttpResponse.error(); }),
    );
    mount();
    await user.click(await screen.findByRole("button", { name: "编辑保留策略" }));
    const dialog = await screen.findByRole("dialog", { name: "编辑数据保留" });
    const days = within(dialog).getByLabelText("R：基础保留天数");
    await user.clear(days);
    await user.type(days, "2");
    await user.click(within(dialog).getByRole("button", { name: "保存" }));
    const alert = await within(dialog).findByRole("alert");
    expect(alert).toHaveTextContent("RETENTION_INVALID");
    expect(alert).toHaveTextContent("preview-request");
    expect(alert).toHaveTextContent("/policy/retention/days");
    expect(alert).not.toHaveTextContent("request could not be completed");
    expect(days).toHaveValue("2");
    expect(applies).toBe(0);
  });
});

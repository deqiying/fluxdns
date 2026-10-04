import { act, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClient } from "@tanstack/react-query";
import { describe, expect, it } from "vitest";
import { http, HttpResponse } from "msw";
import { AppProviders } from "@/app/providers";
import { App } from "@/app/App";
import { setMockAuthenticated } from "@/mocks/handlers";
import { strategiesConfigReadFixture } from "@/mocks/fixtures";
import { server } from "@/mocks/server";
import { configKeys } from "@/shared/config/query-keys";
import type { Candidate } from "@/shared/config/api";
import { pendingOperationKey } from "@/shared/config/pending-operation";

function renderStrategies() {
  window.history.replaceState({}, "", "/strategies");
  setMockAuthenticated(true);
  const client = new QueryClient({ defaultOptions: { queries: { retry: false }, mutations: { retry: false } } });
  render(<AppProviders queryClient={client}><App /></AppProviders>);
  return client;
}
describe("StrategiesPage rule cards", () => {
  it("通过局部 MSW 数据展示折叠规则摘要并展开规则字段", async () => {
    const user = userEvent.setup();
    server.use(http.get("/api/v2/config/modules/strategy", () => HttpResponse.json({
      ...strategiesConfigReadFixture,
      values: [{ module: "strategy", value: {
        ...strategiesConfigReadFixture.values[0].value,
        rules: [{ hosts: "local", edns_client_subnet: { mode: "custom", custom_ip: "192.0.2.0/24" } }],
      } }],
    })));
    renderStrategies();
    await user.click(await screen.findByLabelText("编辑策略 default", {}, { timeout: 10_000 }));
    const dialog = await screen.findByRole("dialog", { name: "编辑策略" });
    const toggle = within(dialog).getByRole("button", { name: /^1\./ });
    await user.click(toggle);
    expect(dialog.querySelector(".strategy-rule-details")).not.toBeNull();
    expect(within(dialog).getAllByLabelText("ECS 覆盖").length).toBeGreaterThan(1);
  });

  it("折叠规则重排后保存 ECS，后台刷新保留打开版本", async () => {
    const user = userEvent.setup();
    const original = { ...strategiesConfigReadFixture.values[0].value, rules: [
      { hosts: "local", edns_client_subnet: { mode: "custom", custom_ip: "2001:db8::/48" } },
      { rule_set: "domains", upstream: "default-group", edns_client_subnet: { mode: "disabled" } },
    ], cache: { enabled: true, optimistic: { enabled: false, answer_ttl: "10s", max_age: "1h" } }, ttl_override: { min: "0s" } };
    let response = { ...strategiesConfigReadFixture, values: [{ module: "strategy", value: original }] };
    let submitted: Candidate | undefined;
    server.use(
      http.get("/api/v2/config/modules/strategy", () => HttpResponse.json(response)),
      http.post("/api/v2/config/modules/strategy/validate", async ({ request }) => { submitted = await request.json() as Candidate; return HttpResponse.json({ code: "VALIDATION_FAILED", message: "test", request_id: "test", retryable: false, field_errors: [] }, { status: 422 }); }),
    );
    const client = renderStrategies();
    await user.click(await screen.findByLabelText("编辑策略 default", {}, { timeout: 10_000 }));
    const dialog = await screen.findByRole("dialog", { name: "编辑策略" });
    await user.click(within(dialog).getByLabelText("下移规则 1"));
    response = { ...response, state: { ...response.state, active_revision: "new-active" } };
    await act(async () => { await client.invalidateQueries({ queryKey: configKeys.moduleRoot("strategy") }); });
    await user.click(within(dialog).getByRole("button", { name: "保存" }));
    await waitFor(() => expect(submitted).toBeDefined());
    expect(submitted?.expected.active_revision).toBe(strategiesConfigReadFixture.state.active_revision);
    expect(submitted?.changes[0]).toMatchObject({ change: { value: { rules: [original.rules[1], original.rules[0]], cache: original.cache, ttl_override: original.ttl_override } } });
  });

  it("未展开的新规则也执行校验并在失败后展开", async () => {
    const user = userEvent.setup();
    renderStrategies();
    await user.click(await screen.findByLabelText("编辑策略 default", {}, { timeout: 10_000 }));
    const dialog = await screen.findByRole("dialog", { name: "编辑策略" });
    await user.click(within(dialog).getByRole("button", { name: "添加规则" }));
    await user.click(within(dialog).getByRole("button", { name: "保存" }));
    await waitFor(() => expect(within(dialog).getByRole("button", { name: /^3\./ })).toHaveAttribute("aria-expanded", "true"));
    expect(dialog.querySelector(".ant-form-item-has-error")).not.toBeNull();
  });
  it("仅修改名称时保留禁用 TTL 的隐藏边界", async () => {
    const user = userEvent.setup();
    const ttl = { enabled: false, min: "0s", max: "1h" };
    let submitted: Candidate | undefined;
    server.use(
      http.get("/api/v2/config/modules/strategy", () => HttpResponse.json({ ...strategiesConfigReadFixture,
        values: [{ module: "strategy", value: { ...strategiesConfigReadFixture.values[0].value, ttl_override: ttl } }],
      })),
      http.post("/api/v2/config/modules/strategy/validate", async ({ request }) => {
        submitted = await request.json() as Candidate;
        return HttpResponse.json({ code: "VALIDATION_FAILED", message: "test", request_id: "test", retryable: false, field_errors: [] }, { status: 422 });
      }),
    );
    renderStrategies();
    await user.click(await screen.findByLabelText("编辑策略 default", {}, { timeout: 10_000 }));
    const dialog = await screen.findByRole("dialog", { name: "编辑策略" });
    await user.type(within(dialog).getByLabelText("名称"), "-renamed");
    await user.click(within(dialog).getByRole("button", { name: "保存" }));
    await waitFor(() => expect(submitted?.changes[0]).toMatchObject({ change: { value: { name: "default-renamed", ttl_override: ttl } } }));
  });

  it("未决操作阻止新保存，继续查询使用原 ID", async () => {
    const user = userEvent.setup();
    let validations = 0;
    let reads = 0;
    server.use(
      http.post("/api/v2/config/modules/strategy/validate", () => { validations += 1; return HttpResponse.error(); }),
      http.get("/api/v2/config/operations/original-pending", () => {
        reads += 1;
        return HttpResponse.json({ operation_id: "original-pending", status: { state: "applied_synced", active_revision: "a2", persisted_revision: "a2" } });
      }),
    );
    const client = renderStrategies();
    await user.click(await screen.findByLabelText("编辑策略 default", {}, { timeout: 10_000 }));
    act(() => { client.setQueryData(pendingOperationKey, "original-pending"); });
    const dialog = await screen.findByRole("dialog", { name: "编辑策略" });
    await user.click(within(dialog).getByRole("button", { name: "保存" }));
    await waitFor(() => expect(within(dialog).getByText(/配置操作结果尚未确认/)).toBeInTheDocument());
    expect(validations).toBe(0);
    await user.click(screen.getByRole("button", { name: "继续查询" }));
    await waitFor(() => expect(client.getQueryData(pendingOperationKey)).toBeNull());
    expect(reads).toBe(1);
    expect(validations).toBe(0);
  });
});

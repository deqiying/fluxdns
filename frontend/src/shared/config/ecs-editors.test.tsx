import { QueryClient } from "@tanstack/react-query";
import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { http, HttpResponse } from "msw";
import { describe, expect, it } from "vitest";
import { App } from "@/app/App";
import { AppProviders } from "@/app/providers";
import { setMockAuthenticated } from "@/mocks/handlers";
import { clientsConfigReadFixture, dnsConfigReadFixture, strategiesConfigReadFixture, upstreamsConfigReadFixture } from "@/mocks/fixtures";
import { server } from "@/mocks/server";
import type { ApplyRequest, ConfigRead } from "./api";

const fallback = { mode: "client_or_custom", custom_ip: "2001:db8::/48" } as const;
const label = "请求 ECS 优先，自定义兜底";
const scenarios = [
  { module: "dns", path: "/dns-settings", edit: "编辑 DNS", dialog: "编辑 DNS 配置", fixture: dnsConfigReadFixture },
  { module: "upstreams", path: "/upstreams", edit: "编辑上游 secure-dns", dialog: "编辑上游", fixture: upstreamsConfigReadFixture },
  { module: "clients", path: "/clients", edit: "编辑客户端 desktop", dialog: /编辑客户端/, fixture: clientsConfigReadFixture },
  { module: "strategy", path: "/strategies", edit: "编辑策略 default", dialog: "编辑策略", fixture: strategiesConfigReadFixture },
  { module: "strategy", path: "/strategies", edit: "编辑策略 default", dialog: "编辑策略", fixture: strategiesConfigReadFixture, rule: true },
] as const;

describe("五层 ECS 编辑器", () => {
  it.each(scenarios)("$module $edit 请求优先模式回显、必填和切换清理", async (scenario) => {
    const user = userEvent.setup();
    const fixture: ConfigRead = structuredClone(scenario.fixture);
    const isRule = "rule" in scenario;
    for (const item of fixture.values) {
      if (item.module === "dns" || item.module === "clients") item.value.edns_client_subnet = fallback;
      if (item.module === "upstreams" && item.value.type === "doh") item.value.edns_client_subnet = fallback;
      if (item.module === "strategy") {
        item.value.rules = [{ hosts: "local", ...(isRule ? { edns_client_subnet: fallback } : {}) }];
        if (!isRule) item.value.edns_client_subnet = fallback;
      }
    }
    const requests: ApplyRequest[] = [];
    server.use(
      http.get(`/api/v2/config/modules/${scenario.module}`, () => HttpResponse.json(fixture)),
      http.post(`/api/v2/config/modules/${scenario.module}/apply`, async ({ request }) => {
        requests.push(await request.json() as ApplyRequest);
        // 保留编辑器，以同一草稿连续验证字段清理；不依赖模拟后端持久化行为。
        return HttpResponse.json({ code: "VALIDATION_FAILED", request_id: "ecs-editor", message: "test rejection", retryable: false, field_errors: [] }, { status: 422 });
      }),
    );
    setMockAuthenticated(true);
    window.history.replaceState({}, "", scenario.path);
    const client = new QueryClient({ defaultOptions: { queries: { retry: false }, mutations: { retry: false } } });
    render(<AppProviders queryClient={client}><App /></AppProviders>);
    await user.click(await screen.findByRole("button", { name: scenario.edit }, { timeout: 10_000 }));
    const dialog = await screen.findByRole("dialog", { name: scenario.dialog });
    if (isRule) await user.click(within(dialog).getByRole("button", { name: /^1\./ }));
    const scope = scenario.module === "strategy"
      ? (isRule ? dialog.querySelector(".strategy-rule-details")! : dialog.querySelectorAll(".strategy-form-section")[2]) as HTMLElement
      : dialog;
    const addressLabel = scenario.module === "clients" ? "自定义 ECS 地址" : "自定义 ECS";
    const address = within(scope).getByLabelText(addressLabel);
    expect(address).toHaveValue(fallback.custom_ip);
    expect(within(scope).getByText(label)).toBeInTheDocument();
    const save = () => user.click(within(dialog).getByRole("button", { name: "保存" }));
    const submittedEcs = () => {
      const change = requests.at(-1)!.changes[0];
      if (change.module === "dns") return change.change.edns_client_subnet;
      if (change.module === "clients" || change.module === "upstreams" || change.module === "strategy") {
        const value = change.change.value;
        if (change.module === "strategy" && isRule && "rules" in value) return value.rules[0].edns_client_subnet;
        return "edns_client_subnet" in value ? value.edns_client_subnet : undefined;
      }
      throw new Error("unexpected ECS change");
    };
    await save();
    await waitFor(() => expect(requests).toHaveLength(1));
    expect(submittedEcs()).toEqual(fallback);
    await user.clear(address);
    await save();
    await waitFor(() => expect(address).toHaveAttribute("aria-invalid", "true"));
    expect(requests).toHaveLength(1);
    await user.type(address, "192.0.2.0/24");
    await save();
    await waitFor(() => expect(requests).toHaveLength(2));
    expect(submittedEcs()).toEqual({ mode: "client_or_custom", custom_ip: "192.0.2.0/24" });
    const modeLabel = scenario.module === "strategy" ? /^ECS 覆盖/ : /^ECS/;
    const modes = [
      { label: "客户端地址", mode: "client" },
      { label: "禁用", mode: "disabled" },
      ...(scenario.module === "clients" || scenario.module === "strategy" ? [{ label: "继承", mode: "inherit" }] : []),
    ];
    for (const [index, mode] of modes.entries()) {
      await user.click(within(scope).getByRole("combobox", { name: modeLabel }));
      await user.click(await screen.findByText(mode.label, { selector: ".ant-select-item-option-content" }));
      expect(within(scope).queryByLabelText(addressLabel)).not.toBeInTheDocument();
      await save();
      await waitFor(() => expect(requests).toHaveLength(3 + index));
      expect(submittedEcs()).toEqual(mode.mode === "inherit" ? undefined : { mode: mode.mode });
    }
    await user.click(within(scope).getByRole("combobox", { name: modeLabel }));
    await user.click(await screen.findByText(label, { selector: ".ant-select-item-option-content" }));
    expect(within(scope).getByLabelText(addressLabel)).toHaveValue("192.0.2.0/24");
    await save();
    await waitFor(() => expect(requests).toHaveLength(3 + modes.length));
    expect(submittedEcs()).toEqual({ mode: "client_or_custom", custom_ip: "192.0.2.0/24" });
  });
});

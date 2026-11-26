import { QueryClient } from "@tanstack/react-query";
import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent, { PointerEventsCheckLevel } from "@testing-library/user-event";
import { ConfigProvider } from "antd";
import { http, HttpResponse } from "msw";
import { describe, expect, it } from "vitest";
import { App } from "@/app/App";
import { AppProviders } from "@/app/providers";
import { setMockAuthenticated } from "@/mocks/handlers";
import { clientsConfigReadFixture, dnsConfigReadFixture, strategiesConfigReadFixture, upstreamsConfigReadFixture } from "@/mocks/fixtures";
import { server } from "@/mocks/server";
import type { SaveRequest, ConfigRead } from "./api";

const fallback = { mode: "client_or_custom", custom_ip: "2001:db8::/48" } as const;
const label = "请求 ECS 优先，自定义兜底";
const scenarios = [
  { name: "DNS", module: "dns", path: "/dns-settings", edit: "编辑 DNS", dialog: "编辑 DNS 配置", fixture: dnsConfigReadFixture },
  { name: "上游", module: "upstreams", path: "/upstreams", edit: "编辑上游 secure-dns", dialog: "编辑上游", fixture: upstreamsConfigReadFixture },
  { name: "客户端", module: "clients", path: "/clients", edit: "编辑客户端 desktop", dialog: /编辑客户端/, fixture: clientsConfigReadFixture },
  { name: "策略级", module: "strategy", path: "/strategies", edit: "编辑策略 default", dialog: "编辑策略", fixture: strategiesConfigReadFixture },
  { name: "规则级", module: "strategy", path: "/strategies", edit: "编辑策略 default", dialog: "编辑策略", fixture: strategiesConfigReadFixture, rule: true },
] as const;

describe("五层 ECS 编辑器", () => {
  it.each(scenarios)("$name ECS 请求优先模式回显、必填和切换清理", async (scenario) => {
    // 只验证编辑行为，不在 jsdom 中为重复点击计算 CSS 命中样式。
    const user = userEvent.setup({ skipHover: true, pointerEventsCheck: PointerEventsCheckLevel.Never });
    const fixture: ConfigRead = structuredClone(scenario.fixture);
    const isRule = "rule" in scenario;
    for (const item of fixture.values) {
      if (item.module === "dns" || item.module === "clients") item.value.edns_client_subnet = fallback;
      if (item.module === "upstreams" && item.value.type === "doh") item.value.edns_client_subnet = fallback;
      if (item.module === "strategy") {
        // 此处只验证 ECS，避免渲染无关的缓存和 TTL 控件。
        delete item.value.cache;
        delete item.value.ttl_override;
        item.value.rules = [{ hosts: "local", ...(isRule ? { edns_client_subnet: fallback } : {}) }];
        if (!isRule) item.value.edns_client_subnet = fallback;
      }
    }
    const requests: SaveRequest[] = [];
    server.use(
      http.get(`/api/v2/config/modules/${scenario.module}`, () => HttpResponse.json(fixture)),
      http.post("/api/v2/config/save", async ({ request }) => {
        requests.push(await request.json() as SaveRequest);
        // 保留编辑器，以同一草稿连续验证字段清理；不依赖模拟后端持久化行为。
        return HttpResponse.json({ code: "VALIDATION_FAILED", request_id: "ecs-editor", message: "test rejection", retryable: false, field_errors: [] }, { status: 422 });
      }),
    );
    setMockAuthenticated(true);
    window.history.replaceState({}, "", scenario.path);
    const client = new QueryClient({ defaultOptions: { queries: { retry: false }, mutations: { retry: false } } });
    render(<ConfigProvider theme={{ token: { motion: false } }}><AppProviders queryClient={client}><App /></AppProviders></ConfigProvider>);
    await user.click(await screen.findByRole("button", { name: scenario.edit }, { timeout: 10_000 }));
    const dialog = await screen.findByRole("dialog", { name: scenario.dialog });
    if (isRule) await user.click(within(dialog).getByRole("button", { name: /^1\./ }));
    const scope = scenario.module === "strategy"
      ? (isRule ? dialog.querySelector(".strategy-rule-details")! : dialog.querySelectorAll(".strategy-form-section")[2]) as HTMLElement
      : dialog;
    const fields = within(scope);
    const addressLabel = scenario.module === "clients" ? "自定义 ECS 地址" : "自定义 ECS";
    const address = fields.getByLabelText(addressLabel);
    const save = within(dialog).getByRole("button", { name: "保存" });
    expect(address).toHaveValue(fallback.custom_ip);
    expect(fields.getByText(label)).toBeInTheDocument();
    const submittedEcs = () => {
      const change = requests.at(-1)!.changes[0];
      expect(change.module).toBe(scenario.module);
      if (change.module === "dns") return change.change.edns_client_subnet;
      if (change.module === "clients" || change.module === "upstreams" || change.module === "strategy") {
        const value = change.change.value;
        if (change.module === "strategy" && isRule && "rules" in value) return value.rules[0].edns_client_subnet;
        return "edns_client_subnet" in value ? value.edns_client_subnet : undefined;
      }
      throw new Error("unexpected ECS change");
    };
    const saveAndExpectEcs = async (expected: ReturnType<typeof submittedEcs>) => {
      const count = requests.length;
      await user.click(save);
      await waitFor(() => expect(requests).toHaveLength(count + 1));
      expect(submittedEcs()).toEqual(expected);
      // MSW 在返回 422 前已记录请求；等 mutation 完成后才能继续编辑同一份草稿。
      await waitFor(() => expect(save).toBeEnabled());
    };

    fireEvent.change(address, { target: { value: "" } });
    await user.click(save);
    await waitFor(() => expect(address).toHaveAttribute("aria-invalid", "true"));
    expect(requests).toHaveLength(0);

    const updated = { mode: "client_or_custom", custom_ip: "192.0.2.0/24" } as const;
    fireEvent.change(address, { target: { value: updated.custom_ip } });

    const modeLabel = scenario.module === "strategy" ? /^ECS 覆盖/ : /^ECS/;
    const modeSelect = fields.getByRole("combobox", { name: modeLabel });
    const selectMode = async (option: string) => {
      await user.click(modeSelect);
      await user.click(await screen.findByText(option, { selector: ".ant-select-item-option-content" }));
    };
    const modes = [
      { label: "客户端地址", mode: "client" },
      { label: "禁用", mode: "disabled" },
      ...(scenario.module === "clients" || scenario.module === "strategy" ? [{ label: "继承", mode: "inherit" } as const] : []),
    ] as const;
    for (const { label: option, mode } of modes) {
      await selectMode(option);
      expect(fields.queryByLabelText(addressLabel)).not.toBeInTheDocument();
      // 策略级与规则级的完整序列化组合由 strategy-form.test.ts 覆盖，UI 只需一次代表性的请求。
      if (scenario.module !== "strategy" || mode === "client") {
        await saveAndExpectEcs(mode === "inherit" ? undefined : { mode });
      }
    }
    await selectMode(label);
    expect(fields.getByLabelText(addressLabel)).toHaveValue(updated.custom_ip);
    await saveAndExpectEcs(updated);
  });
});

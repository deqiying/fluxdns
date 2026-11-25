import { act, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { ConfigProvider } from "antd";
import { QueryClient } from "@tanstack/react-query";
import { describe, expect, it } from "vitest";
import { http, HttpResponse } from "msw";
import { AppProviders } from "@/app/providers";
import { App } from "@/app/App";
import { setMockAuthenticated } from "@/mocks/handlers";
import { strategiesConfigReadFixture } from "@/mocks/fixtures";
import { server } from "@/mocks/server";
import { configKeys } from "@/shared/config/query-keys";
import type { ConfigRead, SaveRequest } from "@/shared/config/api";
import { strategyFieldErrors, type Strategy } from "./strategy-form";

function renderStrategies() {
  window.history.replaceState({}, "", "/strategies");
  setMockAuthenticated(true);
  const client = new QueryClient({ defaultOptions: { queries: { retry: false }, mutations: { retry: false } } });
  render(<ConfigProvider theme={{ token: { motion: false } }}><AppProviders queryClient={client}><App /></AppProviders></ConfigProvider>);
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

  it("上下移规则后提交完整策略，后台刷新不覆盖草稿且保存成功", async () => {
    const user = userEvent.setup();
    const original: Strategy = { ...strategiesConfigReadFixture.values[0].value, rules: [
      { hosts: "local", edns_client_subnet: { mode: "custom", custom_ip: "2001:db8::/48" } },
      { rule_set: "domains", upstream: "default-group", edns_client_subnet: { mode: "disabled" } },
      { rule_set: "custom", upstream: "default-group" },
    ], cache: { enabled: true, optimistic: { enabled: false, answer_ttl: "10s", max_age: "1h" } }, ttl_override: { min: "0s" } };
    let response: ConfigRead = { ...strategiesConfigReadFixture, values: [{ module: "strategy", value: original }] };
    let submitted: SaveRequest | undefined;
    server.use(
      http.get("/api/v2/config/modules/strategy", () => HttpResponse.json(response)),
      http.post("/api/v2/config/save", async ({ request }) => {
        submitted = await request.json() as SaveRequest;
        return HttpResponse.json({ state: response.state, persisted: true });
      }),
    );
    const client = renderStrategies();
    await user.click(await screen.findByLabelText("编辑策略 default", {}, { timeout: 10_000 }));
    const dialog = await screen.findByRole("dialog", { name: "编辑策略" });
    await user.click(within(dialog).getByLabelText("下移规则 1"));
    await user.click(within(dialog).getByLabelText("上移规则 3"));
    response = { ...response, state: { ...response.state, config_digest: "b".repeat(64) }, values: [{ module: "strategy", value: { ...original, rules: [original.rules[0]] } }] };
    await act(async () => { await client.invalidateQueries({ queryKey: configKeys.moduleRoot("strategy") }); });
    expect(await within(dialog).findByText("配置已更新，保存时将以当前内容替换本条目")).toBeInTheDocument();
    await user.click(within(dialog).getByRole("button", { name: "保存" }));
    expect(await screen.findByText("配置已应用并同步到文件")).toBeInTheDocument();
    await waitFor(() => expect(screen.queryByRole("dialog", { name: "编辑策略" })).not.toBeInTheDocument(), { timeout: 5_000 });
    expect(submitted).toEqual({ changes: [{ module: "strategy", change: {
      action: "update", original_name: original.name, value: { ...original, rules: [original.rules[1], original.rules[2], original.rules[0]] },
    } }] });
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
    let submitted: SaveRequest | undefined;
    server.use(
      http.get("/api/v2/config/modules/strategy", () => HttpResponse.json({ ...strategiesConfigReadFixture,
        values: [{ module: "strategy", value: { ...strategiesConfigReadFixture.values[0].value, ttl_override: ttl } }],
      })),
      http.post("/api/v2/config/save", async ({ request }) => {
        submitted = await request.json() as SaveRequest;
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

  it("ECS 解码错误保留规则索引并映射到自定义地址", () => {
    expect(strategyFieldErrors([{ path: "changes[0].change.value.rules[1].edns_client_subnet.custom_ip", code: "INVALID_ARGUMENT" }]))
      .toEqual([{ name: ["rules", 1, "ecs_custom_ip"], errors: ["INVALID_ARGUMENT"] }]);
  });

  it("配置路径字段错误定位到折叠规则并打开草稿", async () => {
    const user = userEvent.setup();
    server.use(http.post("/api/v2/config/save", () => HttpResponse.json({
      code: "VALIDATION_FAILED", message: "validation", request_id: "rule-error", retryable: false,
      field_errors: [{ path: "strategy[0].rules[1].upstream", code: "INVALID_ARGUMENT" }],
    }, { status: 422 })));
    renderStrategies();
    await user.click(await screen.findByLabelText("编辑策略 default", {}, { timeout: 10_000 }));
    const dialog = await screen.findByRole("dialog", { name: "编辑策略" });
    await user.click(within(dialog).getByRole("button", { name: "保存" }));
    expect(await within(dialog).findByText("INVALID_ARGUMENT")).toBeInTheDocument();
    expect(within(dialog).getByRole("button", { name: /^2\./ })).toHaveAttribute("aria-expanded", "true");
    expect(within(dialog).getByLabelText("匹配上游")).toHaveAttribute("aria-invalid", "true");
  });
});

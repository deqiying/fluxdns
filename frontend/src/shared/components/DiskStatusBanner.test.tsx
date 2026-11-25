import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { App } from "antd";
import { http, HttpResponse } from "msw";
import { beforeEach, expect, it } from "vitest";
import { configStateFixture, systemConfigReadFixture } from "@/mocks/fixtures";
import { setMockAuthenticated } from "@/mocks/handlers";
import { server } from "@/mocks/server";
import type { ConfigState } from "@/shared/config/api";
import { configKeys } from "@/shared/config/query-keys";
import { DiskStatusBanner } from "./DiskStatusBanner";

const withStatus = (status: ConfigState["disk"]["status"]): ConfigState => ({ ...configStateFixture, disk: { status } });

function mount() {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  client.setQueryData(configKeys.system, systemConfigReadFixture);
  render(<QueryClientProvider client={client}><App><DiskStatusBanner /></App></QueryClientProvider>);
  return client;
}

beforeEach(() => setMockAuthenticated(true));

it("in_sync 不显示提示，窗口聚焦后刷新最新磁盘状态", async () => {
  let state = withStatus("in_sync");
  server.use(http.get("/api/v2/config/state", () => HttpResponse.json(state)));
  const client = mount();
  await waitFor(() => expect(client.getQueryData(configKeys.state())).toEqual(state));
  expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  state = withStatus("modified");
  act(() => window.dispatchEvent(new Event("focus")));
  expect(await screen.findByText(/配置文件已在磁盘上被修改/)).toBeInTheDocument();
});

it.each([
  ["modified", /配置文件已在磁盘上被修改/, true],
  ["missing", /配置文件不存在/, false],
  ["unreadable", /配置文件无法读取/, false],
] as const)("%s 显示对应提示与操作", async (status, title, canReload) => {
  server.use(http.get("/api/v2/config/state", () => HttpResponse.json(withStatus(status))));
  mount();
  expect(await screen.findByRole("alert")).toHaveTextContent(title);
  expect(screen.getByRole("button", { name: "写入磁盘" })).toBeEnabled();
  expect(Boolean(screen.queryByRole("button", { name: "重新加载" }))).toBe(canReload);
});

it("写入磁盘经确认后只发送无请求体的 persist，并刷新系统配置", async () => {
  const user = userEvent.setup();
  let state = withStatus("missing");
  const bodies: string[] = [];
  server.use(
    http.get("/api/v2/config/state", () => HttpResponse.json(state)),
    http.post("/api/v2/config/persist", async ({ request }) => {
      bodies.push(await request.text());
      state = withStatus("in_sync");
      return HttpResponse.json({ state });
    }),
  );
  const client = mount();
  await user.click(await screen.findByRole("button", { name: "写入磁盘" }));
  expect(await screen.findByText(/磁盘上的外部修改将被丢弃/)).toBeInTheDocument();
  expect(bodies).toEqual([]);
  await user.click(screen.getByRole("button", { name: "确认写入磁盘" }));
  expect(await screen.findByText("已写入磁盘")).toBeInTheDocument();
  await waitFor(() => expect(screen.queryByText("配置文件不存在")).not.toBeInTheDocument());
  expect(bodies).toEqual([""]);
  expect(client.getQueryState(configKeys.system)?.isInvalidated).toBe(true);
});

it("重新加载遇到校验失败时显示字段诊断并保留原运行配置", async () => {
  const user = userEvent.setup();
  server.use(
    http.get("/api/v2/config/state", () => HttpResponse.json(withStatus("modified"))),
    http.post("/api/v2/config/reload", () => HttpResponse.json({
      code: "VALIDATION_FAILED", message: "request could not be completed", request_id: "reload-validation", retryable: false,
      field_errors: [{ path: "strategy[0].name", code: "INVALID_ARGUMENT" }],
    }, { status: 422 })),
  );
  const client = mount();
  await user.click(await screen.findByRole("button", { name: "重新加载" }));
  expect(await screen.findByText(/重新加载失败：.*strategy\[0\]\.name.*运行配置未更改/)).toBeInTheDocument();
  expect(client.getQueryData(configKeys.state())).toEqual(withStatus("modified"));
});

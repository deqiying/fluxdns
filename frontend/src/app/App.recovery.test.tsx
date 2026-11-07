import { QueryClient } from "@tanstack/react-query";
import { act, render, screen } from "@testing-library/react";
import { beforeEach, expect, it, vi } from "vitest";
import { setMockAuthenticated, setMockSetupRequired } from "@/mocks/handlers";
import { App } from "./App";
import { AppErrorBoundary } from "./AppErrorBoundary";
import { AppProviders } from "./providers";

const deployment = vi.hoisted(() => ({ oldChunksRemoved: false }));

// 模块随入口加载时可用；若等服务更新后才动态加载，就模拟旧分片已被移除。
vi.mock("@/modules/auth/LoginPage", async (importOriginal) => {
  if (deployment.oldChunksRemoved) throw new Error("旧登录页分片不存在");
  return importOriginal();
});
vi.mock("@/modules/auth/InitializePage", async (importOriginal) => {
  if (deployment.oldChunksRemoved) throw new Error("旧初始化页分片不存在");
  return importOriginal();
});

beforeEach(() => {
  deployment.oldChunksRemoved = false;
  vi.spyOn(console, "error").mockImplementation(() => {});
});

function renderApp(path: string) {
  window.history.replaceState({}, "", path);
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false }, mutations: { retry: false } } });
  render(
    <AppErrorBoundary>
      <AppProviders queryClient={queryClient}>
        <App />
      </AppProviders>
    </AppErrorBoundary>,
  );
  return queryClient;
}

it("旧分片被移除且刷新会话失效后，仍能返回登录页并清除旧数据", async () => {
  setMockAuthenticated(true);
  const queryClient = renderApp("/dashboard");
  await screen.findByRole("heading", { name: "服务状态" });
  queryClient.setQueryData(["previous-session"], { value: "private-record" });

  act(() => {
    deployment.oldChunksRemoved = true;
    setMockAuthenticated(false);
    document.dispatchEvent(new Event("visibilitychange"));
  });

  expect(await screen.findByRole("heading", { name: "登录 FluxDNS" })).toBeInTheDocument();
  expect(screen.getByText("登录状态已过期，请重新登录。")).toBeInTheDocument();
  expect(window.location.pathname).toBe("/login");
  expect(queryClient.getQueryData(["previous-session"])).toBeUndefined();
  expect(screen.queryByText("页面无法继续渲染")).not.toBeInTheDocument();
  expect(window.localStorage.length).toBe(0);
  expect(window.sessionStorage.length).toBe(0);
});

it("入口代码已加载后，初始化页面不再依赖旧的动态分片", async () => {
  deployment.oldChunksRemoved = true;
  setMockSetupRequired(true);
  renderApp("/initialize");

  expect(await screen.findByRole("heading", { name: "初始化 FluxDNS" })).toBeInTheDocument();
  expect(screen.queryByText("页面无法继续渲染")).not.toBeInTheDocument();
});

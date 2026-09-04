import { QueryClient } from "@tanstack/react-query";
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import { http, HttpResponse } from "msw";
import { setMockAuthenticated, setMockSetupRequired } from "@/mocks/handlers";
import { server } from "@/mocks/server";
import { AppProviders } from "./providers";
import { App } from "./App";

function renderApp(path: string) {
  window.history.replaceState({}, "", path);
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false }, mutations: { retry: false } } });
  return render(
    <AppProviders queryClient={queryClient}>
      <App />
    </AppProviders>,
  );
}

describe("application routes", () => {
  it("未初始化时先进入初始化页且不请求受保护数据", async () => {
    setMockSetupRequired(true);
    renderApp("/dashboard");
    expect(await screen.findByRole("heading", { name: "初始化 FluxDNS" })).toBeInTheDocument();
    expect(window.location.pathname).toBe("/initialize");
  });

  it("初始化成功后自动建立 session 并进入 Dashboard", async () => {
    const user = userEvent.setup();
    setMockSetupRequired(true);
    renderApp("/initialize");
    await screen.findByRole("heading", { name: "初始化 FluxDNS" });
    await user.type(screen.getByLabelText("用户名"), "admin");
    await user.type(screen.getByLabelText("密码"), "correct horse battery staple");
    await user.type(screen.getByLabelText("确认密码"), "correct horse battery staple");
    await user.click(screen.getByRole("button", { name: "创建管理账号" }));
    expect(await screen.findByRole("heading", { name: "运行总览" })).toBeInTheDocument();
    expect(window.location.pathname).toBe("/dashboard");
    expect(window.localStorage.length).toBe(0);
    expect(window.sessionStorage.length).toBe(0);
  });

  it("初始化并发冲突后刷新状态并返回登录页", async () => {
    const user = userEvent.setup();
    let setupState: "required" | "ready" = "required";
    setMockSetupRequired(true);
    server.use(
      http.get("/api/v1/auth/setup", () => HttpResponse.json({ state: setupState })),
      http.post("/api/v1/auth/setup", () => {
        setupState = "ready";
        return HttpResponse.json(
          { code: "SETUP_ALREADY_COMPLETED", message: "setup already completed", request_id: "mock-setup-409", retryable: false },
          { status: 409 },
        );
      }),
    );
    renderApp("/initialize");
    expect(await screen.findByRole("heading", { name: "初始化 FluxDNS" })).toBeInTheDocument();
    await user.type(screen.getByLabelText("用户名"), "admin");
    await user.type(screen.getByLabelText("密码"), "correct horse battery staple");
    await user.type(screen.getByLabelText("确认密码"), "correct horse battery staple");
    await user.click(screen.getByRole("button", { name: "创建管理账号" }));
    expect(await screen.findByRole("heading", { name: "登录 FluxDNS" })).toBeInTheDocument();
    expect(window.location.pathname).toBe("/login");
  });

  it("未登录时保护所有业务路由", async () => {
    renderApp("/runtime");
    expect(await screen.findByRole("heading", { name: "登录 FluxDNS" })).toBeInTheDocument();
    expect(window.location.pathname).toBe("/login");
  });

  it("登录后回跳原受保护路由且不持久化密码", async () => {
    const user = userEvent.setup();
    renderApp("/runtime");
    await screen.findByRole("heading", { name: "登录 FluxDNS" });

    await user.type(screen.getByLabelText("用户名"), "operator");
    const password = screen.getByLabelText("密码") as HTMLInputElement;
    await user.type(password, "fixture-password");
    await user.click(screen.getByRole("button", { name: /登\s*录/ }));

    expect(await screen.findByRole("heading", { name: "Runtime" })).toBeInTheDocument();
    await waitFor(() => expect(password.value).toBe(""));
    expect(window.localStorage.length).toBe(0);
    expect(window.sessionStorage.length).toBe(0);
  });

  it("有效 session 可直接进入 Dashboard 并区分局部不可用卡片", async () => {
    setMockAuthenticated(true);
    renderApp("/dashboard");
    expect(await screen.findByRole("heading", { name: "运行总览" })).toBeInTheDocument();
    expect(await screen.findByText("STORAGE_GAP")).toBeInTheDocument();
  });

  it.each([
    ["/health", "健康状态"],
    ["/statistics", "解析统计"],
    ["/queries", "解析记录"],
    ["/resources", "资源状态"],
    ["/system", "系统信息"],
  ])("有效 session 可加载只读页面 %s", async (path, heading) => {
    setMockAuthenticated(true);
    renderApp(path);
    expect(await screen.findByRole("heading", { name: heading, level: 2 })).toBeInTheDocument();
  });

  it("普通 API 返回 401 时只跳转一次并显示 session 过期提示", async () => {
    setMockAuthenticated(true);
    server.use(
      http.get("/api/v1/overview", () =>
        HttpResponse.json(
          { code: "AUTH_SESSION_EXPIRED", message: "expired", request_id: "expired-401", retryable: false },
          { status: 401 },
        ),
      ),
    );
    renderApp("/dashboard");
    expect(await screen.findByText("登录状态已过期，请重新登录。")).toBeInTheDocument();
    expect(window.location.pathname).toBe("/login");
  });

  it("登出后清理 session 并返回登录页", async () => {
    const user = userEvent.setup();
    setMockAuthenticated(true);
    renderApp("/dashboard");
    await screen.findByRole("heading", { name: "运行总览" });
    await user.click(screen.getByRole("button", { name: /退\s*出/ }));
    expect(await screen.findByRole("heading", { name: "登录 FluxDNS" })).toBeInTheDocument();
    expect(window.location.pathname).toBe("/login");
  });

  it("未知受保护路由显示 404 而不泄漏内部路径", async () => {
    setMockAuthenticated(true);
    renderApp("/unknown-route");
    expect(await screen.findByText("页面不存在")).toBeInTheDocument();
  });
});

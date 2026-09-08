import { QueryClient } from "@tanstack/react-query";
import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import { http, HttpResponse } from "msw";
import { setMockAuthenticated, setMockSetupRequired } from "@/mocks/handlers";
import { processMetricsFixture } from "@/mocks/fixtures";
import { server } from "@/mocks/server";
import { AppProviders } from "./providers";
import { App } from "./App";
import { managementRoutes } from "./route-contract";

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
    expect(await screen.findByRole("heading", { name: "服务状态" })).toBeInTheDocument();
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
    renderApp("/listeners");
    expect(await screen.findByRole("heading", { name: "登录 FluxDNS" })).toBeInTheDocument();
    expect(window.location.pathname).toBe("/login");
  });

  it("登录后回跳原受保护路由且不持久化密码", async () => {
    const user = userEvent.setup();
    renderApp("/listeners");
    await screen.findByRole("heading", { name: "登录 FluxDNS" });

    await user.type(screen.getByLabelText("用户名"), "operator");
    const password = screen.getByLabelText("密码") as HTMLInputElement;
    await user.type(password, "fixture-password");
    await user.click(screen.getByRole("button", { name: /登\s*录/ }));

    expect(await screen.findByRole("heading", { name: "监听入口" })).toBeInTheDocument();
    expect(window.location.pathname).toBe("/listeners");
    await waitFor(() => expect(password.value).toBe(""));
    expect(window.localStorage.length).toBe(0);
    expect(window.sessionStorage.length).toBe(0);
  });

  it("有效 session 可直接进入 Dashboard 并区分局部不可用卡片", async () => {
    setMockAuthenticated(true);
    renderApp("/dashboard");
    expect(await screen.findByRole("heading", { name: "服务状态" })).toBeInTheDocument();
    expect(await screen.findByText("STORAGE_GAP")).toBeInTheDocument();
  });

  it.each(
    managementRoutes
      .filter(({ path }) => path !== "/dashboard" && path !== "/queries" && path !== "/upstreams" && path !== "/system-runtime")
      .map(({ path, title }) => [path, title]),
  )("有效 session 可加载未接线入口 %s", async (path, heading) => {
    setMockAuthenticated(true);
    renderApp(path);
    expect(await screen.findByRole("heading", { name: heading, level: 2 })).toBeInTheDocument();
    expect(screen.getByText("当前版本暂不可用。")).toBeInTheDocument();
  });

  it("系统运行状态显示 v2 进程采样并可手动刷新", async () => {
    const user = userEvent.setup();
    let processRequests = 0;
    setMockAuthenticated(true);
    server.use(
      http.get("/api/v2/system/runtime", () => {
        processRequests += 1;
        return HttpResponse.json(processMetricsFixture);
      }),
    );
    renderApp("/system-runtime");

    expect(await screen.findByRole("heading", { name: "系统运行状态", level: 2 })).toBeInTheDocument();
    expect(await screen.findByText("186.4 MiB")).toBeInTheDocument();
    expect(screen.getByText("1.25%")).toBeInTheDocument();
    expect(screen.getByText("18")).toBeInTheDocument();
    expect(screen.getByText("0.1.0-dev")).toBeInTheDocument();
    expect(screen.getByText(/02:00:/)).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "刷新" }));
    await waitFor(() => expect(processRequests).toBe(2));
  });

  it("系统运行状态不把不可用进程读数伪装为零", async () => {
    setMockAuthenticated(true);
    server.use(
      http.get("/api/v2/system/runtime", () => HttpResponse.json({
        ...processMetricsFixture,
        rss_bytes: { state: "unavailable", reason: "sampling_failed", observed_seconds: null },
        cpu_percent: { state: "unavailable", reason: "warmup", observed_seconds: 1 },
        threads: { state: "unavailable", reason: "unsupported", observed_seconds: null },
      })),
    );
    renderApp("/system-runtime");

    expect(await screen.findByText("sampling_failed")).toBeInTheDocument();
    expect(screen.getByText("warmup")).toBeInTheDocument();
    expect(screen.getByText("unsupported")).toBeInTheDocument();
    expect(screen.queryByText("0 MiB")).not.toBeInTheDocument();
  });

  it("按三组展示十二个入口并保持当前激活态", async () => {
    const user = userEvent.setup();
    setMockAuthenticated(true);
    renderApp("/dashboard");
    await screen.findByRole("heading", { name: "服务状态" });

    const navigation = screen.getByRole("menu", { name: "主导航" });
    expect(within(navigation).getByText("监控")).toBeInTheDocument();
    expect(within(navigation).getByText("DNS 管理")).toBeInTheDocument();
    expect(within(navigation).getByText("系统")).toBeInTheDocument();
    for (const route of managementRoutes) {
      expect(within(navigation).getByText(route.title)).toBeInTheDocument();
    }

    expect(within(navigation).getByText("服务状态").closest("li")).toHaveClass("ant-menu-item-selected");
    await user.click(within(navigation).getByText("监听入口"));
    expect(await screen.findByRole("heading", { name: "监听入口" })).toBeInTheDocument();
    expect(window.location.pathname).toBe("/listeners");
    await user.click(screen.getByRole("button", { name: "收起导航" }));
    expect(screen.getByRole("button", { name: "展开导航" })).toBeInTheDocument();
  });

  it("DNS 上游页内 tab 使用查询参数并支持返回", async () => {
    const user = userEvent.setup();
    setMockAuthenticated(true);
    renderApp("/upstreams");
    expect(await screen.findByRole("heading", { name: "DNS 上游", level: 2 })).toBeInTheDocument();

    const groupsTab = screen.getByRole("tab", { name: "上游组" });
    await user.click(groupsTab);
    expect(groupsTab).toHaveAttribute("aria-selected", "true");
    expect(window.location.search).toBe("?tab=groups");

    window.history.back();
    await waitFor(() => expect(window.location.search).toBe(""));
    expect(screen.getByRole("tab", { name: "上游" })).toHaveAttribute("aria-selected", "true");
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
    await screen.findByRole("heading", { name: "服务状态" });
    await user.click(screen.getByRole("button", { name: "退出登录" }));
    expect(await screen.findByRole("heading", { name: "登录 FluxDNS" })).toBeInTheDocument();
    expect(window.location.pathname).toBe("/login");
  });

  it("未知受保护路由显示 404 而不泄漏内部路径", async () => {
    setMockAuthenticated(true);
    renderApp("/unknown-route");
    expect(await screen.findByRole("heading", { name: "页面不存在" })).toBeInTheDocument();
  });

  it("未知子路径不误选父级菜单", async () => {
    setMockAuthenticated(true);
    renderApp("/dashboard/details");
    expect(await screen.findByRole("heading", { name: "页面不存在" })).toBeInTheDocument();
    expect(document.querySelectorAll(".ant-menu-item-selected")).toHaveLength(0);
  });

  it("旧只读路由不提供兼容跳转", async () => {
    setMockAuthenticated(true);
    renderApp("/runtime");
    expect(await screen.findByRole("heading", { name: "页面不存在" })).toBeInTheDocument();
    expect(window.location.pathname).toBe("/runtime");
  });
});

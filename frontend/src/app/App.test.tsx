import { QueryClient } from "@tanstack/react-query";
import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import { http, HttpResponse } from "msw";
import { setMockAuthenticated, setMockSetupRequired } from "@/mocks/handlers";
import { configStateFixture, outboundConfigReadFixture, processMetricsFixture, systemConfigReadFixture } from "@/mocks/fixtures";
import { server } from "@/mocks/server";
import type { ConfigState } from "@/shared/config/api";
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

function captureConfigSave(requests: unknown[]) {
  return http.post("/api/v2/config/save", async ({ request }) => {
    requests.push(await request.json());
    return HttpResponse.json({ state: configStateFixture, persisted: true });
  });
}

// 全应用用例统一使用 vite.config.ts 的 CI 超时预算，避免局部旧值覆盖全局设置。
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
      http.get("/api/v2/auth/setup", () => HttpResponse.json({ state: setupState })),
      http.post("/api/v2/auth/setup", () => {
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

  it("有效 session 可直接进入 Dashboard 并读取 v2 服务指标", async () => {
    setMockAuthenticated(true);
    renderApp("/dashboard");
    expect(await screen.findByRole("heading", { name: "服务状态" })).toBeInTheDocument();
    expect(await screen.findByText("当前内存")).toBeInTheDocument();
  });

  it("应用布局在磁盘不一致时显示全局写入入口", async () => {
    setMockAuthenticated(true);
    const state: ConfigState = { ...configStateFixture, disk: { status: "modified" } };
    server.use(http.get("/api/v2/config/state", () => HttpResponse.json(state)));
    renderApp("/dashboard");
    expect(await screen.findByText(/配置文件已在磁盘上被修改/)).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "写入磁盘" })).toBeEnabled();
    expect(screen.getByRole("button", { name: "重新加载" })).toBeEnabled();
  });

  it.each(
    managementRoutes
      .filter(({ path }) => path !== "/dashboard" && path !== "/queries" && path !== "/listeners" && path !== "/upstreams" && path !== "/dns-settings" && path !== "/strategies" && path !== "/hosts" && path !== "/rule-sets" && path !== "/clients" && path !== "/proxies" && path !== "/system-settings" && path !== "/system-runtime")
      .map(({ path, title }) => [path, title]),
  )("有效 session 可加载未接线入口 %s", async (path, heading) => {
    setMockAuthenticated(true);
    renderApp(path);
    expect(await screen.findByRole("heading", { name: heading, level: 2 })).toBeInTheDocument();
    expect(screen.getByText("当前版本暂不可用。")).toBeInTheDocument();
  });

  it("代理页通过单次保存提交 SecretRef 引用", async () => {
    const user = userEvent.setup();
    const requests: unknown[] = [];
    setMockAuthenticated(true);
    server.use(captureConfigSave(requests));
    renderApp("/proxies");
    expect(await screen.findByRole("heading", { name: "代理配置", level: 2 })).toBeInTheDocument();
    expect(await screen.findByText("proxy-primary")).toBeInTheDocument();
    // 使用按钮已有的 aria-label，避免 jsdom 为整页按钮计算可访问名称和样式。
    await user.click(screen.getByLabelText("编辑代理 proxy-primary"));
    const dialog = await screen.findByRole("dialog", { name: "编辑代理" });
    const secret = within(dialog).getByLabelText("环境变量", { selector: "input[type='text']" });
    await user.clear(secret);
    await user.type(secret, "UPDATED_PROXY_URL");
    await user.click(within(dialog).getByRole("button", { name: "保存" }));
    await waitFor(() => expect(requests).toHaveLength(1));
    expect(requests[0]).toMatchObject({
      changes: [{
        module: "outbound",
        change: {
          action: "update",
          original_name: "proxy-primary",
          value: { name: "proxy-primary", type: "socks5", proxy_url: { env: "UPDATED_PROXY_URL" } },
        },
      }],
    });
    expect(Object.keys(requests[0] as object)).toEqual(["changes"]);
  });

  it("代理页展示脱敏内联 URL，并在未修改时原样提交引用", async () => {
    const user = userEvent.setup();
    const requests: unknown[] = [];
    setMockAuthenticated(true);
    // 后端返回内联来源时密码位已脱敏；前端应原样回填，不要求用户重新输入。
    server.use(
      http.get("/api/v2/config/modules/outbound", () =>
        HttpResponse.json({
          state: outboundConfigReadFixture.state,
          values: [
            {
              module: "outbound",
              value: {
                name: "proxy-inline",
                type: "socks5",
                proxy_url: "socks5://user:FLUXDNS_REDACTED_SECRET@proxy.example:1080",
              },
            },
          ],
          effective: [],
          references: [],
          runtime: [],
        }),
      ),
      captureConfigSave(requests),
    );
    renderApp("/proxies");
    expect(await screen.findByRole("heading", { name: "代理配置", level: 2 })).toBeInTheDocument();
    expect(await screen.findByText(/FLUXDNS_REDACTED_SECRET/)).toBeInTheDocument();
    await user.click(screen.getByLabelText("编辑代理 proxy-inline"));
    const dialog = await screen.findByRole("dialog", { name: "编辑代理" });
    // 不改动 URL，仅保存，形态保持原样（字符串形式的内联来源，密码仍为占位符）。
    await user.click(within(dialog).getByRole("button", { name: "保存" }));
    await waitFor(() => expect(requests).toHaveLength(1));
    expect(requests[0]).toMatchObject({
      changes: [{
        module: "outbound",
        change: {
          action: "update",
          original_name: "proxy-inline",
          value: {
            proxy_url: "socks5://user:FLUXDNS_REDACTED_SECRET@proxy.example:1080",
          },
        },
      }],
    });
  });

  it("Hosts 页面加载类型化来源和 Runtime 状态", async () => {
    setMockAuthenticated(true);
    renderApp("/hosts");
    expect(await screen.findByRole("heading", { name: "Hosts 配置", level: 2 })).toBeInTheDocument();
    expect(await screen.findByText("office")).toBeInTheDocument();
    expect(screen.getByText("stale")).toBeInTheDocument();
  });

  it("规则集页面区分远程来源和陈旧快照", async () => {
    const user = userEvent.setup();
    setMockAuthenticated(true);
    renderApp("/rule-sets");
    expect(await screen.findByRole("heading", { name: "规则集", level: 2 })).toBeInTheDocument();
    expect(await screen.findByText("domains")).toBeInTheDocument();
    expect(screen.getByText("stale")).toBeInTheDocument();
    // 刷新列显示可感知单位，不再直接暴露后端的纳秒串。
    expect(screen.getByText("1 天")).toBeInTheDocument();
    expect(screen.queryByText("86400000000000ns")).toBeNull();

    await user.click(screen.getByLabelText("编辑规则集 domains"));
    const dialog = await screen.findByRole("dialog", { name: "编辑规则集" });
    expect(within(dialog).getByLabelText("更新周期")).toHaveValue("1");
    expect(within(dialog).getByLabelText("更新周期单位").closest(".ant-select")?.textContent).toBe("天");
  });

  it("策略页面展示有序规则和覆盖来源", async () => {
    const user = userEvent.setup();
    const requests: unknown[] = [];
    setMockAuthenticated(true);
    server.use(captureConfigSave(requests));
    renderApp("/strategies");
    expect(await screen.findByRole("heading", { name: "DNS 分流策略", level: 2 })).toBeInTheDocument();
    expect(await screen.findByText("default-group")).toBeInTheDocument();
    expect(screen.getByText("2 条")).toBeInTheDocument();

    // 可选 TTL 覆盖同样按「数值 + 单位」回显；0 表示该边界不设限，必须允许保存。
    await user.click(screen.getByLabelText("编辑策略 default"));
    const dialog = await screen.findByRole("dialog", { name: "编辑策略" });
    const min = within(dialog).getByLabelText("最小 TTL");
    expect(min).toHaveValue("30");
    expect(within(dialog).getByLabelText("最小 TTL单位").closest(".ant-select")?.textContent).toBe("秒");
    expect(within(dialog).getByLabelText("最大 TTL")).toHaveValue("1");
    expect(within(dialog).getByLabelText("最大 TTL单位").closest(".ant-select")?.textContent).toBe("小时");
    await user.clear(min);
    await user.type(min, "0");
    await user.click(within(dialog).getByRole("button", { name: "保存" }));
    await waitFor(() => expect(requests).toHaveLength(1));
    expect(requests[0]).toMatchObject({
      changes: [{
        module: "strategy",
        change: {
          action: "update",
          original_name: "default",
          value: { ttl_override: { enabled: true, min: "0s", max: "1h" } },
        },
      }],
    });
  });

  it("Listener 页面展示真实绑定接纳状态", async () => {
    setMockAuthenticated(true);
    renderApp("/listeners");
    expect(await screen.findByRole("heading", { name: "监听入口", level: 2 })).toBeInTheDocument();
    expect(await screen.findByText("127.0.0.1:15353")).toBeInTheDocument();
    expect(screen.getAllByText("1/1 接受中")).toHaveLength(2);
  });

  it("DoH 编辑器说明裸路径可选 ID 和显式模板必填规则", async () => {
    const user = userEvent.setup();
    setMockAuthenticated(true);
    renderApp("/listeners");
    // 使用显式 aria-label，避免异步加载期间的全页角色枚举拖慢 CI；仍点击真实编辑按钮。
    await user.click(await screen.findByLabelText("编辑监听入口 https"));
    const dialog = await screen.findByRole("dialog", { name: "编辑监听入口" });
    expect(within(dialog).getByText(/裸路径可直接访问（按来源 IP 识别客户端），也可追加一个非空客户端 ID/)).toBeInTheDocument();
    expect(within(dialog).getByText("{client_id}")).toBeInTheDocument();
    expect(within(dialog).getByText(/必须填写，不接受省略该段的短路径/)).toBeInTheDocument();
    expect(within(dialog).getByText(/固定路径段优先，同形时显式模板优先/)).toBeInTheDocument();
  });

  it("监听入口标题区只保留同构短句和同步状态", async () => {
    const user = userEvent.setup();
    setMockAuthenticated(true);
    renderApp("/listeners");
    expect(await screen.findByText("每一处监听，稳定待命。")).toBeInTheDocument();
    const badge = await screen.findByRole("status", { name: "配置同步状态：已同步" });
    expect(badge).toHaveClass("config-sync-badge-synced");
    // 标题区不再暴露 revision，避免与 服务状态／解析记录 的标题区风格分叉。
    expect(screen.queryByText(/活动版本|文件版本/)).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "添加监听入口" })).toBeInTheDocument();
    // 搜索、空态与弹窗标题同批转为中文，防止只改按钮的半途状态回归。
    expect(screen.getByPlaceholderText("搜索监听入口名称")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "添加监听入口" }));
    expect(await screen.findByRole("dialog", { name: "添加监听入口" })).toBeInTheDocument();
    // 样式钩子：页面级与胶囊 class 必须存在，否则标题区规则会静默失效。
    expect(document.querySelector(".listener-page .page-heading")).not.toBeNull();
    expect(document.querySelector(".config-sync-badge")).not.toBeNull();
  });

  it("客户端 ID 默认锁定，显式修改并确认后随更新提交", async () => {
    const user = userEvent.setup();
    const requests: unknown[] = [];
    setMockAuthenticated(true);
    server.use(captureConfigSave(requests));
    renderApp("/clients");
    expect(await screen.findByRole("heading", { name: "客户端配置", level: 2 })).toBeInTheDocument();
    expect(await screen.findByText("Desktop-01")).toBeInTheDocument();
    // 概览卡与列表只展示非继承覆盖项；无 IP 的客户端提示仅按 ID 匹配。
    expect(screen.getByText("其中 1 个仅按 ID 匹配")).toBeInTheDocument();
    expect(screen.getByText("仅按 ID 匹配")).toBeInTheDocument();
    expect(screen.getByText("全部继承")).toBeInTheDocument();
    // 整页 getByRole 需计算全部可访问名称，在 jsdom 中耗时数秒；按钮自带 aria-label，直接按标签定位。
    await user.click(screen.getByLabelText("编辑客户端 desktop"));
    const dialog = await screen.findByRole("dialog", { name: /编辑客户端/ });
    expect(within(dialog).getByLabelText("客户端 ID")).toHaveAttribute("readonly");
    expect(within(dialog).queryByText("修改客户端 ID 会改变该终端的请求身份")).not.toBeInTheDocument();

    await user.click(within(dialog).getByRole("button", { name: "修改客户端 ID" }));
    // 解锁后切换为可编辑输入框，需重新获取元素。
    const input = within(dialog).getByLabelText("客户端 ID");
    expect(input).not.toHaveAttribute("readonly");
    // 用 paste 一次性写入：逐字 type 会让整页与异步校验重复渲染，在 CI 上超出统一预算。
    await user.clear(input);
    await user.paste("bad id");
    expect(await within(dialog).findByText(/仅支持 A-Z a-z 0-9/)).toBeInTheDocument();
    await user.clear(input);
    await user.paste("desktop-home");
    expect(await within(dialog).findByText("修改客户端 ID 会改变该终端的请求身份")).toBeInTheDocument();
    expect(await within(dialog).findByText("格式有效，且未被其他客户端使用")).toBeInTheDocument();

    await user.click(within(dialog).getByRole("button", { name: "保存" }));
    // ID 变化必须先经过单独确认，且勾选知情项前不能确认；确认前不发起校验请求。
    // 测试环境的 useId 固定返回同一值，抽屉与弹窗的 aria-labelledby 会冲突，因此按标题文本定位弹窗。
    const [confirmTitle] = await screen.findAllByText("确认修改客户端 ID？");
    const confirm = confirmTitle.closest<HTMLElement>(".ant-modal");
    if (!confirm) throw new Error("确认弹窗未渲染");
    expect(within(confirm).getByText("Desktop-01")).toBeInTheDocument();
    expect(within(confirm).getByText("desktop-home")).toBeInTheDocument();
    const confirmButton = within(confirm).getByRole("button", { name: "确认修改" });
    expect(confirmButton).toBeDisabled();
    expect(requests).toHaveLength(0);
    await user.click(within(confirm).getByRole("checkbox"));
    await user.click(confirmButton);
    await waitFor(() => expect(requests).toHaveLength(1));
    expect(requests[0]).toMatchObject({
      changes: [{
        module: "clients",
        change: { action: "update", original_name: "desktop", value: { name: "desktop", client_id: "desktop-home" } },
      }],
    });
  });

  it("DNS 页面展示缓存、详情和真实保留状态", async () => {
    const user = userEvent.setup();
    setMockAuthenticated(true);
    renderApp("/dns-settings");
    expect(await screen.findByRole("heading", { name: "DNS 配置", level: 2 })).toBeInTheDocument();
    expect(await screen.findByText("数据保留")).toBeInTheDocument();
    // 存储大小一律以人类可读单位展示，不再暴露原始字节数。
    expect(await screen.findByText("详情 768 MB")).toBeInTheDocument();
    expect(await screen.findByText("64 MB")).toBeInTheDocument();
    expect(await screen.findByText("1 GB")).toBeInTheDocument();

    // 所有 Duration 字段按「数值 + 单位」回显，单位从纳秒串换算成最大整单位。
    await user.click(screen.getByRole("button", { name: "编辑 DNS" }));
    const dialog = await screen.findByRole("dialog", { name: "编辑 DNS 配置" });
    expect(within(dialog).getByLabelText("失败 TTL")).toHaveValue("5");
    expect(within(dialog).getByLabelText("失败 TTL单位").closest(".ant-select")?.textContent).toBe("秒");
    expect(within(dialog).getByLabelText("回答 TTL")).toHaveValue("10");
    expect(within(dialog).getByLabelText("快照周期")).toHaveValue("5");
    expect(within(dialog).getByLabelText("快照周期单位").closest(".ant-select")?.textContent).toBe("分钟");
    expect(within(dialog).getByLabelText("最大陈旧时间")).toHaveValue("1");
    expect(within(dialog).getByLabelText("最大陈旧时间单位").closest(".ant-select")?.textContent).toBe("天");
    expect(within(dialog).queryByDisplayValue("5000000000ns")).toBeNull();
  });

  it("系统配置页面区分只读启动配置和可编辑日志", async () => {
    const user = userEvent.setup();
    setMockAuthenticated(true);
    renderApp("/system-settings");
    expect(await screen.findByRole("heading", { name: "系统配置", level: 2 })).toBeInTheDocument();
    expect(await screen.findByText("活动源配置中的只读路径表达")).toBeInTheDocument();
    expect(await screen.findByText("D:/Projects/Rust/fluxdns/_fluxdns/statistics.db")).toBeInTheDocument();
    expect(screen.getByText("http://127.0.0.1:8080")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "编辑日志" }));
    expect(await screen.findByRole("dialog", { name: "编辑日志" })).toBeInTheDocument();
    expect(screen.getByLabelText("日志文件")).toHaveValue("./logs/fluxdns.log");
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
    expect(await screen.findByText("186.4 MB")).toBeInTheDocument();
    expect(screen.getByText("1.25%")).toBeInTheDocument();
    expect(screen.getByText("18")).toBeInTheDocument();
    expect(screen.getByText(processMetricsFixture.version)).toBeInTheDocument();
    expect(screen.getByText(/02:00:/)).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "刷新" }));
    await waitFor(() => expect(processRequests).toBe(2));
  });

  it("首次进入系统运行页即采用服务器时区，无需先打开其他页面", async () => {
    setMockAuthenticated(true);
    server.use(http.get("/api/v2/system/runtime", () => HttpResponse.json({ ...processMetricsFixture, timezone: "Asia/Shanghai" })));
    renderApp("/system-runtime");
    await screen.findByRole("heading", { name: "系统运行状态", level: 2 });
    expect(screen.getByRole("group", { name: "启动时间" })).toHaveTextContent("2026/09/09 04:40:00 Asia/Shanghai (UTC+08:00)");
    expect(screen.getByRole("group", { name: "采样时间" })).toHaveTextContent("2026/09/03 16:00:00 Asia/Shanghai (UTC+08:00)");
  });

  it("运行时首次失败后放行系统页，挂载不循环重试且重试不卸载页面", async () => {
    const user = userEvent.setup();
    let reads = 0;
    setMockAuthenticated(true);
    server.use(http.get("/api/v2/system/runtime", () => { reads += 1; return new HttpResponse(null, { status: 503 }); }));
    renderApp("/system-runtime");
    const heading = await screen.findByRole("heading", { name: "系统运行状态", level: 2 });
    expect(reads).toBe(1);
    expect(screen.getByRole("button", { name: "重试时区读取" })).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "重试时区读取" }));
    await waitFor(() => expect(reads).toBe(2));
    expect(screen.getByRole("heading", { name: "系统运行状态", level: 2 })).toBe(heading);
  });

  it("系统运行状态把指标与运行信息放进同一条四列栅格", async () => {
    setMockAuthenticated(true);
    renderApp("/system-runtime");
    expect(await screen.findByRole("heading", { name: "系统运行状态", level: 2 })).toBeInTheDocument();

    const metricList = await screen.findByRole("list", { name: "进程指标" });
    // 四个同构格子：多出来的「采样时间」不再以 16px 双行日期混进 30px 数值格。
    expect(within(metricList).getAllByRole("listitem")).toHaveLength(4);
    expect(metricList).toHaveTextContent("运行时长");
    expect(metricList).toHaveTextContent("自启动累计 2 小时 0 分钟");
    expect(metricList).toHaveTextContent("进程 RSS，含映射页");
    expect(metricList).toHaveTextContent("相对单核，1 秒采样窗口");

    expect(screen.getByRole("group", { name: "启动时间" })).toHaveTextContent("2026/09/08 20:40:00 UTC");
    expect(screen.getByRole("group", { name: "采样时间" })).toHaveTextContent("2026/09/03 08:00:00 UTC");
    expect(screen.getByRole("group", { name: "采样来源" })).toHaveTextContent("主实例进程");
  });

  it("系统运行状态呈现主机环境与数据面摘要", async () => {
    setMockAuthenticated(true);
    renderApp("/system-runtime");
    expect(await screen.findByRole("heading", { name: "系统运行状态", level: 2 })).toBeInTheDocument();

    expect(await screen.findByRole("group", { name: "操作系统" })).toHaveTextContent("Windows 11 Pro 24H2");
    expect(screen.getByRole("group", { name: "内核 / 构建版本" })).toHaveTextContent("10.0.26200.5483");
    expect(screen.getByRole("group", { name: "CPU 架构" })).toHaveTextContent("x86_64 · 16 逻辑核心");
    expect(screen.getByRole("group", { name: "主机名" })).toHaveTextContent("fluxdns-win");
    expect(screen.getByRole("group", { name: "进程 ID" })).toHaveTextContent("4,242");
    expect(screen.getByRole("group", { name: "数据目录" })).toHaveTextContent("D:/Projects/Rust/fluxdns/_fluxdns");

    expect(await screen.findByRole("group", { name: "监听入口" }))
      .toHaveTextContent("2 个绑定 · UDP 127.0.0.1:15353 · DoH 127.0.0.1:18443");
    expect(screen.getByRole("group", { name: "在线客户端" })).toHaveTextContent("2 个");
    expect(screen.getByRole("group", { name: "DNS 缓存快照" })).toHaveTextContent("空闲 · 第 3 代 · 32 KB");
    const revisionRow = screen.getByRole("group", { name: "配置版本" });
    expect(revisionRow).toHaveTextContent(configStateFixture.config_digest.slice(0, 12));
    expect(revisionRow).toHaveTextContent("已同步");
    expect(screen.queryByText(/需重启 FluxDNS 后生效/)).not.toBeInTheDocument();
    expect(screen.getByRole("group", { name: "资源状态" })).toHaveTextContent("Hosts 1/2 就绪 · 规则集 1/2 就绪");
  });

  it.each(["/system-runtime", "/system-settings"])("启动级变更只在运行状态页提示：%s", async (path) => {
    setMockAuthenticated(true);
    server.use(http.get("/api/v2/config/system", () => HttpResponse.json({
      ...systemConfigReadFixture, restart_required: ["database", "webui", "dns.resolve_log"],
    })));
    renderApp(path);
    const hint = "以下配置已修改，需重启 FluxDNS 后生效：数据库、WebUI 监听、解析详情记录";
    if (path === "/system-runtime") expect(await screen.findByText(hint)).toBeInTheDocument();
    else {
      await screen.findByText(systemConfigReadFixture.database_path);
      expect(screen.queryByText(hint)).not.toBeInTheDocument();
    }
  });

  it("系统运行状态保留主机与数据面的降级原因", async () => {
    setMockAuthenticated(true);
    server.use(
      http.get("/api/v2/system/runtime", () => HttpResponse.json({
        ...processMetricsFixture,
        host: { ...processMetricsFixture.host, os: null, kernel: null, hostname: null },
      })),
      http.get("/api/v2/config/modules/listener", () => HttpResponse.json(
        { code: "NOT_FOUND", message: "listener state unavailable", request_id: "mock-v2-404", retryable: false, field_errors: [] },
        { status: 404, headers: { "X-Request-Id": "mock-v2-404" } },
      )),
    );
    renderApp("/system-runtime");
    expect(await screen.findByRole("heading", { name: "系统运行状态", level: 2 })).toBeInTheDocument();

    // 主机字段读取失败时不伪造值，行高由固定三段式排布保持不变。
    const osRow = await screen.findByRole("group", { name: "操作系统" });
    expect(within(osRow).getByText("暂不可用")).toBeInTheDocument();
    expect(within(screen.getByRole("group", { name: "主机名" })).getByText("暂不可用")).toBeInTheDocument();

    // 数据面读取失败保留契约错误码，而不是显示空值。
    const listenerRow = screen.getByRole("group", { name: "监听入口" });
    await waitFor(() => expect(listenerRow).toHaveTextContent("NOT_FOUND"));
    expect(within(listenerRow).getByText("暂不可用")).toBeInTheDocument();
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
    expect(screen.queryByText("0 MB")).not.toBeInTheDocument();
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

  it("服务状态使用 v2 固定口径并在同一趋势图保留缺口", async () => {
    setMockAuthenticated(true);
    renderApp("/dashboard");

    expect(await screen.findByText("当前内存")).toBeInTheDocument();
    expect(screen.getByRole("group", { name: "当前内存" })).toHaveTextContent("186.4 MB");
    expect(screen.getByRole("group", { name: "实时 QPS" })).toHaveTextContent("13 请求/秒");
    expect(screen.getByRole("group", { name: "实时 QPS" })).toHaveTextContent("平均 QPS 4.25");
    expect(screen.getByRole("group", { name: "实时 RPM" })).toHaveTextContent("314 请求/分钟");
    expect(screen.getByRole("group", { name: "实时 RPM" })).toHaveTextContent("平均 RPM 255");
    expect(screen.getByRole("group", { name: "CPU 占用" })).toHaveTextContent("1.25 %");
    expect(screen.getByRole("img", { name: /QPS 与 RPM 请求趋势/ })).toBeInTheDocument();
    expect(document.querySelectorAll(".metrics-chart-qps-line")).toHaveLength(2);
    expect(document.querySelectorAll(".metrics-chart-rpm-line")).toHaveLength(2);
  });

  it("DNS 上游页内 tab 使用查询参数并支持返回", async () => {
    const user = userEvent.setup();
    setMockAuthenticated(true);
    renderApp("/upstreams");
    expect(await screen.findByRole("heading", { name: "DNS 上游", level: 2 })).toBeInTheDocument();
    expect(await screen.findByText("secure-dns")).toBeInTheDocument();

    const groupsTab = screen.getByRole("tab", { name: "上游组" });
    await user.click(groupsTab);
    expect(groupsTab).toHaveAttribute("aria-selected", "true");
    expect(window.location.search).toBe("?tab=groups");

    window.history.back();
    await waitFor(() => expect(window.location.search).toBe(""));
    await waitFor(() => expect(screen.getByRole("tab", { name: "上游" })).toHaveAttribute("aria-selected", "true"));
  });

  it("上游组保存时超时按秒回显并以紧凑 duration 提交", async () => {
    const user = userEvent.setup();
    const requests: unknown[] = [];
    setMockAuthenticated(true);
    server.use(captureConfigSave(requests));
    renderApp("/upstreams?tab=groups");
    expect(await screen.findByText("default-group")).toBeInTheDocument();
    await user.click(screen.getByLabelText("编辑上游 default-group"));
    const dialog = await screen.findByRole("dialog", { name: "编辑上游" });

    // 后端回显的纳秒串换算成「秒」，界面不再出现纳秒量级文本。
    expect(within(dialog).getByLabelText("主要超时")).toHaveValue("5");
    expect(within(dialog).getByLabelText("主要超时单位").closest(".ant-select")?.textContent).toBe("秒");
    expect(within(dialog).getByLabelText("Fallback 超时")).toHaveValue("3");
    expect(within(dialog).queryByDisplayValue("5000000000ns")).toBeNull();

    // 改成毫秒单位后提交仍是紧凑 duration 串，不写裸数字。
    const timeout = within(dialog).getByLabelText("主要超时");
    await user.clear(timeout);
    await user.type(timeout, "1500");
    await user.click(within(dialog).getByLabelText("主要超时单位"));
    await user.click(await screen.findByTitle("毫秒"));
    await user.click(within(dialog).getByRole("button", { name: "保存" }));
    await waitFor(() => expect(requests).toHaveLength(1));
    expect(requests[0]).toMatchObject({
      changes: [{
        module: "upstreams",
        change: {
          action: "update",
          original_name: "default-group",
          value: { type: "group", timeout: "1500ms", fallback_timeout: "3s" },
        },
      }],
    });
    expect(Object.keys(requests[0] as object)).toEqual(["changes"]);
    expect(await screen.findByText("配置已应用并同步到文件")).toBeInTheDocument();
    expect(requests).toHaveLength(1);
  });

  it("普通 API 返回 401 时只跳转一次并显示 session 过期提示", async () => {
    setMockAuthenticated(true);
    server.use(
      http.get("/api/v2/service/metrics", () =>
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

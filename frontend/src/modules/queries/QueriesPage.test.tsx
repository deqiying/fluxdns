import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { ConfigProvider } from "antd";
import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import { http, HttpResponse } from "msw";
import { server } from "@/mocks/server";
import { sessionFixture, v2QueryPageFixture, v2QueryRecordsFixture } from "@/mocks/fixtures";
import { setMockAuthenticated } from "@/mocks/handlers";
import { acceptAuthSession } from "@/shared/api/client";
import { managementEvents, type QueryBatch } from "@/shared/api/events";
import type { QueryRecord, QueryRequest } from "./api";
import { formatCacheActivityResult, formatClientIdentity, formatDurationSummary, formatQueryTime, formatResponseSummary, formatRoute, QueriesPage, sourceLabel } from "./QueriesPage";
import { TimeZoneContext } from "@/shared/time/TimeZoneContext";

function renderPage(timeZone = "UTC") {
  setMockAuthenticated(true);
  acceptAuthSession({
    session: sessionFixture,
    access_token: "A".repeat(43),
    token_type: "Bearer",
    access_expires_at_ms: Date.now() + 300_000,
  });
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  // jsdom 行为测试关闭 CSS motion；浮层准备阶段仍异步，涉及可见性的断言必须等待终态。
  const page = (zone: string) => <ConfigProvider theme={{ token: { motion: false } }}><QueryClientProvider client={queryClient}><TimeZoneContext.Provider value={{ timeZone: zone, fallbackReason: null }}><QueriesPage /></TimeZoneContext.Provider></QueryClientProvider></ConfigProvider>;
  const view = render(page(timeZone));
  return { ...view, queryClient, changeTimeZone: (zone: string) => view.rerender(page(zone)) };
}

/**
 * jsdom 的 matchMedia 默认全部不匹配，antd 会按 responsive 过滤掉「时间」「路由」两列；
 * 需要断言完整列顺序时先让所有断点命中，返回的函数用于恢复全局 mock。
 */
function openAllBreakpoints(): () => void {
  const original = window.matchMedia;
  Object.defineProperty(window, "matchMedia", {
    configurable: true,
    writable: true,
    value: (query: string) => ({
      matches: !query.includes("max-width: 760px"),
      media: query,
      onchange: null,
      addListener: () => {},
      removeListener: () => {},
      addEventListener: () => {},
      removeEventListener: () => {},
      dispatchEvent: () => false,
    }),
  });
  return () => Object.defineProperty(window, "matchMedia", { configurable: true, writable: true, value: original });
}

describe("QueriesPage 展示语义", () => {
  const [direct, cache] = v2QueryRecordsFixture;

  afterEach(() => vi.restoreAllMocks());

  it("时区变化仅重建日期/游标/详情，保留自动刷新、页大小、排序和高级面板偏好", async () => {
    const user = userEvent.setup();
    vi.spyOn(Date, "now").mockReturnValue(Date.parse("2026-11-11T00:30:00Z"));
    const requests: QueryRequest[] = [];
    server.use(http.post("/api/v2/queries/search", async ({ request }) => { requests.push(await request.json() as QueryRequest); return HttpResponse.json(v2QueryPageFixture); }));
    const view = renderPage();
    await screen.findByRole("button", { name: "查看 example.test. 的详情" });
    await user.click(screen.getByRole("switch", { name: "自动刷新" }));
    fireEvent.mouseDown(screen.getByLabelText("每页条数"));
    fireEvent.click(await screen.findByTitle("50 条/页"));
    await user.click(screen.getByRole("button", { name: "高级筛选" }));
    fireEvent.mouseDown(screen.getByLabelText("排序字段"));
    fireEvent.click(await screen.findByTitle("DURATION"));
    fireEvent.mouseDown(screen.getByLabelText("排序方向"));
    fireEvent.click(await screen.findByTitle("ASC"));
    await user.type(screen.getByLabelText("域名"), "example");
    await user.click(screen.getByRole("button", { name: "查询" }));
    await waitFor(() => expect(requests.at(-1)?.filter.qname).toBe("example"));
    await user.click(screen.getByRole("button", { name: "下一页" }));
    await waitFor(() => expect(requests.at(-1)?.cursor).toBe(v2QueryPageFixture.next_cursor));
    await user.click(screen.getByRole("button", { name: "查看 example.test. 的详情" }));
    await screen.findByRole("dialog");
    view.changeTimeZone("Asia/Shanghai");
    await waitFor(() => expect(requests.at(-1)).toMatchObject({ cursor: null, page_size: 50, sort: "duration", order: "asc", filter: { from_ms: Date.parse("2026-11-04T16:00:00Z"), to_ms: Date.parse("2026-11-11T16:00:00Z") } }));
    expect(requests.at(-1)?.filter.qname).toBeUndefined();
    expect(screen.getByRole("switch", { name: "自动刷新" })).not.toBeChecked();
    expect(screen.getByLabelText("域名")).toHaveValue("");
    expect(screen.getByLabelText("每页条数").closest(".ant-select")).toHaveTextContent("50 条/页");
    expect(screen.getByLabelText("排序字段").closest(".ant-select")).toHaveTextContent("DURATION");
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  });

  it("上海时区统一用于列表、详情、默认七天和自定义日期的请求边界", async () => {
    const user = userEvent.setup();
    vi.spyOn(Date, "now").mockReturnValue(Date.parse("2026-11-10T16:30:00Z"));
    const requests: QueryRequest[] = [];
    const record = { ...direct, occurred_at_ms: Date.parse("2026-11-10T16:30:00.123Z") };
    server.use(http.post("/api/v2/queries/search", async ({ request }) => {
      requests.push(await request.json() as QueryRequest);
      return HttpResponse.json({ ...v2QueryPageFixture, items: [record] });
    }));
    const restore = openAllBreakpoints();
    try {
      renderPage("Asia/Shanghai");
      const trigger = await screen.findByRole("button", { name: "查看 example.test. 的详情" });
      expect(screen.getByRole("table")).toHaveTextContent("00:30:00");
      expect(screen.getByRole("table")).toHaveTextContent("2026-11-11");
      expect(requests[0].filter).toMatchObject({ from_ms: Date.parse("2026-11-04T16:00:00Z"), to_ms: Date.parse("2026-11-11T16:00:00Z") });
      await user.click(trigger);
      expect(await screen.findByRole("dialog")).toHaveTextContent("2026-11-11 00:30:00.123 Asia/Shanghai (UTC+08:00)");
      await user.click(screen.getByRole("button", { name: "关闭详情" }));
      await user.click(screen.getByText("自定义", { selector: ".ant-segmented-item-label" }));
      expect(screen.getByLabelText("开始")).toHaveValue("2026-11-05");
      expect(screen.getByLabelText("结束")).toHaveValue("2026-11-11");
      fireEvent.change(screen.getByLabelText("开始"), { target: { value: "2026-11-11" } });
      await user.click(screen.getByRole("button", { name: "查询" }));
      await waitFor(() => expect(requests.at(-1)?.filter).toMatchObject({ from_ms: Date.parse("2026-11-10T16:00:00Z"), to_ms: Date.parse("2026-11-11T16:00:00Z") }));
      fireEvent.change(screen.getByLabelText("开始"), { target: { value: "2026-11-12" } });
      expect(screen.getByRole("button", { name: "查询" })).toBeDisabled();
    } finally { restore(); }
  });

  it("纽约夏令时结束日提交 25 小时区间，结束日期不加固定 24 小时", async () => {
    const user = userEvent.setup();
    const requests: QueryRequest[] = [];
    server.use(http.post("/api/v2/queries/search", async ({ request }) => {
      requests.push(await request.json() as QueryRequest);
      return HttpResponse.json(v2QueryPageFixture);
    }));
    renderPage("America/New_York");
    await screen.findByRole("button", { name: "查看 example.test. 的详情" });
    await user.click(screen.getByText("自定义", { selector: ".ant-segmented-item-label" }));
    fireEvent.change(screen.getByLabelText("开始"), { target: { value: "2026-11-01" } });
    fireEvent.change(screen.getByLabelText("结束"), { target: { value: "2026-11-01" } });
    await user.click(screen.getByRole("button", { name: "查询" }));
    await waitFor(() => expect(requests.at(-1)?.filter).toMatchObject({ from_ms: Date.parse("2026-11-01T04:00:00Z"), to_ms: Date.parse("2026-11-02T05:00:00Z") }));
  });

  it("区分缓存生产出口、直接上游与三种耗时", () => {
    expect(formatRoute(cache)).toBe("udp-in → default → public → public-2");
    expect(formatRoute(direct)).toBe("doh-in → default → public → public-1");
    expect(formatRoute({ ...direct, upstream_target_name: null, upstream_used_name: null })).toBe("doh-in → default → 上游未确定");
    expect(formatDurationSummary(direct)).toEqual({ total: "总耗时 0.12 ms", dnsCore: "主链耗时 0.1 ms", response: "响应耗时 0.18 ms" });
    expect(formatDurationSummary({ ...direct, response_duration_us: null }).response).toBe("响应耗时 未记录");
  });

  it("显示首条 Answer 与截断总数", () => {
    expect(formatResponseSummary(direct)).toEqual({ primary: "A  192.0.2.1", meta: "1 条结果" });
    expect(formatResponseSummary(cache)).toEqual({ primary: "NOERROR · answered", meta: "保留 0 条，共 20 条" });
    expect(formatResponseSummary({ ...direct, rcode: "NOERROR", answers: { state: "available", total_count: 0, records: [] } }))
      .toEqual({ primary: "NOERROR · NODATA", meta: "0 条结果" });
  });

  it("缓存变化附带响应类与写入 TTL，旧记录缺字段时不展示", () => {
    const activity = cache.cache_activity!;
    expect(formatCacheActivityResult(activity)).toBe("NODATA 300s");
    expect(formatCacheActivityResult({ ...activity, response_class: "positive", ttl_secs: 30 })).toBe("TTL 30s");
    expect(formatCacheActivityResult({ ...activity, response_class: "servfail", ttl_secs: null })).toBe("SERVFAIL");
    expect(formatCacheActivityResult({ ...activity, response_class: null, ttl_secs: null })).toBeNull();
  });

  it("悬停预览离开后关闭，点击固定可切换记录，内部点击保留、外部点击关闭", async () => {
    const user = userEvent.setup();
    renderPage();
    const first = await screen.findByRole("button", { name: "查看 example.test. 的详情" });
    const second = screen.getByRole("button", { name: "查看 cached.example.test. 的详情" });
    await user.hover(first);
    expect(await screen.findByRole("dialog")).toHaveTextContent("悬停预览");
    await user.unhover(first);
    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
    await user.click(first);
    await user.unhover(first);
    const detail = await screen.findByRole("dialog", { name: "example.test. 解析详情" });
    expect(detail).toHaveTextContent("点击固定");
    expect(detail.querySelector("details")).not.toHaveAttribute("open");
    await user.click(within(detail).getByText("诊断信息"));
    expect(detail.querySelector("details")).toHaveAttribute("open");
    expect(detail).toHaveTextContent("总耗时");
    expect(detail).toHaveTextContent("主链耗时");
    expect(detail).toHaveTextContent("响应耗时");
    await user.click(within(detail).getByText("总耗时", { selector: ".ant-descriptions-item-label" }));
    expect(detail).toBeInTheDocument();
    await user.click(second);
    expect(await screen.findByRole("dialog", { name: "cached.example.test. 解析详情" })).toHaveTextContent("后台刷新");
    expect(screen.getAllByRole("dialog")).toHaveLength(1);
    await user.click(screen.getByRole("heading", { name: "解析记录" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
  });

  it("外部点击筛选输入框关闭详情后保留新焦点并可连续输入", async () => {
    const user = userEvent.setup();
    renderPage();
    await user.click(await screen.findByRole("button", { name: "查看 example.test. 的详情" }));
    // 节点挂载后仍可能处于透明的浮层准备阶段，等待可见后再验证外部点击。
    await waitFor(() => {
      expect(screen.getByRole("dialog", { name: "example.test. 解析详情" })).toBeVisible();
    });
    const input = screen.getByRole("textbox", { name: "域名" });
    await user.click(input);
    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
    expect(input).toHaveFocus();
    await user.keyboard("continuous-input");
    expect(input).toHaveValue("continuous-input");
  });

  it("Hosts 在结果列与路由列标签行都显示来源标签，且不出现写入标签", async () => {
    const restoreMatchMedia = openAllBreakpoints();
    server.use(http.post("/api/v2/queries/search", () => HttpResponse.json({ ...v2QueryPageFixture, items: [
      { ...direct, source: "hosts", cache: "bypass", cache_activity: null },
      { ...cache, source: "upstream", cache: "miss", cache_activity: null },
    ] })));
    try {
      renderPage();
      await screen.findByText("example.test.");
      expect(screen.queryByText("新建缓存")).not.toBeInTheDocument();
      expect(screen.queryByText("未写入缓存")).not.toBeInTheDocument();
      const row = screen.getByText("example.test.").closest("tr");
      expect(row).not.toBeNull();
      // 结果列来源标签与路由列标签行同级别的 Hosts 标签各一个。
      expect(within(row as HTMLElement).getAllByText("Hosts", { selector: ".ant-tag" })).toHaveLength(2);
      const routeCell = (row as HTMLElement).querySelector(".query-route-chain")?.closest(".query-cell");
      expect(routeCell?.querySelector(".query-cell-secondary .ant-tag")).toHaveTextContent("Hosts");
    } finally {
      restoreMatchMedia();
    }
  });

  it("鼠标 focus 不提前展开浮窗抢占点击，完整点击后固定详情", async () => {
    const matches = Element.prototype.matches;
    vi.spyOn(Element.prototype, "matches").mockImplementation(function (this: Element, selector: string) {
      return selector === ":focus-visible" ? false : matches.call(this, selector);
    });
    renderPage();
    const trigger = await screen.findByRole("button", { name: "查看 example.test. 的详情" });
    fireEvent.pointerDown(trigger, { pointerType: "mouse" });
    fireEvent.focus(trigger);
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    fireEvent.pointerUp(trigger, { pointerType: "mouse" });
    fireEvent.click(trigger);
    expect(await screen.findByRole("dialog")).toHaveTextContent("点击固定");
  });

  it("首次快照后自动订阅增量，首页无详情时插入新记录，关闭开关释放订阅", async () => {
    const user = userEvent.setup();
    let push: ((batch: QueryBatch) => void) | undefined;
    const release = vi.fn();
    const subscribe = vi.spyOn(managementEvents, "subscribeQueries").mockImplementation((_options, onData, _resync, onState) => {
      push = onData;
      onState("open");
      return release;
    });
    renderPage();
    await waitFor(() => expect(subscribe).toHaveBeenCalled());
    expect(subscribe.mock.calls[0][0].after).toEqual(v2QueryPageFixture.snapshot_cursor);
    act(() => push?.({ cursor: { epoch: "stream-1", sequence: "43" }, directoryRevision: "clients-13", items: [
      { ...direct, id: "new-auto", qname: "auto.example.test.", occurred_at_ms: direct.occurred_at_ms + 1_000 },
    ] }));
    expect(await screen.findByText("auto.example.test.")).toBeInTheDocument();
    await user.click(screen.getByRole("switch", { name: "自动刷新" }));
    await waitFor(() => expect(release).toHaveBeenCalled());
  });

  it("默认开启实时更新，以 opaque cursor 翻页并按稳定 ID 打开详情", async () => {
    const user = userEvent.setup();
    const requests: QueryRequest[] = [];
    server.use(http.post("/api/v2/queries/search", async ({ request }) => {
      requests.push(await request.json() as QueryRequest);
      return HttpResponse.json(v2QueryPageFixture);
    }));
    renderPage();

    expect(await screen.findByText("example.test.")).toBeInTheDocument();
    expect(screen.getByRole("switch", { name: "自动刷新" })).toBeChecked();
    expect(requests[0].page_size).toBe(10);
    await user.click(screen.getByRole("button", { name: "下一页" }));
    await waitFor(() => expect(requests).toHaveLength(2));
    expect(requests[1]).toMatchObject({ cursor: v2QueryPageFixture.next_cursor, direction: "older" });

    await user.click(screen.getAllByRole("button", { name: /查看 .* 的详情/ })[0]);
    const detail = await screen.findByRole("dialog", { name: "example.test. 解析详情" });
    await user.click(within(detail).getByText("诊断信息"));
    expect(detail).toHaveTextContent("2026-09-07.18");
    expect(detail).toHaveTextContent("clients-12");
  });

  it("规范化主筛选并重置历史 cursor", async () => {
    const user = userEvent.setup();
    const requests: QueryRequest[] = [];
    server.use(http.post("/api/v2/queries/search", async ({ request }) => {
      requests.push(await request.json() as QueryRequest);
      return HttpResponse.json(v2QueryPageFixture);
    }));
    renderPage();
    await screen.findByText("example.test.");
    expect(screen.getByLabelText("域名")).toHaveAttribute("placeholder", "域名关键词");
    expect(screen.getByLabelText("客户端")).toHaveAttribute("placeholder", "客户端名称关键词");
    expect(screen.getByLabelText("请求 IP")).toHaveAttribute("placeholder", "IP 或地址片段");
    await user.type(screen.getByLabelText("域名"), "  filtered.example.  ");
    await user.click(screen.getByRole("button", { name: "查询" }));
    await waitFor(() => expect(requests).toHaveLength(2));
    expect(requests[1].filter.qname).toBe("filtered.example.");
    expect(requests[1].cursor).toBeNull();
  });

  it("触摸点击固定详情，Escape 关闭并恢复触发按钮焦点；恶意 Answer 只作为文本", async () => {
    const user = userEvent.setup();
    const malicious = "<img src=x onerror=alert(1)>";
    const record = { ...direct, answers: { state: "available" as const, total_count: 1,
      records: [{ name: direct.qname, type: "TXT", ttl_seconds: 60, data: malicious }] } };
    server.use(http.post("/api/v2/queries/search", () => HttpResponse.json({ ...v2QueryPageFixture, items: [record] })));
    renderPage();
    const trigger = await screen.findByRole("button", { name: `查看 ${record.qname} 的详情` });
    await user.pointer([{ keys: "[TouchA>]", target: trigger }, { keys: "[/TouchA]", target: trigger }]);
    const detail = await screen.findByRole("dialog", { name: `${record.qname} 解析详情` });
    await user.click(within(detail).getByText("诊断信息"));
    expect(detail).toHaveTextContent(record.id);
    expect(detail).toHaveTextContent(malicious);
    expect(detail.querySelector("img,[onerror]")).toBeNull();
    await user.keyboard("{Escape}");
    await waitFor(() => expect(screen.queryByRole("dialog", { name: `${record.qname} 解析详情` })).not.toBeInTheDocument());
    await waitFor(() => expect(screen.getByRole("button", { name: `查看 ${record.qname} 的详情` })).toHaveFocus());
  });

  it("详情打开时缓冲去重，关闭后才按事件时间应用新记录", async () => {
    const user = userEvent.setup();
    let push: ((batch: QueryBatch) => void) | undefined;
    vi.spyOn(managementEvents, "subscribeQueries").mockImplementation((_options, onData, _onResync, onState) => {
      push = onData;
      onState("open");
      return vi.fn();
    });
    renderPage();
    await screen.findByText("example.test.");
    await user.click(screen.getAllByRole("button", { name: /查看 .* 的详情/ })[0]);
    await screen.findByRole("dialog", { name: "example.test. 解析详情" });
    await waitFor(() => expect(push).toBeDefined());

    const liveRecord = {
      ...direct,
      id: "live-record",
      occurred_at_ms: direct.occurred_at_ms + 1_000,
      qname: "live.example.test.",
    };
    act(() => push?.({
      cursor: { epoch: "stream-1", sequence: "43" },
      directoryRevision: "clients-13",
      items: [liveRecord, liveRecord],
    }));
    expect(await screen.findByText("有 1 条新记录")).toBeInTheDocument();
    expect(screen.queryByText("live.example.test.")).not.toBeInTheDocument();

    server.use(http.post("/api/v2/queries/search", () => HttpResponse.json({
      ...v2QueryPageFixture,
      items: [liveRecord, ...v2QueryPageFixture.items],
      snapshot_cursor: { epoch: "stream-1", sequence: "43" },
    })));
    await user.click(screen.getByRole("button", { name: "查看新记录" }));
    expect(await screen.findByText("live.example.test.")).toBeInTheDocument();
    expect(screen.queryByText("有 1 条新记录")).not.toBeInTheDocument();
    await waitFor(() => expect(screen.queryByRole("dialog", { name: "example.test. 解析详情" })).not.toBeInTheDocument());
  });
});

describe("QueriesPage 身份与来源标签", () => {
  const [direct, cache] = v2QueryRecordsFixture;

  afterEach(() => vi.restoreAllMocks());

  it("客户端列只显示名称，缺失名称时不把历史匹配 ID 当名称", () => {
    const named = formatClientIdentity(direct);
    expect(named.primary).toBe("workstation");
    expect(named.clientIp).toBe("192.0.2.10");
    // 主文本已是当前名称，次文本只保留当时的匹配结论，不重复“当前 workstation”。
    expect(named.detail).toBe("当时按 IP 匹配 Desktop-01");

    const historical = formatClientIdentity({ ...direct, current_client_name: null });
    expect(historical.primary).toBe("未命名客户端");
    expect(historical.clientIp).toBe("192.0.2.10");
    expect(historical.detail).toBe("当时按 IP 匹配 Desktop-01");

    const unmatched = formatClientIdentity(cache);
    expect(unmatched.primary).toBe("未匹配客户端");
    expect(unmatched.clientIp).toBe("192.0.2.20");
    expect(unmatched.detail).toBe("当时未匹配");
  });

  it("来源标签先按缓存结果分类，再回退到 source", () => {
    expect(sourceLabel({ ...direct, cache: "hit", source: "cache" })).toEqual({ label: "命中缓存", color: "green" });
    expect(sourceLabel({ ...direct, cache: "stale", source: "cache" })).toEqual({ label: "乐观缓存", color: "gold" });
    expect(sourceLabel({ ...direct, cache: "expired", source: "upstream" })).toEqual({ label: "缓存过期", color: "orange" });
    expect(sourceLabel({ ...direct, cache: "miss", source: "upstream" })).toEqual({ label: "请求上游", color: "blue" });
    expect(sourceLabel({ ...direct, cache: "bypass", source: "hosts" })).toEqual({ label: "Hosts", color: "purple" });
    expect(sourceLabel({ ...direct, cache: "miss", source: "rule" })).toEqual({ label: "规则", color: "blue" });
    expect(sourceLabel({ ...direct, cache: "bypass", source: "synthetic" })).toEqual({ label: "synthetic", color: "blue" });
  });

  it("缓存状态筛选用分类名称展示，提交值仍是枚举原文", async () => {
    const user = userEvent.setup();
    const requests: QueryRequest[] = [];
    server.use(http.post("/api/v2/queries/search", async ({ request }) => {
      requests.push(await request.json() as QueryRequest);
      return HttpResponse.json(v2QueryPageFixture);
    }));
    renderPage();
    await screen.findByText("example.test.");
    await user.click(screen.getByRole("button", { name: "高级筛选" }));
    // antd Select 在 jsdom 中按 mousedown 展开，选项文本即分类名称。
    fireEvent.mouseDown(screen.getByLabelText("缓存状态"));
    for (const label of ["命中缓存", "乐观缓存", "缓存过期", "未命中", "未启用"]) {
      expect(await screen.findByTitle(label)).toBeInTheDocument();
    }
    fireEvent.click(screen.getByTitle("乐观缓存"));
    expect(screen.getByLabelText("缓存状态").closest(".ant-select")).toHaveTextContent("乐观缓存");
    await user.click(screen.getByRole("button", { name: "查询" }));
    await waitFor(() => expect(requests).toHaveLength(2));
    expect(requests[1].filter.cache).toBe("stale");
  });

  it("客户端列仅保留名称与 IP，列表只保留响应耗时", async () => {
    const restoreMatchMedia = openAllBreakpoints();
    const record = { ...direct, identity: { client_id: "raw-client-id", client_ip: "192.0.2.55" } };
    server.use(http.post("/api/v2/queries/search", () => HttpResponse.json({ ...v2QueryPageFixture, items: [record] })));
    try {
      renderPage();
      const identity = await screen.findByText("workstation");
      expect(screen.getAllByRole("columnheader").map((cell) => cell.textContent)).toEqual(["时间", "请求", "结果", "响应耗时", "路由", "客户端"]);
      const row = identity.closest("tr");
      expect(row).not.toBeNull();
      expect(row).toHaveTextContent("192.0.2.55");
      expect(row).not.toHaveTextContent("raw-client-id");
      expect(row).not.toHaveTextContent("Desktop-01");
      expect(row).not.toHaveTextContent("总耗时");
      expect(row).not.toHaveTextContent("主链耗时");
      expect(within(row as HTMLElement).getByLabelText("响应耗时 0.18 ms")).toHaveTextContent("0.18 ms");
    } finally {
      restoreMatchMedia();
    }
  });

  it("固定 ECS 与目录快照不受同 ID refetch 或 WS 新记录替换", async () => {
    const user = userEvent.setup();
    const initial: QueryRecord = { ...direct, ecs: {
      request: { state: "absent" }, upstream: { state: "observed", query: { state: "present", cidr: "203.0.113.0/24", scope_prefix: 0 }, response: { state: "absent" }, policy: { source: "global", mode: "client", value_source: "client_ip" }, upstream_leaf_name: "original-leaf" }, cache_match: null, refresh: null,
    } };
    let current = initial;
    let push: ((batch: QueryBatch) => void) | undefined;
    vi.spyOn(managementEvents, "subscribeQueries").mockImplementation((_options, onData) => { push = onData; return vi.fn(); });
    server.use(http.post("/api/v2/queries/search", () => HttpResponse.json({ ...v2QueryPageFixture, directory_revision: current === initial ? "frozen-directory" : "changed-directory", items: [current] })));
    const { queryClient } = renderPage();
    await user.click(await screen.findByRole("button", { name: "查看 example.test. 的详情" }));
    let detail = await screen.findByRole("dialog");
    expect(within(detail).getByLabelText("ECS 摘要")).toHaveTextContent("203.0.113.0/24");
    current = { ...initial, ecs: null, current_client_name: "changed-client" };
    await act(async () => { await queryClient.invalidateQueries({ queryKey: ["api", "v2", "queries"] }); });
    detail = await screen.findByRole("dialog", { name: "example.test. 解析详情" });
    server.use(http.post("/api/v2/queries/search", () => HttpResponse.json({ ...v2QueryPageFixture, items: [] })));
    await act(async () => { await queryClient.invalidateQueries({ queryKey: ["api", "v2", "queries"] }); });
    await waitFor(() => expect(screen.getByRole("dialog", { name: "example.test. 解析详情" })).toBeVisible());
    detail = screen.getByRole("dialog", { name: "example.test. 解析详情" });
    expect(screen.getByRole("button", { name: "查看 example.test. 的详情" })).toBeInTheDocument();
    act(() => push?.({ cursor: { epoch: "stream-1", sequence: "44" }, directoryRevision: "ws-directory", items: [{ ...current, id: "different-record", qname: "new.example." }] }));
    expect(within(detail).getByLabelText("ECS 摘要")).toHaveTextContent("203.0.113.0/24");
    await user.click(within(detail).getByText("诊断信息"));
    expect(detail).toHaveTextContent("frozen-directory");
    expect(detail).not.toHaveTextContent("changed-directory");
    expect(detail).toHaveTextContent("original-leaf");
    await user.keyboard("{Escape}");
  });

  it("高级筛选全部保留，页大小变化清除历史 cursor 与详情", async () => {
    const user = userEvent.setup();
    const requests: QueryRequest[] = [];
    server.use(http.post("/api/v2/queries/search", async ({ request }) => { requests.push(await request.json() as QueryRequest); return HttpResponse.json(v2QueryPageFixture); }));
    renderPage();
    await screen.findByText("example.test.");
    await user.click(screen.getByRole("button", { name: "高级筛选" }));
    for (const label of ["原始 ID", "历史匹配 ID", "QTYPE", "RCODE", "结果状态", "缓存状态", "排序字段", "排序方向"]) expect(screen.getByLabelText(label)).toBeInTheDocument();
    await user.type(screen.getByLabelText("原始 ID"), " original ");
    await user.type(screen.getByLabelText("历史匹配 ID"), " historical ");
    await user.type(screen.getByLabelText("QTYPE"), " AAAA ");
    await user.click(screen.getByRole("button", { name: "查询" }));
    await waitFor(() => expect(requests.at(-1)?.filter).toMatchObject({ client_id: "original", matched_client_id: "historical", qtype: "AAAA" }));
    await user.click(screen.getByRole("button", { name: "下一页" }));
    await waitFor(() => expect(requests.at(-1)?.cursor).toBe(v2QueryPageFixture.next_cursor));
    await user.click(screen.getByRole("button", { name: "查看 example.test. 的详情" }));
    await user.keyboard("{Escape}");
    fireEvent.mouseDown(screen.getByLabelText("每页条数"));
    fireEvent.click(await screen.findByTitle("50 条/页"));
    await waitFor(() => expect(requests.at(-1)).toMatchObject({ page_size: 50, cursor: null }));
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  });

  it("UTC 格式不受本地偏移影响且无效时间不伪造", () => {
    expect(formatQueryTime(Date.parse("2026-11-09T08:01:02+08:00"))).toEqual({ date: "2026-11-09", time: "00:01:02" });
    expect(formatQueryTime(NaN)).toEqual({ date: "—", time: "—" });
  });

  it("显式固定、复制与关闭不会丢失焦点，复制失败可手工选择", async () => {
    const user = userEvent.setup();
    renderPage();
    await user.click(await screen.findByRole("button", { name: "查看 example.test. 的详情" }));
    const detail = await screen.findByRole("dialog");
    const clipboard = vi.spyOn(navigator.clipboard, "writeText").mockResolvedValue();
    await user.click(within(detail).getByRole("button", { name: "复制域名" }));
    expect(clipboard).toHaveBeenCalledWith("example.test.");
    expect(await within(detail).findByRole("status")).toHaveTextContent("域名已复制");
    clipboard.mockRejectedValueOnce(new Error("denied"));
    await user.click(within(detail).getByRole("button", { name: "复制域名" }));
    expect(await within(detail).findByRole("status")).toHaveTextContent("请选择域名手工复制");
    await user.click(within(detail).getByRole("button", { name: "取消固定" }));
    expect(within(detail).getByRole("button", { name: "固定详情" })).toHaveAttribute("aria-pressed", "false");
    await user.click(within(detail).getByRole("button", { name: "固定详情" }));
    await user.click(within(detail).getByRole("button", { name: "关闭详情" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
    await waitFor(() => expect(screen.getByRole("button", { name: "查看 example.test. 的详情" })).toHaveFocus());
  });

  it("窄屏卡片使用底部 Drawer，Esc 关闭并恢复同一记录焦点", async () => {
    vi.spyOn(window, "matchMedia").mockImplementation((query) => ({
      matches: query.includes("max-width: 760px"), media: query, onchange: null,
      addListener: () => {}, removeListener: () => {}, addEventListener: () => {}, removeEventListener: () => {}, dispatchEvent: () => false,
    }));
    const user = userEvent.setup();
    renderPage();
    const trigger = await screen.findByRole("button", { name: "查看 example.test. 的详情" });
    expect(trigger.closest("article")).toHaveTextContent("响应耗时 0.18 ms");
    expect(screen.queryByRole("table")).not.toBeInTheDocument();
    await user.click(trigger);
    const drawer = await screen.findByRole("dialog");
    expect(drawer).toHaveAccessibleName("example.test. 解析详情");
    expect(drawer.closest(".ant-drawer")).toHaveClass("ant-drawer-bottom");
    await user.click(within(drawer).getByText("诊断信息"));
    expect(within(drawer).getByText("目录快照")).toBeInTheDocument();
    await user.keyboard("{Escape}");
    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
    await waitFor(() => expect(screen.getByRole("button", { name: "查看 example.test. 的详情" })).toHaveFocus());
  });
});

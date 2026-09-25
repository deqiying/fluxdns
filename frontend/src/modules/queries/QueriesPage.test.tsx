import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import { http, HttpResponse } from "msw";
import { server } from "@/mocks/server";
import { sessionFixture, v2QueryPageFixture, v2QueryRecordsFixture } from "@/mocks/fixtures";
import { setMockAuthenticated } from "@/mocks/handlers";
import { acceptAuthSession } from "@/shared/api/client";
import { managementEvents, type QueryBatch } from "@/shared/api/events";
import type { QueryRequest } from "./api";
import { formatClientIdentity, formatDurationSummary, formatResponseSummary, formatRoute, QueriesPage, sourceLabel } from "./QueriesPage";

function renderPage() {
  setMockAuthenticated(true);
  acceptAuthSession({
    session: sessionFixture,
    access_token: "A".repeat(43),
    token_type: "Bearer",
    access_expires_at_ms: Date.now() + 300_000,
  });
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(<QueryClientProvider client={queryClient}><QueriesPage /></QueryClientProvider>);
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
      matches: true,
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
    expect(detail).toHaveTextContent("总耗时");
    expect(detail).toHaveTextContent("主链耗时");
    expect(detail).toHaveTextContent("响应耗时");
    await user.click(within(detail).getByText("总耗时"));
    expect(detail).toBeInTheDocument();
    await user.click(second);
    expect(await screen.findByRole("dialog", { name: "cached.example.test. 解析详情" })).toHaveTextContent("后台刷新");
    expect(screen.getAllByRole("dialog")).toHaveLength(1);
    await user.click(screen.getByRole("heading", { name: "解析记录" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
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
    await user.click(screen.getByRole("button", { name: "下一页" }));
    await waitFor(() => expect(requests).toHaveLength(2));
    expect(requests[1]).toMatchObject({ cursor: v2QueryPageFixture.next_cursor, direction: "older" });

    await user.click(screen.getAllByRole("button", { name: /查看 .* 的详情/ })[0]);
    const detail = await screen.findByRole("dialog", { name: "example.test. 解析详情" });
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
      expect(screen.getAllByRole("columnheader").map((cell) => cell.textContent)).toEqual(["时间", "请求", "结果", "路由", "客户端"]);
      const row = identity.closest("tr");
      expect(row).not.toBeNull();
      expect(row).toHaveTextContent("192.0.2.55");
      expect(row).not.toHaveTextContent("raw-client-id");
      expect(row).not.toHaveTextContent("Desktop-01");
      expect(row).not.toHaveTextContent("总耗时");
      expect(row).not.toHaveTextContent("主链耗时");
      expect(row).toHaveTextContent("响应耗时 0.18 ms");
    } finally {
      restoreMatchMedia();
    }
  });
});

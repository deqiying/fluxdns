import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, render, screen, waitFor } from "@testing-library/react";
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

  it("区分 cache producer、direct 与两种耗时", () => {
    expect(formatRoute(cache)).toBe("缓存生产：public → public-2");
    expect(formatRoute(direct)).toBe("public → public-1");
    expect(formatDurationSummary(direct)).toEqual({ total: "总耗时 0.12 ms", dnsCore: "主链 0.1 ms" });
  });

  it("显示首条 Answer 与截断总数", () => {
    expect(formatResponseSummary(direct)).toEqual({ primary: "A  192.0.2.1", meta: "1 条结果" });
    expect(formatResponseSummary(cache)).toEqual({ primary: "NOERROR · answered", meta: "保留 0 条，共 20 条" });
  });

  it("默认关闭实时更新，以 opaque cursor 翻页并按稳定 ID 打开详情", async () => {
    const user = userEvent.setup();
    const requests: QueryRequest[] = [];
    server.use(http.post("/api/v2/queries/search", async ({ request }) => {
      requests.push(await request.json() as QueryRequest);
      return HttpResponse.json(v2QueryPageFixture);
    }));
    renderPage();

    expect(await screen.findByText("example.test.")).toBeInTheDocument();
    expect(screen.getByText("已关闭")).toBeInTheDocument();
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
    await user.click(screen.getByRole("switch", { name: "自动刷新" }));
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

  it("身份列优先当前客户端名称，缺失时回退历史匹配 ID，再回退未匹配占位", () => {
    const named = formatClientIdentity(direct);
    expect(named.primary).toBe("workstation");
    expect(named.clientIp).toBe("192.0.2.10");
    expect(named.detail).toContain("当时按 IP 匹配 Desktop-01");
    expect(named.detail).toContain("当前 workstation");

    const historical = formatClientIdentity({ ...direct, current_client_name: null });
    expect(historical.primary).toBe("Desktop-01");
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
    expect(sourceLabel({ ...direct, cache: "bypass", source: "hosts" })).toEqual({ label: "hosts", color: "purple" });
    expect(sourceLabel({ ...direct, cache: "miss", source: "rule" })).toEqual({ label: "规则", color: "blue" });
    expect(sourceLabel({ ...direct, cache: "bypass", source: "synthetic" })).toEqual({ label: "synthetic", color: "blue" });
  });

  it("按 时间/请求/结果/路由/身份 渲染，身份列展示客户端名称与客户端 IP", async () => {
    const restoreMatchMedia = openAllBreakpoints();
    const record = { ...direct, identity: { client_id: "raw-client-id", client_ip: "192.0.2.55" } };
    server.use(http.post("/api/v2/queries/search", () => HttpResponse.json({ ...v2QueryPageFixture, items: [record] })));
    try {
      renderPage();
      const identity = await screen.findByText("workstation");
      expect(screen.getAllByRole("columnheader").map((cell) => cell.textContent)).toEqual(["时间", "请求", "结果", "路由", "身份"]);
      const row = identity.closest("tr");
      expect(row).not.toBeNull();
      expect(row).toHaveTextContent("192.0.2.55");
      expect(row).not.toHaveTextContent("raw-client-id");
    } finally {
      restoreMatchMedia();
    }
  });
});

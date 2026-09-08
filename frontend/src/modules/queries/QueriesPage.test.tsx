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
import { formatClient, formatDurationSummary, formatResponseSummary, formatRoute, QueriesPage } from "./QueriesPage";

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

describe("QueriesPage 展示语义", () => {
  const [direct, cache] = v2QueryRecordsFixture;

  afterEach(() => vi.restoreAllMocks());

  it("区分 cache producer、direct、原始身份和两种耗时", () => {
    expect(formatRoute(cache)).toBe("缓存生产：public → public-2");
    expect(formatRoute(direct)).toBe("public → public-1");
    expect(formatClient(direct)).toEqual({ primary: "unknown-id", secondary: "192.0.2.10", muted: false });
    expect(formatClient(cache)).toEqual({ primary: "未传入原始 ID", secondary: "192.0.2.20", muted: true });
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

    await user.keyboard("{Escape}");
    expect(await screen.findByText("live.example.test.")).toBeInTheDocument();
    expect(screen.queryByText("有 1 条新记录")).not.toBeInTheDocument();
  });
});

import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { http, HttpResponse } from "msw";
import { describe, expect, it } from "vitest";
import { setMockAuthenticated } from "@/mocks/handlers";
import { memoryAnalysisFixture, memoryOsFixture, memorySnapshotFixture } from "@/mocks/memory-fixtures";
import { server } from "@/mocks/server";
import { TimeZoneContext } from "@/shared/time/TimeZoneContext";
import { MemoryAnalysisCard } from "./MemoryAnalysisCard";
import { memoryAnalysisKey, type MemoryAnalysis } from "./api";

function renderCard(data?: MemoryAnalysis) {
  setMockAuthenticated(true);
  const client = new QueryClient({ defaultOptions: { queries: { retry: false, gcTime: 0 } } });
  if (data) server.use(http.get("/api/v2/system/memory", () => HttpResponse.json(data)));
  const view = render(<QueryClientProvider client={client}><TimeZoneContext.Provider value={{ timeZone: "Asia/Shanghai", fallbackReason: null }}><MemoryAnalysisCard processId={memoryAnalysisFixture.process_id} startedAtMs={memoryAnalysisFixture.started_at_ms} /></TimeZoneContext.Provider></QueryClientProvider>);
  return { ...view, client };
}

describe("MemoryAnalysisCard", () => {
  it("首次只 GET，用户手动采集后更新同一 query cache，计费与字节估算不混淆", async () => {
    const user = userEvent.setup();
    let posts = 0;
    server.use(http.post("/api/v2/system/memory/snapshot", () => {
      posts += 1;
      return HttpResponse.json({ ...memoryAnalysisFixture, snapshot: memorySnapshotFixture });
    }));
    const { client } = renderCard();
    expect(await screen.findByText("尚未采集模块快照")).toBeInTheDocument();
    expect(posts).toBe(0);
    expect(screen.getByRole("img", { name: /^RSS 构成：/ })).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "采集分析快照" }));
    expect(await screen.findByRole("rowheader", { name: /规则索引/ })).toBeInTheDocument();
    expect(posts).toBe(1);
    expect(client.getQueryData<MemoryAnalysis>(memoryAnalysisKey)?.snapshot?.snapshot_id).toBe("1");
    const cache = screen.getByRole("rowheader", { name: "DNS 响应缓存" }).closest("tr")!;
    expect(within(cache).getByText("未采集")).toBeInTheDocument();
    expect(cache).toHaveTextContent("淘汰权重");
    expect(cache).toHaveTextContent("计费字节，非内存");
    expect(cache).toHaveTextContent("预算，非实占");
    const sqlite = screen.getByRole("rowheader", { name: "SQLite 工作内存" }).closest("tr")!;
    expect(sqlite).toHaveTextContent("租约预算：8（计数）");
    expect(sqlite).not.toHaveTextContent("MiB");
    const summary = screen.getByText("展开 2 个唯一索引");
    // jsdom 不模拟 summary 的 Enter 默认动作；验证原生 details 点击展开，不另加自定义键盘行为。
    await user.click(summary);
    expect(summary.closest("details")).toHaveAttribute("open");
    expect(within(summary.closest("details")!).getByText(/保留旧索引/)).toBeVisible();
  });

  it("非原子读数不一致时保留原值，降级而不画构成", async () => {
    renderCard({ ...memoryAnalysisFixture, os: { ...memoryOsFixture, rss_bytes: { state: "available", value: "132120577" } } });
    expect(await screen.findByText(/分项总和与 RSS 不一致/)).toBeInTheDocument();
    expect(screen.queryByRole("img", { name: /^RSS 构成：/ })).not.toBeInTheDocument();
    expect(screen.getByText("108.0 MiB")).toBeInTheDocument();
  });

  it("POST 失败只提交一次，安全错误与 request ID 保留，旧快照不冒充本次成功", async () => {
    let posts = 0;
    server.use(http.post("/api/v2/system/memory/snapshot", () => {
      posts += 1;
      return HttpResponse.json({ code: "SERVICE_UNAVAILABLE", message: "sampling failed", request_id: "memory-safe-503", retryable: true, field_errors: [] }, { status: 503 });
    }));
    renderCard({ ...memoryAnalysisFixture, snapshot: memorySnapshotFixture });
    await screen.findByText("5.0 MiB");
    await userEvent.click(screen.getByRole("button", { name: "采集分析快照" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("请求 ID：memory-safe-503");
    expect(screen.getByRole("alert")).toHaveTextContent("历史成功快照，不代表本次采集成功");
    expect(screen.getByText("5.0 MiB")).toBeInTheDocument();
    expect(posts).toBe(1);
  });

  it("服务端 failed 返回旧 snapshot 也明确失败", async () => {
    renderCard({ ...memoryAnalysisFixture, snapshot: memorySnapshotFixture, collection_state: "failed", last_failure: "runtime_changed" });
    expect(await screen.findByRole("alert")).toHaveTextContent("runtime_changed");
    expect(screen.getByRole("alert")).toHaveTextContent("不代表本次采集成功");
    expect(screen.getByText("失败", { selector: ".memory-status" })).toBeInTheDocument();
  });

  it("支持箭头与 Home/End tab 导航，趋势使用服务器时区与真实样本", async () => {
    renderCard();
    const modules = await screen.findByRole("tab", { name: "模块明细" });
    modules.focus();
    fireEvent.keyDown(modules, { key: "ArrowRight" });
    const trend = screen.getByRole("tab", { name: "变化趋势" });
    expect(trend).toHaveFocus();
    expect(trend).toHaveAttribute("aria-selected", "true");
    expect(screen.getByRole("tabpanel", { name: "变化趋势" })).toHaveTextContent("4 个真实样本");
    expect(screen.getByRole("tabpanel", { name: "变化趋势" })).toHaveTextContent("Asia/Shanghai");
    expect(screen.getByRole("img", { name: /进程内存变化趋势/ })).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "1 小时" }));
    expect(screen.getByRole("button", { name: "1 小时" })).toHaveAttribute("aria-pressed", "true");
    fireEvent.keyDown(trend, { key: "End" });
    expect(screen.getByRole("tab", { name: "采样口径" })).toHaveFocus();
    fireEvent.keyDown(screen.getByRole("tab", { name: "采样口径" }), { key: "Home" });
    expect(modules).toHaveFocus();
  });

  it("当前进程身份不符时不展示旧快照和增量", async () => {
    renderCard({ ...memoryAnalysisFixture, started_at_ms: memoryAnalysisFixture.started_at_ms - 1, snapshot: memorySnapshotFixture, previous_snapshot: { ...memorySnapshotFixture, snapshot_id: "0" } });
    expect(await screen.findByText(/进程已变化/)).toBeInTheDocument();
    expect(screen.queryByRole("table")).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "采集分析快照" })).toBeDisabled();
  });

  it("部分不支持与全部不支持保持原因，未知 counter 安全显示原 key", async () => {
    const unavailable = { state: "unavailable", reason: "unsupported", observed_seconds: null } as const;
    const os = Object.fromEntries(Object.keys(memoryOsFixture).map((key) => [key, key === "sampled_at_ms" ? memoryOsFixture.sampled_at_ms : unavailable])) as unknown as MemoryAnalysis["os"];
    const snapshot = structuredClone(memorySnapshotFixture);
    snapshot.modules[0].counters.push({ key: "future_counter", value: "9007199254740993", kind: "count" });
    renderCard({ ...memoryAnalysisFixture, os, snapshot });
    expect(await screen.findByText("不支持", { selector: ".memory-status" })).toBeInTheDocument();
    expect(screen.queryByRole("img", { name: /^RSS 构成：/ })).not.toBeInTheDocument();
    expect(screen.getByRole("table")).toHaveTextContent("future_counter：9,007,199,254,740,993");
    expect(screen.getAllByText(/不支持（unsupported）/).length).toBeGreaterThan(0);
  });

  it("GET 失败单独提供读取重试，不触发扫描", async () => {
    server.use(http.get("/api/v2/system/memory", () => HttpResponse.json({ code: "SERVICE_UNAVAILABLE", message: "sample unavailable", request_id: "memory-read-503", retryable: false, field_errors: [] }, { status: 503 })));
    renderCard();
    expect(await screen.findByRole("alert")).toHaveTextContent("memory-read-503");
    expect(screen.getByRole("button", { name: "重试读取" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "采集分析快照" })).toBeDisabled();
  });

  it("缺失字节不填零，busy 独立呈现", async () => {
    const snapshot = structuredClone(memorySnapshotFixture);
    snapshot.modules[0].state = "busy";
    snapshot.modules[0].estimated_bytes = null;
    renderCard({ ...memoryAnalysisFixture, snapshot });
    expect(await screen.findByText("忙碌，暂不可用")).toBeInTheDocument();
    expect(screen.getByText("部分不可用", { selector: ".memory-status" })).toBeInTheDocument();
    expect(within(screen.getByRole("rowheader", { name: /规则索引/ }).closest("tr")!).getByText("不可比")).toBeInTheDocument();
    await waitFor(() => expect(screen.queryByText("5.0 MiB")).not.toBeInTheDocument());
  });
});

import { fireEvent, render, screen, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { serviceMetricsFixture } from "@/mocks/fixtures";
import { DashboardPage } from "./DashboardPage";
import { useServiceMetrics } from "./hooks";

vi.mock("./hooks", () => ({ useServiceMetrics: vi.fn() }));

function mockMetrics(overrides: Partial<ReturnType<typeof useServiceMetrics>> = {}) {
  vi.mocked(useServiceMetrics).mockReturnValue({
    data: serviceMetricsFixture, connectionState: "open", stale: false, isLoading: false, error: null,
    refetch: vi.fn(), ...overrides,
  } as ReturnType<typeof useServiceMetrics>);
}

describe("DashboardPage", () => {
  beforeEach(() => mockMetrics());

  it("连接打开但快照过期时显示延迟，断开与重连不显示正常", () => {
    const { rerender } = render(<DashboardPage />);
    expect(screen.getByRole("status")).toHaveTextContent("实时连接正常");
    mockMetrics({ stale: true });
    rerender(<DashboardPage />);
    expect(screen.getByRole("status")).toHaveTextContent("指标更新延迟");
    expect(screen.getByRole("alert")).toHaveTextContent("最后一次有效快照");
    mockMetrics({ connectionState: "closed" });
    rerender(<DashboardPage />);
    expect(screen.getByRole("status")).toHaveTextContent("实时连接中断");
    mockMetrics({ connectionState: "reconnecting" });
    rerender(<DashboardPage />);
    expect(screen.getByRole("status")).toHaveTextContent("正在重新连接");
  });

  it("卡片保留不可用原因，不显示单位或零值", () => {
    mockMetrics({ data: { ...serviceMetricsFixture, rss_bytes: { state: "unavailable", reason: "sampling_failed", observed_seconds: null } } });
    render(<DashboardPage />);
    const memory = screen.getByRole("group", { name: "当前内存" });
    expect(within(memory).getByText("采样失败")).toBeInTheDocument();
    expect(within(memory).queryByText("MB")).not.toBeInTheDocument();
    expect(within(memory).queryByText("0")).not.toBeInTheDocument();
  });

  it("深色切换保留指标与图表，重新挂载恢复浅色", () => {
    const { container, unmount } = render(<DashboardPage />);
    const chart = screen.getByRole("img", { name: /QPS 与 RPM 请求趋势/ });
    fireEvent.click(screen.getByRole("button", { name: "深色样例" }));
    expect(screen.getByRole("button", { name: "浅色显示" })).toHaveAttribute("aria-pressed", "true");
    expect(container.querySelector(".service-status-preview-dark")).not.toBeNull();
    expect(screen.getByRole("img", { name: /QPS 与 RPM 请求趋势/ })).toBe(chart);
    expect(screen.getByRole("group", { name: "当前内存" })).toHaveTextContent("186.4 MB");
    unmount();
    render(<DashboardPage />);
    expect(screen.getByRole("button", { name: "深色样例" })).toHaveAttribute("aria-pressed", "false");
  });

  it("CPU 卡片按进程百分比显示，不可用时保留原因和单位口径", () => {
    const { rerender } = render(<DashboardPage />);
    const cpu = screen.getByRole("group", { name: "CPU 占用" });
    expect(within(cpu).getByText("1.25")).toBeInTheDocument();
    expect(within(cpu).getByText("%")).toBeInTheDocument();

    mockMetrics({ data: { ...serviceMetricsFixture, cpu_percent: { state: "unavailable", reason: "sampling_failed", observed_seconds: null } } });
    rerender(<DashboardPage />);
    const unavailable = screen.getByRole("group", { name: "CPU 占用" });
    expect(within(unavailable).getByText("采样失败")).toBeInTheDocument();
    expect(within(unavailable).queryByText("%")).not.toBeInTheDocument();
  });

  it("页面缓存最近 120 秒 QPS，快照只含最新秒点时仍能算出逐秒 RPM", () => {
    const sampled = Date.parse("2026-09-22T13:16:15Z");
    const perSecond = (from: number, count: number) => Array.from({ length: count }, (_, index) => ({
      at_ms: from + index * 1_000,
      value: { state: "available" as const, value: 2 },
    }));
    mockMetrics({ data: { ...serviceMetricsFixture, sampled_at_ms: sampled, qps_trend: perSecond(sampled - 120_000, 120) } });
    const { rerender } = render(<DashboardPage />);
    expect(screen.getByRole("group", { name: /^13:16:14 UTC/ })).toHaveAttribute("aria-label", "13:16:14 UTC，QPS 2，RPM 120");

    // 快照只带同一秒点网格上的最新 30 秒，其余 60 秒窗口由本地缓存的 120 秒补齐。
    mockMetrics({ data: { ...serviceMetricsFixture, sampled_at_ms: sampled + 30_000, qps_trend: perSecond(sampled, 30) } });
    rerender(<DashboardPage />);
    expect(screen.getByRole("group", { name: /^13:16:44 UTC/ })).toHaveAttribute("aria-label", "13:16:44 UTC，QPS 2，RPM 120");
  });

  it("快照自带窗口起点前 60 秒前瞻时，窗口最左一秒也有 RPM", () => {
    const sampled = Date.parse("2026-09-22T13:16:15Z");
    const perSecond = (from: number, count: number) => Array.from({ length: count }, (_, index) => ({
      at_ms: from + index * 1_000,
      value: { state: "available" as const, value: 2 },
    }));
    mockMetrics({ data: { ...serviceMetricsFixture, sampled_at_ms: sampled, qps_trend: perSecond(sampled - 660_000, 660) } });
    render(<DashboardPage />);
    const chart = screen.getByRole("group", { name: /^13:16:14 UTC/ });
    fireEvent.keyDown(chart, { key: "Home" });
    expect(chart).toHaveAttribute("aria-label", "13:06:15 UTC，QPS 2，RPM 120");
  });

  it("速率卡片主值取窗口内原始计数，右下角显示近 600 秒平均值", () => {
    const sampled = Date.parse("2026-09-22T13:16:15Z");
    const perSecond = (from: number, count: number) => Array.from({ length: count }, (_, index) => ({
      at_ms: from + index * 1_000,
      value: { state: "available" as const, value: 3 },
    }));
    mockMetrics({
      data: { ...serviceMetricsFixture, sampled_at_ms: sampled, rpm: { state: "available", value: 600 }, qps_trend: perSecond(sampled - 660_000, 660) },
    });
    render(<DashboardPage />);

    // 主值是末位逐秒桶的原始计数，不是近 60 秒均值；平均值才除以窗口长度（600 ÷ 60 = 10）。
    const qps = screen.getByRole("group", { name: "实时 QPS" });
    expect(within(qps).getByText("3")).toBeInTheDocument();
    expect(within(qps).getByText("请求/秒")).toBeInTheDocument();
    expect(within(qps).getByText("最近一秒")).toBeInTheDocument();
    expect(qps).toHaveTextContent("平均 QPS 10");

    // RPM 主值是过去 60 秒请求数（60 × 3 = 180），平均值直接取近 600 秒的每分钟请求数。
    const rpm = screen.getByRole("group", { name: "实时 RPM" });
    expect(within(rpm).getByText("180")).toBeInTheDocument();
    expect(within(rpm).getByText("请求/分钟")).toBeInTheDocument();
    expect(within(rpm).getByText("过去一分钟")).toBeInTheDocument();
    expect(rpm).toHaveTextContent("平均 RPM 600");
  });

  it("平均值独立降级：rpm 暖机时主值仍给实时计数，右下角保留原因", () => {
    const sampled = Date.parse("2026-09-22T13:16:15Z");
    const perSecond = Array.from({ length: 660 }, (_, index) => ({
      at_ms: sampled - (660 - index) * 1_000,
      value: { state: "available" as const, value: 3 },
    }));
    mockMetrics({
      data: { ...serviceMetricsFixture, sampled_at_ms: sampled, rpm: { state: "unavailable", reason: "warmup", observed_seconds: 42 }, qps_trend: perSecond },
    });
    render(<DashboardPage />);

    const qps = screen.getByRole("group", { name: "实时 QPS" });
    expect(within(qps).getByText("3")).toBeInTheDocument();
    expect(qps).toHaveTextContent("平均 QPS 暖机中 · 42s");
    expect(within(qps).queryByText("10")).not.toBeInTheDocument();
  });

  it("没有逐秒样本时速率卡片按暖机降级，不显示零值", () => {
    mockMetrics({ data: { ...serviceMetricsFixture, qps_trend: [] } });
    render(<DashboardPage />);

    const qps = screen.getByRole("group", { name: "实时 QPS" });
    expect(within(qps).getByText("暖机中")).toBeInTheDocument();
    expect(within(qps).queryByText("0")).not.toBeInTheDocument();
    expect(screen.getByRole("group", { name: "实时 RPM" })).toHaveTextContent("暂不可用");
  });
});

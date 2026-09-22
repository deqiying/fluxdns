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
    expect(within(memory).queryByText("MiB")).not.toBeInTheDocument();
    expect(within(memory).queryByText("0")).not.toBeInTheDocument();
  });

  it("深色切换保留指标与图表，重新挂载恢复浅色", () => {
    const { container, unmount } = render(<DashboardPage />);
    const chart = screen.getByRole("img", { name: /QPS 与 RPM 请求趋势/ });
    fireEvent.click(screen.getByRole("button", { name: "深色样例" }));
    expect(screen.getByRole("button", { name: "浅色显示" })).toHaveAttribute("aria-pressed", "true");
    expect(container.querySelector(".service-status-preview-dark")).not.toBeNull();
    expect(screen.getByRole("img", { name: /QPS 与 RPM 请求趋势/ })).toBe(chart);
    expect(screen.getByRole("group", { name: "当前内存" })).toHaveTextContent("186.4 MiB");
    unmount();
    render(<DashboardPage />);
    expect(screen.getByRole("button", { name: "深色样例" })).toHaveAttribute("aria-pressed", "false");
  });
});

import { act, fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { serviceMetricsFixture } from "@/mocks/fixtures";
import type { ServiceMetrics } from "./api";
import { MetricsTrendChart } from "./MetricsTrendChart";

const sampledAt = Date.parse("2026-09-22T13:16:15Z");
const sample = (offset: number, value: number): ServiceMetrics["qps_trend"][number] => ({
  at_ms: sampledAt + offset,
  value: { state: "available", value },
});
const metrics: ServiceMetrics = {
  ...serviceMetricsFixture,
  sampled_at_ms: sampledAt,
  qps_trend: [sample(-600_000, 2), { at_ms: sampledAt - 300_000, value: { state: "unavailable", reason: "observation_gap", observed_seconds: null } }, sample(0, 3)],
  rpm_trend: [sample(-600_000, 60), sample(0, 121)],
};

describe("MetricsTrendChart", () => {
  it("保留双折线缺口与孤立样本，时间和图例明确使用 UTC 及左右轴", () => {
    const { container } = render(<MetricsTrendChart metrics={metrics} />);
    expect(screen.getByRole("img", { name: /QPS 与 RPM 请求趋势/ })).toBeInTheDocument();
    expect(screen.getByText("请求/秒 · 左轴")).toBeInTheDocument();
    expect(screen.getByText("请求/分钟 · 右轴")).toBeInTheDocument();
    expect(screen.getByRole("group")).toHaveAttribute("aria-label", "13:16:15 UTC，QPS 3，RPM 121");
    const qpsPaths = container.querySelectorAll(".metrics-chart-qps-line");
    expect(container.querySelector(".metrics-chart-tooltip")).toBeNull();
    expect(qpsPaths).toHaveLength(2);
    for (const path of qpsPaths) expect(path.getAttribute("d")).toMatch(/^M (.+) L \1$/);
    expect(container.querySelectorAll(".metrics-chart-rpm-line")).toHaveLength(1);
    expect(container.querySelector(".metrics-chart-qps-area")).toBeNull();
  });

  it("键盘选中缺数点时不伪造零值，并能返回最新点", () => {
    const { container } = render(<MetricsTrendChart metrics={metrics} />);
    const chart = screen.getByRole("group");
    fireEvent.keyDown(chart, { key: "ArrowLeft" });
    expect(chart).toHaveAttribute("aria-label", expect.stringContaining("13:11:15 UTC，QPS 暂不可用"));
    expect(container.querySelector(".metrics-chart-point-qps")).toBeNull();
    fireEvent.keyDown(chart, { key: "Home" });
    expect(chart).toHaveAttribute("aria-label", "13:06:15 UTC，QPS 2，RPM 60");
    fireEvent.keyDown(chart, { key: "End" });
    expect(chart).toHaveAttribute("aria-label", "13:16:15 UTC，QPS 3，RPM 121");
    expect(container.querySelector(".metrics-chart-tooltip")).not.toBeNull();
    fireEvent.blur(chart);
    expect(container.querySelector(".metrics-chart-tooltip")).toBeNull();
  });

  it("窗口外的高值不影响当前刻度、可选样本或提示", () => {
    const { container } = render(<MetricsTrendChart metrics={{ ...metrics, qps_trend: [sample(-700_000, 99999), sample(-600_000, 2), sample(0, 3)], rpm_trend: [sample(-700_000, 99999)] }} />);
    expect(container.querySelector(".metrics-chart-axis-qps")).toHaveTextContent("4");
    fireEvent.keyDown(screen.getByRole("group"), { key: "Home" });
    expect(screen.getByRole("group")).toHaveAttribute("aria-label", "13:06:15 UTC，QPS 2，RPM 暂不可用");
  });

  it("空趋势与全部不可用都有明确提示，不制造折线", () => {
    const { container, rerender } = render(<MetricsTrendChart metrics={{ ...metrics, qps_trend: [], rpm_trend: [] }} />);
    expect(screen.getByText("最近十分钟暂无趋势样本")).toBeInTheDocument();
    expect(container.querySelector(".metrics-chart-tooltip")).toBeNull();
    rerender(<MetricsTrendChart metrics={{ ...metrics, qps_trend: [metrics.qps_trend[1]], rpm_trend: [] }} />);
    expect(screen.getByText("趋势样本暂不可用，等待有效采样")).toBeInTheDocument();
    expect(container.querySelectorAll(".metrics-chart-qps-line, .metrics-chart-rpm-line")).toHaveLength(0);
  });

  it("容器变窄后指针仍按同一时间轴选择，tooltip 不越过右边界", () => {
    let resize!: ResizeObserverCallback;
    const original = globalThis.ResizeObserver;
    vi.stubGlobal("ResizeObserver", class {
      constructor(callback: ResizeObserverCallback) { resize = callback; }
      observe() {}
      disconnect() {}
    });
    try {
      const { container } = render(<MetricsTrendChart metrics={metrics} />);
      act(() => resize([{ contentRect: { width: 300 } } as ResizeObserverEntry], {} as ResizeObserver));
      const chart = screen.getByRole("img", { name: /QPS 与 RPM 请求趋势/ });
      expect(chart).toHaveAttribute("viewBox", "0 0 300 252");
      vi.spyOn(chart, "getBoundingClientRect").mockReturnValue({ left: 0, width: 300 } as DOMRect);
      fireEvent.pointerDown(chart, { clientX: 42 + (300 - 42 - 48) / 2 });
      expect(screen.getByRole("group")).toHaveAttribute("aria-label", expect.stringContaining("13:11:15 UTC，QPS 暂不可用"));
      fireEvent.pointerMove(chart, { clientX: 300 });
      expect(screen.getByRole("group")).toHaveAttribute("aria-label", "13:16:15 UTC，QPS 3，RPM 121");
      expect(container.querySelector(".metrics-chart-tooltip")).toHaveStyle({ left: "200px" });
    } finally {
      vi.stubGlobal("ResizeObserver", original);
    }
  });
});

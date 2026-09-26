import { act, fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { serviceMetricsFixture } from "@/mocks/fixtures";
import type { ServiceMetrics } from "./api";
import { MetricsTrendChart } from "./MetricsTrendChart";
import { RPM_WINDOW_SECONDS, TREND_WINDOW_MS, rollingRpm, type RateSample, type RateTrend } from "./rateTrend";

const sampledAt = Date.parse("2026-09-22T13:16:15Z");
const startAt = sampledAt - TREND_WINDOW_MS;
const WIDTH = 1000;
const GAP_INDEX = 300;

const metrics: ServiceMetrics = { ...serviceMetricsFixture, sampled_at_ms: sampledAt };

/** 逐秒 QPS 覆盖整个窗口，缺口秒返回不可用，用于验证不插值与 RPM 窗口完整性。 */
function qpsSeries(gapIndex: number | undefined, extra: RateSample[] = []): RateSample[] {
  const series = Array.from({ length: 600 }, (_, index): RateSample => ({
    at_ms: startAt + index * 1_000,
    value: index === gapIndex
      ? { state: "unavailable", reason: "observation_gap", observed_seconds: null }
      : { state: "available", value: 2 },
  }));
  return [...extra, ...series].sort((left, right) => left.at_ms - right.at_ms);
}

function rateTrend(gapIndex: number | undefined = GAP_INDEX, extra: RateSample[] = []): RateTrend {
  const qps = qpsSeries(gapIndex, extra);
  return { qps, rpm: rollingRpm(qps, startAt, sampledAt) };
}

/** 折线顶点数，等于该段上的逐秒采样点数量。 */
function vertices(path: Element): number {
  return (path.getAttribute("d") ?? "").split(" L ").length;
}

/** 横轴时间标签；左右轴的数值标签带专属类名，需要排除。 */
function tickLabels(container: HTMLElement): (string | null)[] {
  return [...container.querySelectorAll("text.metrics-chart-axis:not(.metrics-chart-axis-qps):not(.metrics-chart-axis-rpm)")]
    .map((node) => node.textContent);
}

describe("MetricsTrendChart", () => {
  it("QPS 每秒一个点，RPM 为过去 60 秒请求数并在缺口处断开", () => {
    const { container } = render(<MetricsTrendChart metrics={metrics} rateTrend={rateTrend()} />);
    expect(screen.getByRole("img", { name: /QPS 与 RPM 请求趋势/ })).toBeInTheDocument();
    expect(screen.getByText("请求/秒 · 左轴")).toBeInTheDocument();
    expect(screen.getByText("请求/分钟 · 右轴")).toBeInTheDocument();
    expect(screen.getByRole("group")).toHaveAttribute("aria-label", "13:16:14 UTC，QPS 2，RPM 120");
    // 缺口秒不参与连线：QPS 分成 300 与 299 个逐秒点。
    expect([...container.querySelectorAll(".metrics-chart-qps-line")].map(vertices)).toEqual([300, 299]);
    // 缺口秒会让其后 60 秒的窗口都不完整，因此 RPM 分成 241 与 240 个逐秒点。
    expect([...container.querySelectorAll(".metrics-chart-rpm-line")].map(vertices)).toEqual([241, 240]);
    expect(container.querySelector(".metrics-chart-qps-area")).toBeNull();
  });

  it("时间轴刻度标签按分钟给出，首尾对齐窗口边界", () => {
    const { container } = render(<MetricsTrendChart metrics={metrics} rateTrend={rateTrend()} />);
    const labels = tickLabels(container);
    expect(labels).toHaveLength(11);
    expect(labels[0]).toBe("13:06");
    expect(labels.at(-1)).toBe("13:16");
  });

  it("相邻可用样本间隔过大时按缺口断开，不跨秒点网格连线", () => {
    const qps: RateSample[] = [
      { at_ms: startAt, value: { state: "available", value: 2 } },
      { at_ms: startAt + 1_000, value: { state: "available", value: 2 } },
      { at_ms: startAt + 2_000, value: { state: "available", value: 2 } },
      { at_ms: startAt + 30_347, value: { state: "available", value: 2 } },
      { at_ms: startAt + 31_347, value: { state: "available", value: 2 } },
    ];
    const { container } = render(
      <MetricsTrendChart metrics={metrics} rateTrend={{ qps, rpm: rollingRpm(qps, startAt, sampledAt) }} />,
    );
    expect([...container.querySelectorAll(".metrics-chart-qps-line")].map(vertices)).toEqual([3, 2]);
  });

  it("键盘按秒选点，缺口秒不伪造零值", () => {
    const { container } = render(<MetricsTrendChart metrics={metrics} rateTrend={rateTrend()} />);
    const chart = screen.getByRole("group");
    // 窗口最左 60 秒缺少历史，RPM 不可用但不影响 QPS。
    fireEvent.keyDown(chart, { key: "Home" });
    expect(chart).toHaveAttribute("aria-label", "13:06:15 UTC，QPS 2，RPM 暂不可用");
    fireEvent.keyDown(chart, { key: "End" });
    expect(chart).toHaveAttribute("aria-label", "13:16:14 UTC，QPS 2，RPM 120");
    fireEvent.keyDown(chart, { key: "ArrowLeft" });
    expect(chart).toHaveAttribute("aria-label", "13:16:13 UTC，QPS 2，RPM 120");

    const svg = screen.getByRole("img", { name: /QPS 与 RPM 请求趋势/ });
    vi.spyOn(svg, "getBoundingClientRect").mockReturnValue({ left: 0, width: WIDTH } as DOMRect);
    fireEvent.pointerDown(svg, { clientX: 42 + (300_000 / TREND_WINDOW_MS) * (WIDTH - 42 - 48) });
    expect(chart).toHaveAttribute("aria-label", "13:11:15 UTC，QPS 暂不可用，RPM 暂不可用");
    expect(container.querySelector(".metrics-chart-point-qps")).toBeNull();
    expect(container.querySelector(".metrics-chart-tooltip")).not.toBeNull();
    fireEvent.blur(chart);
    expect(container.querySelector(".metrics-chart-tooltip")).toBeNull();
  });

  it("窗口外的高值不影响当前刻度或可选样本", () => {
    const { container } = render(
      <MetricsTrendChart
        metrics={metrics}
        rateTrend={rateTrend(undefined, [
          { at_ms: startAt - 700_000, value: { state: "available", value: 99_999 } },
          { at_ms: sampledAt + 1_000, value: { state: "available", value: 99_999 } },
        ])}
      />,
    );
    expect(container.querySelector(".metrics-chart-axis-qps")).toHaveTextContent("4");
    fireEvent.keyDown(screen.getByRole("group"), { key: "Home" });
    expect(screen.getByRole("group")).toHaveAttribute("aria-label", "13:06:15 UTC，QPS 2，RPM 暂不可用");
  });

  it("空趋势与全部不可用都有明确提示，不制造折线", () => {
    const { container, rerender } = render(<MetricsTrendChart metrics={metrics} rateTrend={{ qps: [], rpm: [] }} />);
    expect(screen.getByText("最近十分钟暂无趋势样本")).toBeInTheDocument();
    expect(container.querySelector(".metrics-chart-tooltip")).toBeNull();
    rerender(
      <MetricsTrendChart
        metrics={metrics}
        rateTrend={{
          qps: [{ at_ms: startAt + 1_000, value: { state: "unavailable", reason: "observation_gap", observed_seconds: null } }],
          rpm: [],
        }}
      />,
    );
    expect(screen.getByText("趋势样本暂不可用，等待有效采样")).toBeInTheDocument();
    expect(container.querySelectorAll(".metrics-chart-qps-line, .metrics-chart-rpm-line")).toHaveLength(0);
  });

  it("RPM 窗口不足 60 秒时只给最新一秒数值", () => {
    const short = qpsSeries(undefined).filter(({ at_ms }) => at_ms >= startAt + 540_000);
    const trend = { qps: short, rpm: rollingRpm(short, startAt, sampledAt) };
    expect(trend.rpm).toHaveLength(RPM_WINDOW_SECONDS);
    // 只有最后一秒的 60 秒窗口完整，其余点保留为不可用而不是零值。
    expect(trend.rpm.filter(({ value }) => value.state === "available")).toHaveLength(1);
    expect(trend.rpm.at(-1)?.value).toEqual({ state: "available", value: 120 });
    expect(trend.rpm.at(-2)?.value.state).toBe("unavailable");
    const { container } = render(<MetricsTrendChart metrics={metrics} rateTrend={trend} />);
    const rpmPaths = [...container.querySelectorAll(".metrics-chart-rpm-line")];
    expect(rpmPaths).toHaveLength(1);
    // 孤立样本用零长度线段保留可见点。
    expect(rpmPaths[0]?.getAttribute("d")).toMatch(/^M (.+) L \1$/);
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
      const { container } = render(<MetricsTrendChart metrics={metrics} rateTrend={rateTrend()} />);
      act(() => resize([{ contentRect: { width: 300 } } as ResizeObserverEntry], {} as ResizeObserver));
      const chart = screen.getByRole("img", { name: /QPS 与 RPM 请求趋势/ });
      expect(chart).toHaveAttribute("viewBox", "0 0 300 252");
      // 窄屏刻度标签放宽到 5 分钟，避免文字重叠。
      expect(tickLabels(container)).toEqual(["13:06", "13:11", "13:16"]);
      vi.spyOn(chart, "getBoundingClientRect").mockReturnValue({ left: 0, width: 300 } as DOMRect);
      fireEvent.pointerDown(chart, { clientX: 42 + (300 - 42 - 48) / 2 });
      expect(screen.getByRole("group")).toHaveAttribute("aria-label", "13:11:15 UTC，QPS 暂不可用，RPM 暂不可用");
      fireEvent.pointerMove(chart, { clientX: 300 });
      expect(screen.getByRole("group")).toHaveAttribute("aria-label", "13:16:14 UTC，QPS 2，RPM 120");
      expect(container.querySelector(".metrics-chart-tooltip")).toHaveStyle({ left: "200px" });
    } finally {
      vi.stubGlobal("ResizeObserver", original);
    }
  });
});

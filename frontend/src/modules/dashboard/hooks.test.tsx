import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { serviceMetricsFixture } from "@/mocks/fixtures";
import { managementEvents, type EventConnectionState, type ServiceMetrics } from "@/shared/api/events";
import { getServiceMetrics } from "./api";
import { METRICS_STALE_AFTER_MS, isMetricsStale, useServiceMetrics } from "./hooks";

vi.mock("./api", () => ({
  serviceMetricsKey: ["api", "v2", "service", "metrics"],
  getServiceMetrics: vi.fn(),
}));

vi.mock("@/shared/api/events", () => ({
  managementEvents: { subscribeMetrics: vi.fn() },
}));

/** 后端时钟落后浏览器 3.5 秒：旧实现按两者差值判断，会在每个推送周期内来回切换状态。 */
const SERVER_CLOCK_OFFSET_MS = 3_500;

function fixturesAged(offsetMs: number): ServiceMetrics {
  return { ...serviceMetricsFixture, sampled_at_ms: Date.now() - offsetMs };
}

describe("isMetricsStale", () => {
  it("只按本地收到数据的时刻判定，与服务端时间戳和浏览器时钟偏移无关", () => {
    const receivedAt = 1_000_000;
    const connectionState: EventConnectionState = "open";
    expect(isMetricsStale({ hasData: true, dataUpdatedAt: receivedAt, nowMs: receivedAt + 500, connectionState })).toBe(false);
    expect(isMetricsStale({ hasData: true, dataUpdatedAt: receivedAt, nowMs: receivedAt + METRICS_STALE_AFTER_MS, connectionState })).toBe(false);
    expect(isMetricsStale({ hasData: true, dataUpdatedAt: receivedAt, nowMs: receivedAt + METRICS_STALE_AFTER_MS + 1, connectionState })).toBe(true);
  });

  it("没有快照时不判定延迟，连接错误立即判定延迟", () => {
    expect(isMetricsStale({ hasData: false, dataUpdatedAt: 0, nowMs: 9_999_999, connectionState: "connecting" })).toBe(false);
    expect(isMetricsStale({ hasData: true, dataUpdatedAt: 9_999_999, nowMs: 9_999_999, connectionState: "error" })).toBe(true);
  });
});

describe("useServiceMetrics 延迟判定", () => {
  let push: ((metrics: ServiceMetrics) => void) | undefined;

  beforeEach(() => {
    vi.useFakeTimers();
    push = undefined;
    vi.mocked(getServiceMetrics).mockResolvedValue(fixturesAged(SERVER_CLOCK_OFFSET_MS));
    vi.mocked(managementEvents.subscribeMetrics).mockImplementation((onData, onState) => {
      push = onData;
      onState("open");
      return () => { push = undefined; };
    });
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  function renderMetrics() {
    const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    return renderHook(() => useServiceMetrics(), {
      wrapper: ({ children }) => <QueryClientProvider client={client}>{children}</QueryClientProvider>,
    });
  }

  /** react-query 用 setTimeout(…, 0) 派发通知，假定时器下必须显式推进才会重渲染。 */
  async function deliverPush(metrics: ServiceMetrics) {
    await act(async () => {
      push?.(metrics);
      await vi.advanceTimersByTimeAsync(0);
    });
  }

  it("服务端时间戳落后本地时钟时，推送持续到达就保持正常", async () => {
    const { result } = renderMetrics();
    await act(async () => { await vi.advanceTimersByTimeAsync(0); });
    expect(result.current.data).toBeDefined();
    expect(push).toBeDefined();
    // 前提：快照时间戳确实比本地时钟早 3.5 秒，超过延迟阈值。
    expect(Date.now() - (result.current.data?.sampled_at_ms ?? 0)).toBeGreaterThan(METRICS_STALE_AFTER_MS);
    expect(result.current.stale).toBe(false);

    // 每个推送周期都收到新快照，状态始终正常，不出现每秒一次的闪烁。
    for (let cycle = 0; cycle < 3; cycle += 1) {
      await act(async () => { await vi.advanceTimersByTimeAsync(1_000); });
      expect(result.current.stale).toBe(false);
      await deliverPush(fixturesAged(SERVER_CLOCK_OFFSET_MS));
      expect(result.current.stale).toBe(false);
    }
  });

  it("推送停止超过阈值后判定延迟，恢复推送后回到正常", async () => {
    const { result } = renderMetrics();
    await act(async () => { await vi.advanceTimersByTimeAsync(0); });
    expect(result.current.stale).toBe(false);

    await act(async () => { await vi.advanceTimersByTimeAsync(METRICS_STALE_AFTER_MS + 1_000); });
    expect(result.current.stale).toBe(true);

    await deliverPush(fixturesAged(SERVER_CLOCK_OFFSET_MS));
    expect(result.current.stale).toBe(false);
  });
});

import { http, HttpResponse } from "msw";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { server } from "@/mocks/server";
import { setMockAuthenticated } from "@/mocks/handlers";
import { serviceMetricsFixture, sessionFixture, v2QueryPageFixture, v2QueryRecordsFixture } from "@/mocks/fixtures";
import { acceptAuthSession, onUnauthorized } from "./client";
import { managementEvents, type ServiceMetrics } from "./events";

class CapturingWebSocket extends EventTarget {
  static readonly CONNECTING = 0;
  static readonly OPEN = 1;
  static readonly CLOSING = 2;
  static readonly CLOSED = 3;
  static instances: CapturingWebSocket[] = [];
  readonly url: string;
  readonly protocols: string[];
  protocol = "";
  readyState = CapturingWebSocket.CONNECTING;
  sent: string[] = [];

  constructor(url: string | URL, protocols?: string | string[]) {
    super();
    this.url = String(url);
    this.protocols = Array.isArray(protocols) ? protocols : protocols ? [protocols] : [];
    CapturingWebSocket.instances.push(this);
  }

  open() {
    this.readyState = CapturingWebSocket.OPEN;
    this.protocol = this.protocols[0] ?? "";
    this.dispatchEvent(new Event("open"));
    this.message({ type: "ready", protocol_version: 1, epoch: "stream-test" });
  }

  message(value: unknown) {
    this.dispatchEvent(new MessageEvent("message", { data: JSON.stringify(value) }));
  }

  send(value: string) {
    this.sent.push(value);
  }

  close(code = 1000, reason = "") {
    if (this.readyState === CapturingWebSocket.CLOSED) return;
    this.readyState = CapturingWebSocket.CLOSED;
    this.dispatchEvent(new CloseEvent("close", { code, reason, wasClean: code === 1000 }));
  }
}

const originalWebSocket = globalThis.WebSocket;

/** 测试基准时刻；增量帧的连续性按整秒步长断言。 */
const at = Date.parse("2026-09-22T13:16:15Z");

/** 增量帧里的后缀样本只用于时间连续性，取值不参与断言。 */
const rateSample = (at_ms: number) => ({ at_ms, value: { state: "available", value: 1 } });

/** 增量帧复用快照夹具的标量，避免在用例里重复声明一份指标值。 */
const deltaScalars = {
  qps: serviceMetricsFixture.qps,
  rpm: serviceMetricsFixture.rpm,
  online_clients: serviceMetricsFixture.online_clients,
  rss_bytes: serviceMetricsFixture.rss_bytes,
  cpu_percent: serviceMetricsFixture.cpu_percent,
};

describe("ManagementEventClient", () => {
  beforeEach(() => {
    CapturingWebSocket.instances = [];
    Object.defineProperty(globalThis, "WebSocket", { configurable: true, writable: true, value: CapturingWebSocket });
    setMockAuthenticated(true);
    acceptAuthSession({
      session: sessionFixture,
      access_token: "A".repeat(43),
      token_type: "Bearer",
      access_expires_at_ms: Date.now() + 300_000,
    });
    server.use(http.post("/api/v2/events/ticket", () => HttpResponse.json({
      ticket: "W".repeat(43),
      expires_at_ms: Date.now() + 30_000,
    }, { status: 201 })));
  });

  afterEach(() => {
    Object.defineProperty(globalThis, "WebSocket", { configurable: true, writable: true, value: originalWebSocket });
  });

  it("只在 subprotocol 发送单次 ticket，并转发指标与心跳", async () => {
    const metrics = vi.fn();
    const states = vi.fn();
    const unsubscribe = managementEvents.subscribeMetrics(metrics, states);
    await vi.waitFor(() => expect(CapturingWebSocket.instances).toHaveLength(1));
    const socket = CapturingWebSocket.instances[0];
    expect(socket.url).toBe("ws://localhost:3000/api/v2/events");
    expect(socket.url).not.toContain("token");
    expect(socket.protocols).toEqual(["fluxdns.v1", `fluxdns.ticket.${"W".repeat(43)}`]);

    socket.open();
    const sent = socket.sent.map((value) => JSON.parse(value));
    expect(sent).toContainEqual(expect.objectContaining({ type: "subscribe_metrics" }));
    const subscription = sent.find(({ type }) => type === "subscribe_metrics");
    socket.message({ type: "metrics", subscription_id: subscription.subscription_id, data: serviceMetricsFixture });
    expect(metrics).toHaveBeenCalledWith(serviceMetricsFixture);
    socket.message({ type: "ping", nonce: "ping:1" });
    expect(socket.sent.map((value) => JSON.parse(value))).toContainEqual({ type: "pong", nonce: "ping:1" });
    expect(states).toHaveBeenLastCalledWith("open");
    unsubscribe();
  });

  it("订阅全量基线后的增量帧按序拼装转发，不重复订阅", async () => {
    const metrics = vi.fn<(value: ServiceMetrics) => void>();
    const unsubscribe = managementEvents.subscribeMetrics(metrics, vi.fn());
    await vi.waitFor(() => expect(CapturingWebSocket.instances).toHaveLength(1));
    const socket = CapturingWebSocket.instances[0];
    socket.open();
    const subscription = socket.sent.map((value) => JSON.parse(value)).find(({ type }) => type === "subscribe_metrics");

    socket.message({
      type: "metrics",
      subscription_id: subscription.subscription_id,
      data: { ...serviceMetricsFixture, qps_trend: [rateSample(at)], rpm_trend: [] },
    });
    expect(metrics).toHaveBeenCalledTimes(1);

    socket.message({
      type: "metrics_delta",
      subscription_id: subscription.subscription_id,
      data: { ...deltaScalars, sampled_at_ms: at + 2_000, qps_samples: [rateSample(at + 1_000)], rpm_samples: [rateSample(at)] },
    });
    expect(metrics).toHaveBeenCalledTimes(2);
    expect(metrics.mock.calls[1][0].qps_trend.map(({ at_ms }) => at_ms)).toEqual([at, at + 1_000]);
    expect(metrics.mock.calls[1][0].rpm_trend.map(({ at_ms }) => at_ms)).toEqual([at]);
    expect(metrics.mock.calls[1][0].sampled_at_ms).toBe(at + 2_000);
    expect(socket.sent.map((value) => JSON.parse(value)).filter(({ type }) => type === "subscribe_metrics")).toHaveLength(1);
    unsubscribe();
  });

  it("增量帧带洞时重新订阅取新基线，绝不转发拼接结果", async () => {
    const metrics = vi.fn();
    const unsubscribe = managementEvents.subscribeMetrics(metrics, vi.fn());
    await vi.waitFor(() => expect(CapturingWebSocket.instances).toHaveLength(1));
    const socket = CapturingWebSocket.instances[0];
    socket.open();
    const subscription = socket.sent.map((value) => JSON.parse(value)).find(({ type }) => type === "subscribe_metrics");

    socket.message({
      type: "metrics",
      subscription_id: subscription.subscription_id,
      data: { ...serviceMetricsFixture, qps_trend: [rateSample(at)], rpm_trend: [] },
    });
    expect(metrics).toHaveBeenCalledTimes(1);

    socket.message({
      type: "metrics_delta",
      subscription_id: subscription.subscription_id,
      data: { ...deltaScalars, sampled_at_ms: at + 3_000, qps_samples: [rateSample(at + 2_000)], rpm_samples: [] },
    });
    expect(metrics).toHaveBeenCalledTimes(1);
    const subscribes = socket.sent.map((value) => JSON.parse(value)).filter(({ type }) => type === "subscribe_metrics");
    expect(subscribes).toHaveLength(2);
    expect(subscribes[1].subscription_id).toBe(subscription.subscription_id);
    unsubscribe();
  });

  it("增量帧缺少后缀样本字段时按非法消息断开连接", async () => {
    const unsubscribe = managementEvents.subscribeMetrics(vi.fn(), vi.fn());
    await vi.waitFor(() => expect(CapturingWebSocket.instances).toHaveLength(1));
    const socket = CapturingWebSocket.instances[0];
    socket.open();
    const subscription = socket.sent.map((value) => JSON.parse(value)).find(({ type }) => type === "subscribe_metrics");

    socket.message({ type: "metrics_delta", subscription_id: subscription.subscription_id, data: { sampled_at_ms: at } });
    expect(socket.readyState).toBe(CapturingWebSocket.CLOSED);
    unsubscribe();
  });

  it("基线帧缺少标量或趋势字段时按非法消息断开连接", async () => {
    const metrics = vi.fn<(value: ServiceMetrics) => void>();
    const unsubscribe = managementEvents.subscribeMetrics(metrics, vi.fn());
    await vi.waitFor(() => expect(CapturingWebSocket.instances).toHaveLength(1));
    const socket = CapturingWebSocket.instances[0];
    socket.open();
    const subscription = socket.sent.map((value) => JSON.parse(value)).find(({ type }) => type === "subscribe_metrics");

    const { cpu_percent: _cpu_percent, ...withoutCpu } = serviceMetricsFixture;
    socket.message({ type: "metrics", subscription_id: subscription.subscription_id, data: withoutCpu });
    expect(socket.readyState).toBe(CapturingWebSocket.CLOSED);
    expect(metrics).not.toHaveBeenCalled();
    unsubscribe();
  });

  it("异常断线取得新 ticket 重连，会话失效则停止并通知认证 owner", async () => {
    const unauthorized = vi.fn();
    const removeUnauthorized = onUnauthorized(unauthorized);
    const unsubscribe = managementEvents.subscribeMetrics(vi.fn(), vi.fn());
    await vi.waitFor(() => expect(CapturingWebSocket.instances).toHaveLength(1));
    CapturingWebSocket.instances[0].open();
    CapturingWebSocket.instances[0].close(1013, "slow consumer");
    await vi.waitFor(() => expect(CapturingWebSocket.instances).toHaveLength(2), { timeout: 1_500 });
    CapturingWebSocket.instances[1].open();
    CapturingWebSocket.instances[1].close(4401, "session expired");
    await vi.waitFor(() => expect(unauthorized).toHaveBeenCalledOnce());
    await new Promise((resolve) => window.setTimeout(resolve, 600));
    expect(CapturingWebSocket.instances).toHaveLength(2);
    unsubscribe();
    removeUnauthorized();
  });

  it("记录订阅在断线后携最新 commit cursor replay，并显式转发 resync", async () => {
    const batches = vi.fn();
    const resync = vi.fn();
    const unsubscribe = managementEvents.subscribeQueries({
      filter: { from_ms: 1, to_ms: 2 },
      after: v2QueryPageFixture.snapshot_cursor,
      retentionRevision: v2QueryPageFixture.retention_revision,
    }, batches, resync, vi.fn());
    await vi.waitFor(() => expect(CapturingWebSocket.instances).toHaveLength(1));
    const first = CapturingWebSocket.instances[0];
    first.open();
    const firstSubscription = first.sent.map((value) => JSON.parse(value)).find(({ type }) => type === "subscribe_queries");
    expect(firstSubscription).toMatchObject({
      after: v2QueryPageFixture.snapshot_cursor,
      retention_revision: "9",
    });

    first.message({
      type: "queries",
      subscription_id: firstSubscription.subscription_id,
      cursor: { epoch: "stream-1", sequence: "43" },
      directory_revision: "clients-13",
      items: [v2QueryRecordsFixture[0]],
    });
    expect(batches).toHaveBeenCalledWith(expect.objectContaining({
      cursor: { epoch: "stream-1", sequence: "43" },
      directoryRevision: "clients-13",
    }));

    first.close(1013, "slow consumer");
    await vi.waitFor(() => expect(CapturingWebSocket.instances).toHaveLength(2), { timeout: 1_500 });
    const second = CapturingWebSocket.instances[1];
    second.open();
    expect(second.sent.map((value) => JSON.parse(value))).toContainEqual(expect.objectContaining({
      type: "subscribe_queries",
      after: { epoch: "stream-1", sequence: "43" },
    }));
    second.message({
      type: "resync_required",
      subscription_id: firstSubscription.subscription_id,
      reason: "retention_changed",
    });
    expect(resync).toHaveBeenCalledWith("retention_changed");
    unsubscribe();
  });
});

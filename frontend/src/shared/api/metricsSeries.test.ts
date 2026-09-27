import { describe, expect, it } from "vitest";
import { serviceMetricsFixture } from "@/mocks/fixtures";
import {
  SERIES_MINUTES,
  SERIES_SECONDS,
  applyMetricsDelta,
  applyMetricsSnapshot,
  type ServiceMetrics,
  type ServiceMetricsDelta,
  type RateSample,
} from "./metricsSeries";

const base = Date.parse("2026-09-22T13:16:15Z");

const sample = (at_ms: number, value: number): RateSample => ({ at_ms, value: { state: "available", value } });

/** 从 `from` 起每秒一个可用样本。 */
const seconds = (from: number, count: number, value = 2): RateSample[] =>
  Array.from({ length: count }, (_, index) => sample(from + index * 1_000, value));

/** 基线只声明被测的秒序列；分钟序列由用例显式经增量帧提供。 */
const snapshot = (trend: RateSample[], sampledAt: number): ServiceMetrics => ({
  ...serviceMetricsFixture,
  sampled_at_ms: sampledAt,
  qps_trend: trend,
  rpm_trend: [],
});

/** 增量帧只带标量与新增后缀样本，不含窗口字段。 */
const delta = (overrides: Partial<ServiceMetricsDelta> = {}): ServiceMetricsDelta => ({
  sampled_at_ms: base,
  qps: serviceMetricsFixture.qps,
  rpm: serviceMetricsFixture.rpm,
  online_clients: serviceMetricsFixture.online_clients,
  rss_bytes: serviceMetricsFixture.rss_bytes,
  cpu_percent: serviceMetricsFixture.cpu_percent,
  qps_samples: [],
  rpm_samples: [],
  ...overrides,
});

describe("applyMetricsSnapshot", () => {
  it("整体替换序列并按上限裁剪", () => {
    const trimmed = applyMetricsSnapshot(snapshot(seconds(base - 700_000, 700), base));
    expect(trimmed.qps_trend).toHaveLength(SERIES_SECONDS);
    expect(trimmed.qps_trend.at(-1)?.at_ms).toBe(base - 1_000);
  });
});

describe("applyMetricsDelta", () => {
  it("把新秒桶追加到基线末桶之后并更新标量", () => {
    const baseline = snapshot(seconds(base - 3_000, 3), base);
    const outcome = applyMetricsDelta(baseline, delta({
      sampled_at_ms: base + 2_000,
      qps_samples: [sample(base, 7), sample(base + 1_000, 8)],
      rpm_samples: [sample(base - 60_000, 255)],
    }));

    expect(outcome.status).toBe("applied");
    if (outcome.status !== "applied") return;
    expect(outcome.metrics.qps_trend.map(({ at_ms }) => at_ms)).toEqual([
      base - 3_000, base - 2_000, base - 1_000, base, base + 1_000,
    ]);
    expect(outcome.metrics.qps_trend.at(-1)?.value).toEqual({ state: "available", value: 8 });
    expect(outcome.metrics.sampled_at_ms).toBe(base + 2_000);
    expect(outcome.metrics.rpm_trend.map(({ at_ms }) => at_ms)).toEqual([base - 60_000]);
  });

  it("超出上限时丢弃最旧秒桶与分钟桶", () => {
    const baseline = snapshot(seconds(base - (SERIES_SECONDS - 1) * 1_000, SERIES_SECONDS), base);
    const outcome = applyMetricsDelta(baseline, delta({
      sampled_at_ms: base + 1_000,
      qps_samples: [sample(base + 1_000, 1)],
      rpm_samples: Array.from({ length: SERIES_MINUTES + 1 }, (_, index) => sample(base - (SERIES_MINUTES - index) * 60_000, 10)),
    }));

    expect(outcome.status).toBe("applied");
    if (outcome.status !== "applied") return;
    expect(outcome.metrics.sampled_at_ms).toBe(base + 1_000);
    expect(outcome.metrics.rpm_trend).toHaveLength(SERIES_MINUTES);
    expect(outcome.metrics.qps_trend[0]?.at_ms).toBe(base - (SERIES_SECONDS - 2) * 1_000);
  });

  it("没有全量基线时拒绝拼接并要求重同步", () => {
    expect(applyMetricsDelta(undefined, delta({ qps_samples: [sample(base, 1)] })))
      .toEqual({ status: "resync", reason: "no_baseline" });
  });

  it("与基线末桶不连续时拒绝拼接，不用带洞序列冒充连续曲线", () => {
    const baseline = snapshot(seconds(base - 3_000, 3), base);
    expect(applyMetricsDelta(baseline, delta({ qps_samples: [sample(base + 1_000, 1)] })))
      .toEqual({ status: "resync", reason: "gap" });
    expect(applyMetricsDelta(baseline, delta({ qps_samples: [sample(base - 10_000, 1)] })))
      .toEqual({ status: "resync", reason: "gap" });
  });

  it("帧内本身有洞时同样要求重同步", () => {
    const baseline = snapshot(seconds(base - 1_000, 1), base);
    expect(applyMetricsDelta(baseline, delta({ qps_samples: [sample(base, 1), sample(base + 3_000, 1)] })))
      .toEqual({ status: "resync", reason: "gap" });
    expect(applyMetricsDelta(baseline, delta({ qps_samples: [sample(base, 1)], rpm_samples: [sample(base, 1), sample(base + 120_000, 1)] })))
      .toEqual({ status: "resync", reason: "gap" });
  });

  it("服务刚启动的基线序列为空时，第一帧增量正好补上序列起点", () => {
    const emptyBaseline = snapshot([], base);
    const outcome = applyMetricsDelta(emptyBaseline, delta({ sampled_at_ms: base + 1_000, qps_samples: [sample(base, 1)] }));
    expect(outcome.status).toBe("applied");
    if (outcome.status !== "applied") return;
    expect(outcome.metrics.qps_trend.map(({ at_ms }) => at_ms)).toEqual([base]);
  });

  it("只有标量没有新样本的增量仍然推进采样时间", () => {
    const baseline = snapshot(seconds(base - 1_000, 1), base - 1_000);
    const outcome = applyMetricsDelta(baseline, delta({ sampled_at_ms: base }));
    expect(outcome.status).toBe("applied");
    if (outcome.status !== "applied") return;
    expect(outcome.metrics.sampled_at_ms).toBe(base);
    expect(outcome.metrics.qps_trend).toEqual(baseline.qps_trend);
  });
});

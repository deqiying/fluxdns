import type { components } from "./generated-v2";

type Schemas = components["schemas"];
export type ServiceMetrics = Schemas["ServiceMetrics"];
export type ServiceMetricsDelta = Schemas["ServiceMetricsDelta"];
export type RateSample = Schemas["RateSample"];

/**
 * 本地逐秒序列上限，与 v2 全量快照 `qps_trend` 的 `maxItems` 一致：
 * 最近十分钟窗口加 60 秒前瞻，便于图表推导窗口最左一分钟的 RPM。
 */
export const SERIES_SECONDS = 660;
/** 本地分钟序列上限，与 `rpm_trend` 的 `maxItems` 一致。 */
export const SERIES_MINUTES = 10;

/** 增量帧结果：要么拼出新的完整快照，要么必须重新订阅取新基线。 */
export type MetricsDeltaOutcome =
  | { status: "applied"; metrics: ServiceMetrics }
  | { status: "resync"; reason: "no_baseline" | "gap" };

/**
 * 全量基线：整体替换序列并按上限裁剪，和 REST `/service/metrics` 的返回形状保持一致。
 * 服务端已经按上限裁剪，这里的裁剪只防御异常长度的帧。
 */
export function applyMetricsSnapshot(snapshot: ServiceMetrics): ServiceMetrics {
  return {
    ...snapshot,
    qps_trend: snapshot.qps_trend.slice(-SERIES_SECONDS),
    rpm_trend: snapshot.rpm_trend.slice(-SERIES_MINUTES),
  };
}

/**
 * 增量帧：把新完成的秒桶/分钟桶追加到既有序列，并更新标量。
 *
 * 只有确知既不是缺基线、也没有断点时才拼接：缺基线、与基线末桶不连续、或帧内本身
 * 不连续都返回 `resync`，由调用方重新订阅取全量基线。绝不把带洞的序列当作连续曲线。
 */
export function applyMetricsDelta(
  baseline: ServiceMetrics | undefined,
  delta: ServiceMetricsDelta,
): MetricsDeltaOutcome {
  if (!baseline) return { status: "resync", reason: "no_baseline" };
  const lastAt = baseline.qps_trend.at(-1)?.at_ms;
  const firstAt = delta.qps_samples.at(0)?.at_ms;
  // 空基线只可能来自服务启动不足一秒时的快照，此时服务端增量游标停在第 0 秒，
  // 这一帧就是从序列起点开始补的，没有需要补的洞；因此只在基线已有末桶时要求整秒连续。
  if (firstAt !== undefined && lastAt !== undefined && firstAt !== lastAt + 1_000) {
    return { status: "resync", reason: "gap" };
  }
  if (!isContiguous(delta.qps_samples, 1_000) || !isContiguous(delta.rpm_samples, 60_000)) {
    return { status: "resync", reason: "gap" };
  }
  return {
    status: "applied",
    metrics: {
      sampled_at_ms: delta.sampled_at_ms,
      qps: delta.qps,
      rpm: delta.rpm,
      online_clients: delta.online_clients,
      rss_bytes: delta.rss_bytes,
      cpu_percent: delta.cpu_percent,
      qps_trend: [...baseline.qps_trend, ...delta.qps_samples].slice(-SERIES_SECONDS),
      rpm_trend: [...baseline.rpm_trend, ...delta.rpm_samples].slice(-SERIES_MINUTES),
    },
  };
}

/** 帧内样本必须按固定步长严格递增，否则这一段序列本身就有洞。 */
function isContiguous(samples: RateSample[], stepMs: number): boolean {
  return samples.every((sample, index) => index === 0 || sample.at_ms === samples[index - 1]!.at_ms + stepMs);
}

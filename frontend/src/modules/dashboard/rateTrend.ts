import { useEffect, useMemo, useState } from "react";
import type { ServiceMetrics } from "./api";

export type RateSample = ServiceMetrics["qps_trend"][number];

/** 趋势窗口与后端 `qps_trend` 上限一致（600 秒）。 */
export const TREND_WINDOW_MS = 10 * 60_000;
/** RPM 口径：过去 60 秒内的请求次数，因此每秒都需要 60 个逐秒 QPS 样本。 */
export const RPM_WINDOW_SECONDS = 60;
/** 页面本地保留的逐秒 QPS 长度：2 倍于 60 秒窗口，用于在快照的逐秒样本缺失时补位。 */
export const QPS_CACHE_SECONDS = 120;

export interface RateTrend {
  qps: RateSample[];
  rpm: RateSample[];
}

const EMPTY_RATE_TREND: RateTrend = { qps: [], rpm: [] };

/** 按秒合并两份逐秒样本，同一秒以 `incoming` 为准；输出按时间升序，且复用原始样本对象。 */
export function unionSamples(base: RateSample[], incoming: RateSample[]): RateSample[] {
  const merged = new Map<number, RateSample>();
  for (const sample of base) merged.set(sample.at_ms, sample);
  for (const sample of incoming) merged.set(sample.at_ms, sample);
  return [...merged.values()].sort((left, right) => left.at_ms - right.at_ms);
}

/**
 * 维护本地 QPS 缓存：在已有缓存上并入新快照的逐秒样本，按已知最新时刻只保留最近 120 秒。
 * 维护本地 QPS 缓存：在已有缓存上并入新快照的逐秒样本，按已知最新时刻只保留最近 120 秒。
 */
export function mergeQpsCache(previous: RateSample[], incoming: RateSample[], newestAt: number): RateSample[] {
  // 以已知最新时刻（缓存与快照取大者）为基准，避免滞后快照把更近的秒点裁掉。
  const newest = Math.max(newestAt, previous.at(-1)?.at_ms ?? 0);
  const kept = unionSamples(previous, incoming).filter(({ at_ms }) => at_ms > newest - QPS_CACHE_SECONDS * 1_000);
  // 同一快照被重复合并时内容不变，返回原引用可以少一次 setState 和渲染。
  if (kept.length === previous.length && kept.every((sample, index) => sample === previous[index])) return previous;
  return kept;
}

/**
 * 逐秒推导 RPM：每个秒点的值是该秒及其前 59 秒的 QPS 之和。
 * 这段窗口内只要有任一秒缺失或不可用就不给数值，避免用部分窗口伪造“过去 60 秒请求数”；
 * 窗口起点早于已知最早样本时按 warmup 说明已覆盖秒数，其余缺口按 observation_gap 处理。
 */
export function rollingRpm(samples: RateSample[], startAt: number, endAt: number): RateSample[] {
  const bySecond = new Map(samples.map((sample) => [sample.at_ms, sample.value]));
  const earliest = samples.at(0)?.at_ms ?? Number.POSITIVE_INFINITY;
  const trend: RateSample[] = [];
  for (const { at_ms } of samples) {
    if (at_ms < startAt || at_ms > endAt) continue;
    let total = 0;
    let covered = 0;
    for (let offset = 0; offset < RPM_WINDOW_SECONDS; offset += 1) {
      const value = bySecond.get(at_ms - offset * 1_000);
      if (value?.state !== "available") break;
      total += value.value;
      covered += 1;
    }
    if (covered === RPM_WINDOW_SECONDS) {
      trend.push({ at_ms, value: { state: "available", value: total } });
      continue;
    }
    const warming = at_ms - (RPM_WINDOW_SECONDS - 1) * 1_000 < earliest;
    trend.push({
      at_ms,
      value: {
        state: "unavailable",
        reason: warming ? "warmup" : "observation_gap",
        observed_seconds: warming ? covered : null,
      },
    });
  }
  return trend;
}

/**
 * 页面级逐秒速率：以最新快照为准，用本地 120 秒 QPS 缓存补上快照缺失的秒点，再按 60 秒窗口推导 RPM。
 * 缓存只补位，不覆盖快照口径；后端推送停止时缓存不再增长，图表随快照一起停止推进。
 * RPM 需要按秒对齐的逐秒样本，因此跨后端实例的缓存只有在秒点对齐时才能补齐窗口。
 */
export function useRateTrend(metrics: ServiceMetrics | undefined): RateTrend {
  const [cache, setCache] = useState<RateSample[]>([]);
  useEffect(() => {
    if (!metrics) return;
    setCache((previous) => mergeQpsCache(previous, metrics.qps_trend, metrics.sampled_at_ms));
  }, [metrics]);
  return useMemo(() => {
    if (!metrics) return EMPTY_RATE_TREND;
    const startAt = metrics.sampled_at_ms - TREND_WINDOW_MS;
    const endAt = metrics.sampled_at_ms;
    const qps = unionSamples(cache, metrics.qps_trend)
      .filter(({ at_ms }) => at_ms >= startAt && at_ms <= endAt);
    return { qps, rpm: rollingRpm(qps, startAt, endAt) };
  }, [cache, metrics]);
}

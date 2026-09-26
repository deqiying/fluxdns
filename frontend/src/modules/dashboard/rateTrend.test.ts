import { describe, expect, it } from "vitest";
import {
  QPS_CACHE_SECONDS,
  RPM_WINDOW_SECONDS,
  mergeQpsCache,
  rollingRpm,
  unionSamples,
  type RateSample,
} from "./rateTrend";

const base = Date.parse("2026-09-22T13:16:15Z");

const available = (at: number, value: number): RateSample => ({ at_ms: at, value: { state: "available", value } });
const unavailable = (at: number): RateSample => ({
  at_ms: at,
  value: { state: "unavailable", reason: "observation_gap", observed_seconds: null },
});
/** 从 `from` 开始每秒一个样本的逐秒 QPS。 */
const series = (from: number, count: number, value = 1): RateSample[] =>
  Array.from({ length: count }, (_, index) => available(from + index * 1_000, value));

const reasonAt = (samples: RateSample[], index: number): string | undefined => {
  const value = samples[index]?.value;
  return value?.state === "unavailable" ? value.reason : undefined;
};

describe("unionSamples", () => {
  it("按时间升序合并，同一秒以新样本为准", () => {
    const merged = unionSamples([available(3_000, 1), available(1_000, 1)], [available(3_000, 9), available(2_000, 5)]);
    expect(merged.map(({ at_ms }) => at_ms)).toEqual([1_000, 2_000, 3_000]);
    expect(merged.at(-1)?.value).toEqual({ state: "available", value: 9 });
  });
});

describe("mergeQpsCache", () => {
  it("只保留已知最新时刻之前的 120 秒，并在同一快照重复合并时复用引用", () => {
    const incoming = series(base - 200_000, 201);
    const cache = mergeQpsCache([], incoming, base);
    expect(cache).toHaveLength(QPS_CACHE_SECONDS);
    expect(cache[0]?.at_ms).toBe(base - 119_000);
    expect(mergeQpsCache(cache, incoming, base)).toBe(cache);
  });

  it("滞后快照与更早的秒点不会裁掉已缓存的新秒点", () => {
    const cache = mergeQpsCache([], series(base, 10), base);
    const merged = mergeQpsCache(cache, series(base - 5_000, 3), base - 6_000);
    expect(merged).toHaveLength(13);
    expect(merged[0]?.at_ms).toBe(base - 5_000);
    expect(merged.at(-1)?.at_ms).toBe(base + 9_000);
  });
});

describe("rollingRpm", () => {
  it("每个秒点是该秒及前 59 秒 QPS 之和，历史不足时按 warmup 报已覆盖秒数", () => {
    const samples = series(base, 120, 2);
    const rpm = rollingRpm(samples, base, base + 119_000);
    expect(rpm).toHaveLength(120);
    expect(rpm[0]?.value).toEqual({ state: "unavailable", reason: "warmup", observed_seconds: 1 });
    expect(rpm[RPM_WINDOW_SECONDS - 2]?.value).toEqual({ state: "unavailable", reason: "warmup", observed_seconds: 59 });
    expect(rpm[RPM_WINDOW_SECONDS - 1]?.value).toEqual({ state: "available", value: 120 });
    expect(rpm.at(-1)?.value).toEqual({ state: "available", value: 120 });
  });

  it("缺口秒会让其后 60 秒的 RPM 都不可用，不按部分窗口求和", () => {
    const samples = series(base, 200);
    samples[50] = unavailable(samples[50]!.at_ms);
    const rpm = rollingRpm(samples, base, base + 199_000);
    expect(reasonAt(rpm, RPM_WINDOW_SECONDS - 1)).toBe("observation_gap");
    expect(reasonAt(rpm, 109)).toBe("observation_gap");
    expect(rpm[110]?.value).toEqual({ state: "available", value: 60 });
  });

  it("只对窗口内样本给点，窗口外样本不参与", () => {
    const samples = [...series(base - 700_000, 3), ...series(base, 120, 3), ...series(base + 200_000, 2)];
    const rpm = rollingRpm(samples, base, base + 119_000);
    expect(rpm).toHaveLength(120);
    expect(rpm.at(-1)?.value).toEqual({ state: "available", value: 180 });
  });
});

describe("本地 QPS 缓存与 RPM 的关系", () => {
  it("快照只带最新秒点时，缓存在同一秒点网格上补齐过去 60 秒窗口", () => {
    const latestAt = base + 120_000;
    const cache = mergeQpsCache([], series(base, 120, 2), latestAt - 1_000);
    const latest = series(latestAt, 1, 2);

    expect(rollingRpm(unionSamples(cache, latest), base, latestAt).at(-1)?.value)
      .toEqual({ state: "available", value: 120 });
    // 没有缓存时只有 1 秒样本，窗口不完整，不用部分窗口给出数值。
    expect(rollingRpm(latest, base, latestAt).at(-1)?.value.state).toBe("unavailable");
  });

  it("秒点网格不一致的后端实例无法与缓存组合，只能等新实例自己补齐 60 秒", () => {
    // 真实后端实例的秒点带启动毫秒偏移，重启后网格整体平移，旧缓存不再命中窗口。
    const latestAt = base + 120_000 + 347;
    const cache = mergeQpsCache([], series(base, 120, 2), latestAt - 1_000);
    const latest = series(latestAt, 1, 2);

    expect(rollingRpm(unionSamples(cache, latest), base, latestAt).at(-1)?.value.state).toBe("unavailable");
  });
});

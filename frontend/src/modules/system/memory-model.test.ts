import { describe, expect, it } from "vitest";
import { memoryAnalysisFixture, memoryOsFixture, memorySnapshotFixture } from "@/mocks/memory-fixtures";
import type { MemoryAnalysis } from "./api";
import { formatMemoryBytes, memoryTrendSegments, moduleDelta, parseMemoryBytes, rssComposition, type MemoryOsSample } from "./memory-model";

const unavailable = { state: "unavailable", reason: "sampling_failed", observed_seconds: null } as const;

describe("memory bytes and RSS composition", () => {
  it("完整且 raw bytes 严格一致才绘制，包括合法的零分项", () => {
    expect(rssComposition(memoryOsFixture)).toEqual({ values: [113246208n, 18874368n, 0n], reason: null });
    expect(rssComposition({ ...memoryOsFixture, rss_bytes: { state: "available", value: "132120577" } }).reason).toMatch("不一致");
  });

  it("缺测、负数、总量零和超出 u64 均不绘制或补零", () => {
    expect(rssComposition({ ...memoryOsFixture, shared_bytes: unavailable }).reason).toMatch("不完整");
    expect(rssComposition({ ...memoryOsFixture, anonymous_bytes: { state: "available", value: "-1" } }).values).toEqual([]);
    expect(rssComposition({ ...memoryOsFixture, rss_bytes: { state: "available", value: "0" } }).reason).toMatch("非正");
    expect(parseMemoryBytes("18446744073709551616")).toBeNull();
    expect(parseMemoryBytes(null)).toBeNull();
  });

  it("大于 Number.MAX_SAFE_INTEGER 的值与差值仍精确，展示使用 MiB", () => {
    expect(parseMemoryBytes("9007199254740993")).toBe(9007199254740993n);
    expect(formatMemoryBytes(1048576n)).toBe("1.0 MiB");
    expect(formatMemoryBytes(-1048576n)).toBe("−1.0 MiB");
    expect(formatMemoryBytes(null)).toBe("未采集");
  });
});

function comparison(): MemoryAnalysis {
  const snapshot = structuredClone(memorySnapshotFixture);
  snapshot.snapshot_id = "2";
  snapshot.modules[0].estimated_bytes = "9007199254740993";
  const previous = structuredClone(snapshot);
  previous.snapshot_id = "1";
  previous.modules[0].estimated_bytes = "9007199254740992";
  return { ...memoryAnalysisFixture, snapshot, previous_snapshot: previous };
}

describe("memory snapshot comparison", () => {
  it("只使用同一响应中的 previous，按 BigInt 计算精确增量", () => {
    const data = comparison();
    expect(moduleDelta(data, data.snapshot!.modules[0])).toBe(1n);
    data.previous_snapshot = null;
    expect(moduleDelta(data, data.snapshot!.modules[0])).toBeNull();
  });

  it("runtime revision 改变时所有模块不比，版本或模块身份不匹配也不比", () => {
    const data = comparison();
    data.previous_snapshot!.runtime_revision = "13";
    expect(moduleDelta(data, data.snapshot!.modules[0])).toBeNull();
    data.previous_snapshot!.runtime_revision = "12";
    data.previous_snapshot!.modules[0].estimate_version = "different";
    expect(moduleDelta(data, data.snapshot!.modules[0])).toBeNull();
    data.previous_snapshot!.modules = [];
    expect(moduleDelta(data, data.snapshot!.modules[0])).toBeNull();
  });

  it("同一快照、缺失估算版本、busy、缺字节均不比", () => {
    const data = comparison();
    const current = data.snapshot!.modules[0];
    data.previous_snapshot!.snapshot_id = data.snapshot!.snapshot_id;
    expect(moduleDelta(data, current)).toBeNull();
    data.previous_snapshot!.snapshot_id = "1";
    expect(moduleDelta(data, { ...current, estimate_version: null })).toBeNull();
    expect(moduleDelta(data, { ...current, state: "busy" })).toBeNull();
    expect(moduleDelta(data, { ...current, estimated_bytes: null })).toBeNull();
  });
});

const sample = (at: number, available = true): MemoryOsSample => ({
  ...memoryOsFixture, sampled_at_ms: at, rss_bytes: available ? memoryOsFixture.rss_bytes : unavailable,
});

describe("memory trend gaps", () => {
  it("缺测、超过 75 秒、倒序、重复时刻断线，保持真实点不排序或插值", () => {
    const points = [sample(0), sample(30_000), sample(60_000, false), sample(90_000), sample(180_000), sample(180_000), sample(170_000), sample(200_000)];
    expect(memoryTrendSegments(points, "rss_bytes", 75).map((segment) => segment.map((point) => point.at)))
      .toEqual([[0, 30_000], [90_000], [180_000], [180_000], [170_000, 200_000]]);
  });

  it("75 秒边界仍连线；孤点与空数据保持原样", () => {
    expect(memoryTrendSegments([sample(0), sample(75_000)], "rss_bytes", 75)).toHaveLength(1);
    expect(memoryTrendSegments([sample(0)], "rss_bytes", 75)[0]).toHaveLength(1);
    expect(memoryTrendSegments([], "rss_bytes", 75)).toEqual([]);
  });
});

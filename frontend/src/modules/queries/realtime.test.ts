import { describe, expect, it } from "vitest";
import { v2QueryRecordsFixture } from "@/mocks/fixtures";
import {
  appendRealtimeBuffer,
  emptyRealtimeBuffer,
  mergeLatestRecords,
  QUERY_BUFFER_RECORDS,
  QUERY_BUFFER_BYTES,
} from "./realtime";

describe("解析记录实时缓冲", () => {
  it("按稳定 ID 去重，并在同毫秒保留推送到达顺序", () => {
    const [first, second] = v2QueryRecordsFixture;
    const merged = mergeLatestRecords([second], [first, first], 20);
    expect(merged.map(({ id }) => id)).toEqual([first.id, second.id]);

    const buffer = appendRealtimeBuffer(emptyRealtimeBuffer(), [first, first, second], new Set([second.id]));
    expect(buffer.items.map(({ id }) => id)).toEqual([first.id]);
  });

  it("超过 500 条后丢弃正文并将数量标记为未知", () => {
    const records = Array.from({ length: QUERY_BUFFER_RECORDS + 1 }, (_, index) => ({
      ...v2QueryRecordsFixture[0],
      id: `buffer-${index}`,
    }));
    expect(appendRealtimeBuffer(emptyRealtimeBuffer(), records)).toEqual({ items: [], bytes: 0, overflow: true });
  });

  it("先达到 2 MiB 时同样进入 resync 状态", () => {
    const answer = { name: "n".repeat(1_000), type: "TXT", ttl_seconds: 30, data: "d".repeat(500) };
    const records = Array.from({ length: 100 }, (_, index) => ({
      ...v2QueryRecordsFixture[0],
      id: `bytes-${index}`,
      answers: { state: "available" as const, total_count: 16, records: Array.from({ length: 16 }, () => answer) },
    }));
    expect(appendRealtimeBuffer(emptyRealtimeBuffer(), records).overflow).toBe(true);
  });
  it("ECS 完整 UTF-8 载荷参与 2 MiB 预算，不能只计算基础记录", () => {
    const base = { ...v2QueryRecordsFixture[0], answers: { state: "available" as const, total_count: 16, records: Array.from({ length: 16 }, () => ({ name: "example.test.", type: "TXT", ttl_seconds: 30, data: "d".repeat(300) })) } };
    const exchange = { state: "observed" as const, query: { state: "present" as const, cidr: "2001:db8::/56", scope_prefix: 0 }, response: { state: "present" as const, cidr: "2001:db8::/56", scope_prefix: 48 }, policy: { source: "rule" as const, mode: "custom" as const, value_source: "custom" as const }, upstream_leaf_name: "成员".repeat(40) };
    const enriched = { ...base, ecs: { request: { state: "absent" as const }, upstream: exchange, cache_match: null, refresh: exchange } };
    const baseBytes = appendRealtimeBuffer(emptyRealtimeBuffer(), [base]).bytes;
    const enrichedBytes = appendRealtimeBuffer(emptyRealtimeBuffer(), [enriched]).bytes;
    expect(enrichedBytes).toBeGreaterThan(baseBytes);
    expect(enrichedBytes).toBe(new TextEncoder().encode(JSON.stringify(enriched)).byteLength + 1);
    const count = Math.floor(QUERY_BUFFER_BYTES / enrichedBytes) + 1;
    expect(count).toBeLessThan(QUERY_BUFFER_RECORDS);
    const records = Array.from({ length: count }, (_, index) => ({ ...enriched, id: `${enriched.id}-${index}` }));
    expect(appendRealtimeBuffer(emptyRealtimeBuffer(), records).overflow).toBe(true);
    expect(appendRealtimeBuffer(emptyRealtimeBuffer(), records.map(({ ecs: _ecs, ...record }) => record)).overflow).toBe(false);
  });
});

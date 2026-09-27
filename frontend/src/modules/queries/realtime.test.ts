import { describe, expect, it } from "vitest";
import { v2QueryRecordsFixture } from "@/mocks/fixtures";
import {
  appendRealtimeBuffer,
  emptyRealtimeBuffer,
  mergeLatestRecords,
  QUERY_BUFFER_RECORDS,
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
});

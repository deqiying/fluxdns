import type { QueryRecord } from "./api";

export const QUERY_BUFFER_RECORDS = 500;
export const QUERY_BUFFER_BYTES = 2 * 1024 * 1024;

export interface RealtimeBuffer {
  items: QueryRecord[];
  bytes: number;
  overflow: boolean;
}

export function emptyRealtimeBuffer(): RealtimeBuffer {
  return { items: [], bytes: 0, overflow: false };
}

/** 溢出后丢弃正文并保留未知缺口，避免继续显示不准确的待更新数量。 */
export function appendRealtimeBuffer(
  buffer: RealtimeBuffer,
  incoming: QueryRecord[],
  visibleIds: ReadonlySet<string> = new Set(),
): RealtimeBuffer {
  if (buffer.overflow) return buffer;
  const known = new Set([...visibleIds, ...buffer.items.map(({ id }) => id)]);
  const additions = incoming.filter(({ id }) => {
    if (known.has(id)) return false;
    known.add(id);
    return true;
  });
  if (additions.length === 0) return buffer;
  const bytes = additions.reduce((total, record) => total + recordBytes(record), buffer.bytes);
  if (buffer.items.length + additions.length > QUERY_BUFFER_RECORDS || bytes > QUERY_BUFFER_BYTES) {
    return { items: [], bytes: 0, overflow: true };
  }
  return { items: [...buffer.items, ...additions], bytes, overflow: false };
}

/** 默认首页按事件时间倒序合并；同毫秒保留服务端/到达顺序，ID 只负责去重。 */
export function mergeLatestRecords(
  current: QueryRecord[],
  incoming: QueryRecord[],
  pageSize: number,
): QueryRecord[] {
  const known = new Set<string>();
  return [...incoming, ...current]
    .filter(({ id }) => {
      if (known.has(id)) return false;
      known.add(id);
      return true;
    })
    .sort((left, right) => right.occurred_at_ms - left.occurred_at_ms)
    .slice(0, pageSize);
}

function recordBytes(record: QueryRecord): number {
  return new TextEncoder().encode(JSON.stringify(record)).byteLength + 1;
}

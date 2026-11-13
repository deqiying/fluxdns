import type { components } from "@/shared/api/generated-v2";
import type { MemoryAnalysis } from "./api";

type Schemas = components["schemas"];
export type MemoryModule = Schemas["MemoryModule"];
export type MemoryOsSample = Schemas["MemoryOsSample"];
export type BytesMeasurement = Schemas["BytesMeasurement"];
export const moduleLabels: Record<MemoryModule["id"], string> = {
  rules: "规则索引", dns_cache: "DNS 响应缓存", details_ws: "解析记录 / WebSocket",
  statistics_queues: "统计与队列", sqlite: "SQLite 工作内存", allocator: "分配器保留空间",
};
export const counterLabels: Record<string, string> = {
  unique_indexes: "唯一索引", retained_indexes: "保留索引", entries: "条目数",
  weighted_size: "淘汰权重", max_weight: "权重上限", replay_records: "回放记录",
  replay_batches: "回放批次", replay_billing_bytes: "回放计费", ws_connections: "WS 连接",
  active_events: "活跃事件", pending_batches: "待处理批次", pending_events: "待处理事件",
  ingress_queue: "入口队列", cache_commit_queue: "缓存提交队列", detail_projection_queue: "明细投影队列",
  active_leases: "活跃租约", lease_budget: "租约预算",
};

/** API 字节必须是无损十进制 u64；非法值不能作为 0 使用。 */
export function parseMemoryBytes(value: string | null | undefined): bigint | null {
  if (typeof value !== "string" || !/^(0|[1-9][0-9]{0,19})$/.test(value)) return null;
  const bytes = BigInt(value);
  return bytes <= 18_446_744_073_709_551_615n ? bytes : null;
}

export function measuredBytes(value: BytesMeasurement): bigint | null {
  return value.state === "available" ? parseMemoryBytes(value.value) : null;
}

/** 内存分析统一 MiB，四舍五入在 BigInt 上完成，不丢失大整数精度。 */
export function formatMemoryBytes(bytes: bigint | null): string {
  if (bytes === null) return "未采集";
  const negative = bytes < 0n;
  const abs = negative ? -bytes : bytes;
  const tenths = (abs * 10n + 524_288n) / 1_048_576n;
  return `${negative ? "−" : ""}${tenths / 10n}.${tenths % 10n} MiB`;
}

/** 非原子 OS 读数必须逐字节一致才画分项，绝不按比例修正或补差额。 */
export function rssComposition(os: MemoryOsSample): { values: bigint[]; reason: string | null } {
  const rss = measuredBytes(os.rss_bytes);
  const parts = [os.anonymous_bytes, os.file_bytes, os.shared_bytes].map(measuredBytes);
  if (rss === null || parts.some((part) => part === null)) return { values: [], reason: "分项不完整，无法绘制 RSS 构成。" };
  const values = parts as bigint[];
  if (rss <= 0n) return { values: [], reason: "RSS 总量非正，无法绘制构成。" };
  if (values.reduce((sum, value) => sum + value, 0n) !== rss) return { values: [], reason: "分项总和与 RSS 不一致；OS 采样不是原子快照，保留原始读数，不修正分项。" };
  return { values, reason: null };
}

/** previous_snapshot 由服务端保证同进程；只消费同一响应，绝不引用上一轮客户端缓存。 */
export function moduleDelta(data: MemoryAnalysis, module: MemoryModule): bigint | null {
  const current = data.snapshot;
  const previous = data.previous_snapshot;
  if (!current || !previous || current.snapshot_id === previous.snapshot_id || current.runtime_revision !== previous.runtime_revision) return null;
  const before = previous.modules.find((item) => item.id === module.id);
  if (!before || module.state !== "available" || before.state !== "available"
      || !module.estimate_version || module.estimate_version !== before.estimate_version) return null;
  const nowBytes = parseMemoryBytes(module.estimated_bytes);
  const oldBytes = parseMemoryBytes(before.estimated_bytes);
  return nowBytes === null || oldBytes === null ? null : nowBytes - oldBytes;
}

export type TrendKey = "rss_bytes" | "anonymous_bytes" | "file_bytes";
export interface MemoryPoint { at: number; bytes: bigint }

/** 保持服务端顺序；缺测、过长间隔和倒序点各自形成断点，不插值、不补样本。 */
export function memoryTrendSegments(samples: MemoryOsSample[], key: TrendKey, gapSeconds: number): MemoryPoint[][] {
  const segments: MemoryPoint[][] = [];
  let segment: MemoryPoint[] = [];
  let previousAt: number | null = null;
  for (const sample of samples) {
    const at = sample.sampled_at_ms;
    const bytes = measuredBytes(sample[key]);
    const validTime = Number.isSafeInteger(at) && at >= 0;
    const broken = previousAt !== null && (at <= previousAt || at - previousAt > gapSeconds * 1_000);
    if (!validTime || bytes === null || broken) {
      if (segment.length) segments.push(segment);
      segment = [];
    }
    if (validTime && bytes !== null) segment.push({ at, bytes });
    previousAt = validTime ? at : null;
  }
  if (segment.length) segments.push(segment);
  return segments;
}

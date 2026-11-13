import type { components } from "@/shared/api/generated-v2";
import { processMetricsFixture } from "./fixtures";

type Schemas = components["schemas"];
const available = (value: string): Schemas["BytesMeasurement"] => ({ state: "available", value });
const sampledAt = Date.parse("2026-09-09T06:03:00Z");

/** 仅供显式 DEV mock 与测试的确定性夹具；正式页面不生成模拟读数。 */
export const memoryOsFixture: Schemas["MemoryOsSample"] = {
  sampled_at_ms: sampledAt,
  rss_bytes: available("132120576"), peak_rss_bytes: available("136314880"),
  anonymous_bytes: available("113246208"), file_bytes: available("18874368"), shared_bytes: available("0"),
  pss_bytes: available("131072000"), private_dirty_bytes: available("112197632"), swap_bytes: available("0"),
};

export const memorySnapshotFixture: Schemas["MemorySnapshot"] = {
  snapshot_id: "1", sampled_at_ms: sampledAt, runtime_revision: "12",
  modules: [
    {
      id: "rules", state: "available", estimated_bytes: "5242880", estimate_version: "rule-payload-v1",
      counters: [{ key: "unique_indexes", value: "2", kind: "count" }, { key: "retained_indexes", value: "1", kind: "count" }],
      rules: [
        { resource: "fixture-blocklist", selector: "domain", rule_count: "40000", estimated_bytes: "4194304", holders: ["worker", "policy"], retained: false },
        { resource: "fixture-blocklist", selector: "domain", rule_count: "10000", estimated_bytes: "1048576", holders: ["initial"], retained: true },
      ],
    },
    {
      id: "dns_cache", state: "available", estimated_bytes: null, estimate_version: null, rules: [],
      counters: [{ key: "entries", value: "1420", kind: "count" }, { key: "weighted_size", value: "1258291", kind: "billing_bytes" }, { key: "max_weight", value: "16777216", kind: "budget_bytes" }],
    },
    {
      id: "details_ws", state: "available", estimated_bytes: null, estimate_version: null, rules: [],
      counters: [{ key: "replay_records", value: "32", kind: "count" }, { key: "replay_batches", value: "2", kind: "count" }, { key: "replay_billing_bytes", value: "8192", kind: "billing_bytes" }, { key: "ws_connections", value: "1", kind: "count" }],
    },
    {
      id: "statistics_queues", state: "available", estimated_bytes: null, estimate_version: null, rules: [],
      counters: [{ key: "active_events", value: "10", kind: "count" }, { key: "pending_batches", value: "1", kind: "count" }, { key: "pending_events", value: "5", kind: "count" }, { key: "ingress_queue", value: "2", kind: "count" }, { key: "cache_commit_queue", value: "1", kind: "count" }, { key: "detail_projection_queue", value: "0", kind: "count" }],
    },
    { id: "sqlite", state: "available", estimated_bytes: null, estimate_version: null, rules: [], counters: [{ key: "active_leases", value: "2", kind: "count" }, { key: "lease_budget", value: "8", kind: "count" }] },
    { id: "allocator", state: "not_collected", estimated_bytes: null, estimate_version: null, rules: [], counters: [] },
  ],
};

export const memoryAnalysisFixture: Schemas["MemoryAnalysis"] = {
  process_id: processMetricsFixture.host.process_id,
  started_at_ms: processMetricsFixture.started_at_ms,
  sample_interval_seconds: 30, gap_after_seconds: 75, os: memoryOsFixture,
  trend: [
    { ...memoryOsFixture, sampled_at_ms: sampledAt - 90_000, rss_bytes: available("130023424") },
    { ...memoryOsFixture, sampled_at_ms: sampledAt - 60_000, rss_bytes: available("131072000") },
    { ...memoryOsFixture, sampled_at_ms: sampledAt - 30_000 }, memoryOsFixture,
  ],
  snapshot: null, previous_snapshot: null, collection_state: "idle", last_failure: null,
};

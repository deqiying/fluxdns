import type {
  HealthSnapshot,
  Overview,
  QueryPage,
  ResourceSnapshot,
  RuntimeSnapshot,
  Session,
  SetupStatus,
  StatisticsPage,
  SystemInfo,
} from "@/shared/api/types";

export const setupReadyFixture = { state: "ready" } satisfies SetupStatus;
export const setupRequiredFixture = { state: "required" } satisfies SetupStatus;

export const sessionFixture = {
  user: { name: "operator" },
  expires_at: "2026-09-03T12:30:00Z",
} satisfies Session;

export const overviewFixture = {
  sampled_at: "2026-09-03T08:00:00Z",
  runtime_revision: "rev-42",
  overall_status: "degraded",
  cards: [
    { key: "queries_24h", label: "24 小时解析量", value: 284_913, unit: "count", status: "available" },
    { key: "cache_hit_rate", label: "缓存命中率", value: 86.47, unit: "percent", status: "available" },
    { key: "failed_queries_24h", label: "24 小时失败", status: "unavailable", unavailable_reason_code: "STORAGE_GAP" },
    { key: "active_listeners", label: "活动 Listener", value: 3, unit: "count", status: "available" },
    { key: "resources", label: "已装配资源", value: 4, unit: "count", status: "available" },
  ],
} satisfies Overview;

export const runtimeFixture = {
  sampled_at: "2026-09-03T08:00:00Z",
  revision: "rev-42",
  normalized_hash: "sha256:47d8…a91c",
  listener_count: 3,
  bind_count: 3,
  resource_count: 4,
  has_policy_core: true,
  binds: [
    { transport: "udp", address: "127.0.0.1", port: 53, owner: "listener-default", v6_only: false, state: "active" },
    { transport: "tcp", address: "127.0.0.1", port: 53, owner: "listener-default", v6_only: false, state: "active" },
    { transport: "doh", address: "127.0.0.1", port: 8053, owner: "doh-local", v6_only: false, state: "draining" },
  ],
} satisfies RuntimeSnapshot;

export const healthFixture = {
  sampled_at: "2026-09-03T08:00:00Z",
  overall_status: "degraded",
  components: [
    {
      component: "runtime",
      status: "healthy",
      reason_code: "READY",
      first_changed_at: "2026-09-03T06:00:00Z",
      last_changed_at: "2026-09-03T06:00:00Z",
      last_success_at: "2026-09-03T07:59:59Z",
      retry_count: 0,
      stale: false,
      gap: false,
    },
    {
      component: "storage",
      status: "degraded",
      reason_code: "PERSISTENCE_GAP",
      first_changed_at: "2026-09-03T07:45:00Z",
      last_changed_at: "2026-09-03T07:45:00Z",
      last_success_at: "2026-09-03T07:44:58Z",
      retry_count: 2,
      stale: false,
      gap: true,
    },
    {
      component: "resource:geosite",
      status: "healthy",
      reason_code: "SNAPSHOT_CURRENT",
      first_changed_at: null,
      last_changed_at: "2026-09-03T07:50:00Z",
      last_success_at: "2026-09-03T07:50:00Z",
      retry_count: 0,
      stale: false,
      gap: false,
    },
  ],
} satisfies HealthSnapshot;

export const statisticsFixture = {
  sampled_at: "2026-09-03T08:00:00Z",
  runtime_revision: "rev-42",
  page: 1,
  page_size: 20,
  total_items: 3,
  items: [
    { date: "2026-09-01", dimension_kind: "total", dimension_value: "all", count: 91_042 },
    { date: "2026-09-02", dimension_kind: "total", dimension_value: "all", count: 97_824 },
    { date: "2026-09-03", dimension_kind: "total", dimension_value: "all", count: 96_047 },
  ],
} satisfies StatisticsPage;

export const queryPageFixture = {
  sampled_at: "2026-09-03T08:00:00Z",
  runtime_revision: "rev-42",
  page: 1,
  page_size: 20,
  total_items: 3,
  items: [
    {
      id: "qry_01k45h8x",
      occurred_at: "2026-09-03T07:59:58Z",
      duration_ms: 1.42,
      transport: "udp",
      source: "cache",
      rcode: "NOERROR",
      outcome: "answered",
      cache: "hit",
      policy_matched: true,
      resource_matched: false,
    },
    {
      id: "qry_01k45h7q",
      occurred_at: "2026-09-03T07:59:51Z",
      duration_ms: 28.7,
      transport: "doh",
      source: "upstream",
      rcode: "NOERROR",
      outcome: "answered",
      cache: "miss",
      policy_matched: true,
      resource_matched: true,
    },
    {
      id: "qry_01k45h6m",
      occurred_at: "2026-09-03T07:59:43Z",
      duration_ms: 1_500,
      transport: "tcp",
      source: "upstream",
      rcode: "SERVFAIL",
      outcome: "timeout",
      cache: "bypass",
      policy_matched: false,
      resource_matched: false,
    },
  ],
} satisfies QueryPage;

export const resourceFixture = {
  sampled_at: "2026-09-03T08:00:00Z",
  runtime_revision: "rev-42",
  items: [
    { id: "resource_01", display_name: "内置 Hosts", epoch: "17", revision: "hosts-17", source_kind: "const", fallback: false, stale: false },
    { id: "resource_02", display_name: "本地规则集", epoch: "9", revision: "rules-9", source_kind: "file", fallback: false, stale: false },
    { id: "resource_03", display_name: "远程 Geosite", epoch: "31", revision: "geo-31", source_kind: "remote", fallback: true, stale: true },
  ],
} satisfies ResourceSnapshot;

export const systemFixture = {
  version: "0.1.0-dev",
  started_at: "2026-09-03T06:00:00Z",
  uptime_seconds: 7_200,
  capabilities: [
    "read:overview",
    "read:runtime",
    "read:health",
    "read:statistics",
    "read:queries",
    "read:resources",
    "read:system",
  ],
} satisfies SystemInfo;

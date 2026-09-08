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
import type { components as V2Components } from "@/shared/api/generated-v2";

type V2Schemas = V2Components["schemas"];

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
  resolution_pipeline: {
    accepted: 284_913,
    dropped: 2,
    gap_started_at_utc_millis: 1_788_425_900_000,
    cache_commit_stored: 37_412,
    cache_commit_rejected: 19,
    cache_commit_conflict: 3,
    cache_commit_unavailable: 1,
    cache_commit_dropped: 0,
    detail_accepted: 284_911,
    detail_dropped: 2,
    detail_failed: 0,
  },
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
  total_items: 4,
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
      duration_ms: 1,
      dns_core_duration_ms: 0.08,
      transport: "udp",
      source: "cache",
      rcode: "NOERROR",
      outcome: "answered",
      cache: "hit",
      policy_matched: true,
      resource_matched: false,
      detail_status: "available",
      qname: "cached.example.",
      qtype: "A",
      client_name: "office",
      client_ip: "192.0.2.10",
      strategy_id: "default",
      upstream_target_id: "public-dns",
      upstream_used_id: "alidns",
      answer_count: 2,
      answers_truncated: false,
      answers: [
        { name: "cached.example.", type: "CNAME", ttl: 60, data: "edge.example." },
        { name: "edge.example.", type: "A", ttl: 60, data: "192.0.2.20" },
      ],
    },
    {
      id: "qry_01k45h7q",
      occurred_at: "2026-09-03T07:59:51Z",
      duration_ms: 29,
      dns_core_duration_ms: 28.35,
      transport: "doh",
      source: "upstream",
      rcode: "NOERROR",
      outcome: "answered",
      cache: "miss",
      policy_matched: true,
      resource_matched: true,
      detail_status: "available",
      qname: "direct.example.",
      qtype: "AAAA",
      client_name: null,
      client_ip: "2001:db8::25",
      strategy_id: "privacy",
      upstream_target_id: "cloudflare",
      upstream_used_id: "cloudflare",
      answer_count: 20,
      answers_truncated: true,
      answers: [
        { name: "direct.example.", type: "AAAA", ttl: 120, data: "2001:db8::80" },
        { name: "direct.example.", type: "AAAA", ttl: 120, data: "2001:db8::81" },
      ],
    },
    {
      id: "qry_01k45h6m",
      occurred_at: "2026-09-03T07:59:43Z",
      duration_ms: 1_500,
      dns_core_duration_ms: 1_499.75,
      transport: "tcp",
      source: "upstream",
      rcode: "SERVFAIL",
      outcome: "timeout",
      cache: "bypass",
      policy_matched: false,
      resource_matched: false,
      detail_status: "available",
      qname: "timeout.example.",
      qtype: "HTTPS",
      client_name: null,
      client_ip: "192.0.2.30",
      strategy_id: "default",
      upstream_target_id: "fallback-group",
      upstream_used_id: null,
      answer_count: 0,
      answers_truncated: false,
      answers: [],
    },
    {
      id: "qry_legacy",
      occurred_at: "2026-09-03T07:59:40Z",
      duration_ms: 4,
      dns_core_duration_ms: null,
      transport: "udp",
      source: "hosts",
      rcode: "NOERROR",
      outcome: "answered",
      cache: "bypass",
      policy_matched: true,
      resource_matched: true,
      detail_status: "legacy_redacted",
      qname: null,
      qtype: "A",
      client_name: null,
      client_ip: null,
      strategy_id: null,
      upstream_target_id: null,
      upstream_used_id: null,
      answer_count: null,
      answers_truncated: null,
      answers: null,
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

export const processMetricsFixture = {
  sampled_at_ms: Date.parse("2026-09-03T08:00:00Z"),
  uptime_seconds: 7_200,
  rss_bytes: { state: "available", value: "195454566" },
  cpu_percent: { state: "available", value: 1.25 },
  threads: { state: "available", value: 18 },
} satisfies V2Schemas["ProcessMetrics"];

export const serviceMetricsFixture = {
  sampled_at_ms: Date.parse("2026-09-07T00:00:10Z"),
  qps: { state: "available", value: 4.25 },
  rpm: { state: "available", value: 255 },
  online_clients: { state: "available", value: 2 },
  rss_bytes: { state: "available", value: "195454566" },
  qps_trend: [
    { at_ms: Date.parse("2026-09-07T00:00:08Z"), value: { state: "available", value: 1.5 } },
    { at_ms: Date.parse("2026-09-07T00:00:09Z"), value: { state: "unavailable", reason: "observation_gap", observed_seconds: null } },
    { at_ms: Date.parse("2026-09-07T00:00:10Z"), value: { state: "available", value: 12.75 } },
  ],
  rpm_trend: [
    { at_ms: Date.parse("2026-09-07T00:00:00Z"), value: { state: "unavailable", reason: "warmup", observed_seconds: 300 } },
    { at_ms: Date.parse("2026-09-07T00:00:10Z"), value: { state: "available", value: 255 } },
  ],
} satisfies V2Schemas["ServiceMetrics"];

export const configStateFixture = {
  active_revision: "active-8",
  runtime_revision: "runtime-12",
  persisted_revision: "active-7",
  observed_file_revision: "files-10",
  files: { source: "changed", derived: "unchanged" },
  synchronization: "applied_unpersisted",
  operation_id: "op-123",
} satisfies V2Schemas["ConfigState"];

const synchronizedConfigStateFixture = {
  ...configStateFixture,
  persisted_revision: "active-8",
  files: { source: "unchanged", derived: "unchanged" },
  synchronization: "synced",
  operation_id: null,
} satisfies V2Schemas["ConfigState"];

export const dnsConfigReadFixture = {
  state: synchronizedConfigStateFixture,
  values: [{
    module: "dns",
    value: {
      cache: {
        enabled: true,
        memory: { max_size_bytes: 67_108_864 },
        failure_ttl: "5000000000ns",
        optimistic: { enabled: true, answer_ttl: "10000000000ns", max_age: "86400000000000ns" },
        persistence: { enabled: true, path: "./data/dns-cache.fdcs", snapshot_interval: "300000000000ns" },
      },
      resolve_log: { enable: true },
    },
  }],
  effective: [
    { path: "dns.cache.enabled", source: "global", value: true },
    { path: "dns.cache.memory.max_size_bytes", source: "global", value: 67_108_864 },
  ],
  references: [],
  runtime: [{
    module: "dns",
    snapshot: {
      state: "idle",
      owner_revision: "owner-1",
      generation: "3",
      file_bytes: "32768",
      last_success_at_ms: Date.parse("2026-09-07T00:00:00Z"),
      last_error: null,
    },
  }],
} satisfies V2Schemas["ConfigRead"];

export const statisticsConfigReadFixture = {
  state: synchronizedConfigStateFixture,
  values: [{
    module: "statistics",
    value: { retention: { days: 7, grace_days: 3, reference_size_bytes: 1_073_741_824 } },
  }],
  effective: [
    { path: "statistics.retention.days", source: "global", value: 7 },
    { path: "statistics.retention.grace_days", source: "global", value: 3 },
  ],
  references: [],
  runtime: [],
} satisfies V2Schemas["ConfigRead"];

export const logsConfigReadFixture = {
  state: synchronizedConfigStateFixture,
  values: [{ module: "logs", value: { enable: true, level: "info", path: "./logs/fluxdns.log" } }],
  effective: [],
  references: [],
  runtime: [],
} satisfies V2Schemas["ConfigRead"];

export const outboundConfigReadFixture = {
  state: synchronizedConfigStateFixture,
  values: [
    { module: "outbound", value: { name: "proxy-primary", type: "socks5", proxy_url: { env: "PROXY_URL" } } },
    { module: "outbound", value: { name: "proxy-backup", type: "socks5", proxy_url: { file: "./secrets/proxy.txt" } } },
  ],
  effective: [],
  references: [
    { from_module: "upstreams", from_name: "secure-dns", path: "proxy", to_name: "proxy-primary" },
    { from_module: "rule_set", from_name: "domains", path: "proxy", to_name: "proxy-primary" },
  ],
  runtime: [],
} satisfies V2Schemas["ConfigRead"];

export const systemConfigReadFixture = {
  state: synchronizedConfigStateFixture,
  work_path: "D:/Projects/Rust/fluxdns/_fluxdns",
  rules_path: "D:/Projects/Rust/fluxdns/_fluxdns/rules",
  database_path: "D:/Projects/Rust/fluxdns/_fluxdns/statistics.db",
  records_path: "D:/Projects/Rust/fluxdns/_fluxdns/queries",
  webui_enabled: true,
  webui_address: "127.0.0.1",
  webui_port: 8080,
  public_origin: "http://127.0.0.1:8080",
} satisfies V2Schemas["SystemConfigRead"];

export const retentionStatusFixture = {
  policy: { retention: { days: 7, grace_days: 3, reference_size_bytes: 1_073_741_824 } },
  sampled_at_ms: Date.parse("2026-09-07T01:00:10Z"),
  detail_bytes: "805306368",
  cutoff_utc_date: "2026-08-28",
  last_completed_at_ms: Date.parse("2026-09-07T01:00:05Z"),
  next_scheduled_at_ms: Date.parse("2026-09-08T01:00:00Z"),
  pending_reclaim_bytes: "16777216",
} satisfies V2Schemas["RetentionStatus"];

export const v2QueryRecordsFixture = [
  {
    id: "2026-09-07.18",
    occurred_at_ms: Date.parse("2026-09-07T00:00:01Z"),
    identity: { client_id: "unknown-id", client_ip: "192.0.2.10" },
    matched: { source: "ip", matched_client_id: "Desktop-01" },
    current_client_name: "workstation",
    qname: "example.test.",
    qtype: "A",
    transport: "doh",
    rcode: "NOERROR",
    source: "upstream",
    outcome: "answered",
    cache: "miss",
    strategy_name: "default",
    upstream_target_name: "public",
    upstream_used_name: "public-1",
    cache_producer: null,
    duration_us: 123,
    dns_core_duration_us: 100,
    answers: {
      state: "available",
      total_count: 1,
      records: [{ name: "example.test.", type: "A", ttl_seconds: 30, data: "192.0.2.1" }],
    },
  },
  {
    id: "2026-09-07.17",
    occurred_at_ms: Date.parse("2026-09-07T00:00:01Z"),
    identity: { client_id: null, client_ip: "192.0.2.20" },
    matched: { source: "none" },
    current_client_name: null,
    qname: "cached.example.test.",
    qtype: "AAAA",
    transport: "udp",
    rcode: "NOERROR",
    source: "cache",
    outcome: "answered",
    cache: "stale",
    strategy_name: "default",
    upstream_target_name: null,
    upstream_used_name: null,
    cache_producer: {
      strategy_name: "default",
      upstream_target_name: "public",
      upstream_used_name: "public-2",
    },
    duration_us: 42,
    dns_core_duration_us: 30,
    answers: { state: "truncated", total_count: 20, records: [] },
  },
] satisfies V2Schemas["QueryRecord"][];

export const v2QueryPageFixture = {
  items: v2QueryRecordsFixture,
  previous_cursor: null,
  next_cursor: "cursor:older:2026-09-07.17",
  snapshot_cursor: { epoch: "stream-1", sequence: "42" },
  directory_revision: "clients-12",
  retention_revision: "retention-9",
  available_from_ms: Date.parse("2026-08-28T00:00:00Z"),
} satisfies V2Schemas["QueryPage"];

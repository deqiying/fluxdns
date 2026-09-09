import type { Session, SetupStatus } from "@/shared/api/types";
import type { components as V2Components } from "@/shared/api/generated-v2";

type V2Schemas = V2Components["schemas"];

export const setupReadyFixture = { state: "ready" } satisfies SetupStatus;
export const setupRequiredFixture = { state: "required" } satisfies SetupStatus;

export const sessionFixture = {
  user: { name: "operator" },
  expires_at: "2026-09-03T12:30:00Z",
} satisfies Session;

export const processMetricsFixture = {
  version: "0.1.1",
  started_at_ms: 1_788_900_000_000,
  sampled_at_ms: Date.parse("2026-09-03T08:00:00Z"),
  uptime_seconds: 7_200,
  rss_bytes: { state: "available", value: "195454566" },
  cpu_percent: { state: "available", value: 1.25 },
  threads: { state: "available", value: 18 },
} satisfies V2Schemas["ProcessMetrics"];

const serviceMetricsSampledAt = Date.now();

export const serviceMetricsFixture = {
  sampled_at_ms: serviceMetricsSampledAt,
  qps: { state: "available", value: 4.25 },
  rpm: { state: "available", value: 255 },
  online_clients: { state: "available", value: 2 },
  rss_bytes: { state: "available", value: "195454566" },
  qps_trend: [
    { at_ms: serviceMetricsSampledAt - 2_000, value: { state: "available", value: 1.5 } },
    { at_ms: serviceMetricsSampledAt - 1_000, value: { state: "unavailable", reason: "observation_gap", observed_seconds: null } },
    { at_ms: serviceMetricsSampledAt, value: { state: "available", value: 12.75 } },
  ],
  rpm_trend: [
    { at_ms: serviceMetricsSampledAt - 60_000, value: { state: "unavailable", reason: "warmup", observed_seconds: 300 } },
    { at_ms: serviceMetricsSampledAt, value: { state: "available", value: 255 } },
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

export const hostsConfigReadFixture = {
  state: synchronizedConfigStateFixture,
  values: [
    { module: "hosts", value: { name: "local", type: "const", format: "hosts", hosts: "127.0.0.1 localhost\n192.0.2.20 printer.lan" } },
    { module: "hosts", value: { name: "office", type: "file", format: "json", path: "./rules/office-hosts.json", auto_update: true, update_interval: "300000000000ns" } },
  ],
  effective: [],
  references: [{ from_module: "strategy", from_name: "default", path: "rules[0].hosts", to_name: "local" }],
  runtime: [
    { module: "hosts", name: "local", condition: "ready", last_updated_at_ms: Date.parse("2026-09-08T00:00:00Z"), next_update_at_ms: null, error: null },
    { module: "hosts", name: "office", condition: "stale", last_updated_at_ms: Date.parse("2026-09-07T23:55:00Z"), next_update_at_ms: Date.parse("2026-09-08T00:05:00Z"), error: "APPLY_FAILED" },
  ],
} satisfies V2Schemas["ConfigRead"];

export const ruleSetsConfigReadFixture = {
  state: synchronizedConfigStateFixture,
  values: [
    { module: "rule_set", value: { name: "domains", type: "remote", format: "json", url: "https://rules.example.test/domains.json", proxy: "proxy-primary", auto_update: true, update_interval: "86400000000000ns" } },
    { module: "rule_set", value: { name: "custom", type: "const", format: "clash", rule: "+.example.test\n+.example.org" } },
  ],
  effective: [],
  references: [{ from_module: "strategy", from_name: "default", path: "rules[1].rule_set", to_name: "domains" }],
  runtime: [
    { module: "rule_set", name: "domains", condition: "stale", last_updated_at_ms: Date.parse("2026-09-07T00:00:00Z"), next_update_at_ms: Date.parse("2026-09-09T00:00:00Z"), error: "APPLY_FAILED" },
    { module: "rule_set", name: "custom", condition: "ready", last_updated_at_ms: null, next_update_at_ms: null, error: null },
  ],
} satisfies V2Schemas["ConfigRead"];

export const upstreamsConfigReadFixture = {
  state: synchronizedConfigStateFixture,
  values: [
    { module: "upstreams", value: { name: "local", type: "hosts", format: "hosts", hosts: "127.0.0.1 localhost" } },
    { module: "upstreams", value: { name: "secure-dns", type: "doh", address: "https://dns.example.test/dns-query", bootstrap: "local", proxy: "proxy-primary", edns_client_subnet: { mode: "disabled" } } },
    { module: "upstreams", value: { name: "default-group", type: "group", upstreams: [{ name: "secure-dns", weight: 2 }, { name: "local", weight: 1 }], upstream_mode: "load-balance", timeout: "5000000000ns", fallbacks: [{ name: "local", weight: 1 }], fallback_upstream_mode: "failover", fallback_timeout: "3000000000ns" } },
  ],
  effective: [],
  references: [{ from_module: "strategy", from_name: "default", path: "default_upstream", to_name: "default-group" }],
  runtime: [],
} satisfies V2Schemas["ConfigRead"];

export const strategiesConfigReadFixture = {
  state: synchronizedConfigStateFixture,
  values: [{
    module: "strategy",
    value: {
      name: "default",
      default_upstream: "default-group",
      rules: [{ hosts: "local" }, { rule_set: "domains", upstream: "secure-dns" }],
      cache: { enabled: true },
      ttl_override: { enabled: true, min: "30000000000ns", max: "3600000000000ns" },
      edns_client_subnet: { mode: "disabled" },
    },
  }],
  effective: [{ path: "strategy.default.cache.enabled", source: "strategy", value: true }],
  references: [{ from_module: "listener", from_name: "local", path: "strategy", to_name: "default" }],
  runtime: [],
} satisfies V2Schemas["ConfigRead"];

export const listenersConfigReadFixture = {
  state: synchronizedConfigStateFixture,
  values: [
    { module: "listener", value: { name: "local", type: "udp", addresses: ["127.0.0.1"], port: 15353, strategy: "default", hosts: "local" } },
    { module: "listener", value: { name: "https", type: "doh", routes: [{ path: "/dns-query", strategy: "default" }], endpoints: [{ name: "loopback", addresses: ["127.0.0.1"], port: 18443, tls: { mode: "external" }, client_ip: { source: "forwarded_header", header: "X-Forwarded-For", trusted_proxies: ["127.0.0.1/32"], on_missing: "reject", on_invalid: "reject" } }] } },
  ],
  effective: [],
  references: [],
  runtime: [
    { module: "listener", name: "local", bindings: [{ endpoint_name: null, address: "127.0.0.1", port: 15353, transport: "udp", accepting: true }] },
    { module: "listener", name: "https", bindings: [{ endpoint_name: "loopback", address: "127.0.0.1", port: 18443, transport: "doh", accepting: true }] },
  ],
} satisfies V2Schemas["ConfigRead"];

export const clientsConfigReadFixture = {
  state: synchronizedConfigStateFixture,
  values: [
    { module: "clients", value: { name: "desktop", client_id: "Desktop-01", match: { ips: ["192.0.2.10", "2001:db8::/64"] }, strategy: "default", cache: { enabled: true }, edns_client_subnet: { mode: "disabled" } } },
    { module: "clients", value: { name: "mobile", client_id: "Mobile-01", match: { ips: [] } } },
  ],
  effective: [{ path: "clients.desktop.cache.enabled", source: "client", value: true }],
  references: [],
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
  retention_revision: "9",
  available_from_ms: Date.parse("2026-08-28T00:00:00Z"),
} satisfies V2Schemas["QueryPage"];

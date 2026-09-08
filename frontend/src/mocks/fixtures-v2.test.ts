import { beforeEach, expect, it } from "vitest";
import { getDnsConfig, getRetentionStatus, getStatisticsConfig } from "@/modules/dns-settings/api";
import { getLogsConfig, getSystemConfig } from "@/modules/system-settings/api";
import { apiV2Request } from "@/shared/api/client";
import type { components } from "@/shared/api/generated-v2";
import { fetchConfigState } from "@/shared/config/api";
import { setMockAuthenticated } from "./handlers";

type Schemas = components["schemas"];

beforeEach(() => setMockAuthenticated(true));

it("提供服务指标的峰值、缺口和在线身份固定契约", async () => {
  const metrics = await apiV2Request<Schemas["ServiceMetrics"]>("/service/metrics");

  expect(metrics.online_clients).toEqual({ state: "available", value: 2 });
  expect(metrics.qps_trend.some(({ value }) => value.state === "unavailable" && value.reason === "observation_gap")).toBe(true);
  expect(metrics.qps_trend.at(-1)?.value).toEqual({ state: "available", value: 12.75 });
});

it("提供同毫秒稳定 ID、历史身份和缓存生产者固定契约", async () => {
  const request = {
    filter: { from_ms: Date.parse("2026-09-06T00:00:00Z"), to_ms: Date.parse("2026-09-08T00:00:00Z") },
    cursor: null,
    direction: "older",
    page_size: 20,
    sort: "occurred_at",
    order: "desc",
  } satisfies Schemas["QueryRequest"];
  const page = await apiV2Request<Schemas["QueryPage"]>("/queries/search", { method: "POST", body: request });

  expect(page.items.map(({ id }) => id)).toEqual(["2026-09-07.18", "2026-09-07.17"]);
  expect(page.items[0]).toMatchObject({
    identity: { client_id: "unknown-id", client_ip: "192.0.2.10" },
    matched: { source: "ip", matched_client_id: "Desktop-01" },
    current_client_name: "workstation",
  });
  expect(page.items[1].cache_producer).toMatchObject({ upstream_used_name: "public-2" });

  const detail = await apiV2Request<Schemas["QueryDetail"]>(`/queries/${page.items[0].id}`);
  expect(detail.record.id).toBe(page.items[0].id);
  expect(detail.directory_revision).toBe(page.directory_revision);
});

it("提供 DNS、统计、保留与系统只读状态且不回显认证秘密", async () => {
  const [state, dns, statistics, retention, system, logs] = await Promise.all([
    fetchConfigState(),
    getDnsConfig(),
    getStatisticsConfig(),
    getRetentionStatus(),
    getSystemConfig(),
    getLogsConfig(),
  ]);

  expect(state).toMatchObject({ synchronization: "applied_unpersisted", files: { source: "changed" } });
  expect(dns.values).toHaveLength(1);
  expect(dns.values[0].module).toBe("dns");
  expect(statistics.values[0].module).toBe("statistics");
  expect(retention.policy.retention).toEqual({ days: 7, grace_days: 3, reference_size_bytes: 1_073_741_824 });
  expect(logs.values[0].module).toBe("logs");

  const serialized = JSON.stringify(system);
  expect(serialized).not.toContain("users");
  expect(serialized).not.toContain("password_hash");
  expect(serialized).not.toContain("FLUXDNS_");
});

it("v2 固定路由保留严格错误 envelope", async () => {
  await expect(apiV2Request("/queries/search", {
    method: "POST",
    body: { filter: {}, cursor: null, direction: "older", page_size: 20, sort: "occurred_at", order: "desc" },
  })).rejects.toMatchObject({ code: "INVALID_ARGUMENT", status: 400, requestId: "mock-v2-400" });

  await expect(apiV2Request("/config/modules/upstreams"))
    .rejects.toMatchObject({ code: "NOT_FOUND", status: 404, requestId: "mock-v2-404" });
});

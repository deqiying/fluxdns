import { describe, expect, it } from "vitest";
import {
  healthFixture,
  overviewFixture,
  queryPageFixture,
  resourceFixture,
  runtimeFixture,
  statisticsFixture,
  systemFixture,
} from "@/mocks/fixtures";
import { queryRecordKeys } from "@/modules/queries/api";
import { getQueries } from "@/modules/queries/api";
import { statisticsKeys } from "@/modules/statistics/api";
import { setMockAuthenticated } from "@/mocks/handlers";

const forbiddenFields = new Set([
  "canonical_qname",
  "request_digest",
  "client_id",
  "secret_ref",
  "password_hash",
  "raw_wire",
  "headers",
]);

function collectKeys(value: unknown, result = new Set<string>()): Set<string> {
  if (Array.isArray(value)) {
    value.forEach((item) => collectKeys(item, result));
  } else if (typeof value === "object" && value !== null) {
    Object.entries(value).forEach(([key, child]) => {
      result.add(key.toLowerCase());
      collectKeys(child, result);
    });
  }
  return result;
}

describe("management API fixtures", () => {
  it("所有投影都不包含契约禁止的内部字段", () => {
    const keys = collectKeys([
      overviewFixture,
      runtimeFixture,
      healthFixture,
      statisticsFixture,
      queryPageFixture,
      resourceFixture,
      systemFixture,
    ]);
    forbiddenFields.forEach((field) => expect(keys.has(field), `${field} 不应出现在 fixture`).toBe(false));
  });

  it("只有 authenticated queries 投影包含完整查询详情", () => {
    const nonQueryKeys = collectKeys([
      overviewFixture,
      runtimeFixture,
      healthFixture,
      statisticsFixture,
      resourceFixture,
      systemFixture,
    ]);
    ["qname", "client_ip", "answers"].forEach((field) => expect(nonQueryKeys.has(field)).toBe(false));

    const available = queryPageFixture.items.find((item) => item.detail_status === "available");
    expect(available).toMatchObject({
      qname: "cached.example.",
      client_name: "office",
      client_ip: "192.0.2.10",
      strategy_id: "default",
      upstream_target_id: "public-dns",
      upstream_used_id: "alidns",
      answer_count: 2,
      answers_truncated: false,
    });
    expect(available?.answers?.[0]).toEqual({
      name: "cached.example.",
      type: "CNAME",
      ttl: 60,
      data: "edge.example.",
    });
    const legacy = queryPageFixture.items.find((item) => item.detail_status === "legacy_redacted");
    expect(legacy).toMatchObject({ qname: null, client_ip: null, answers: null });
  });

  it("query key 包含全部服务端参数", () => {
    const base = { page: 1, pageSize: 20, sort: "occurred_at", order: "desc" } as const;
    expect(queryRecordKeys.list(base)).not.toEqual(queryRecordKeys.list({ ...base, page: 2 }));
    expect(queryRecordKeys.list(base)).not.toEqual(queryRecordKeys.list({ ...base, transport: "udp" }));

    const statistics = { dateFrom: "2026-09-01", dateTo: "2026-09-03", dimension: "total", page: 1, pageSize: 20 } as const;
    expect(statisticsKeys.list(statistics)).not.toEqual(statisticsKeys.list({ ...statistics, dimension: "rcode" }));
  });

  it("mock contract 拒绝越界页大小", async () => {
    setMockAuthenticated(true);
    await expect(
      getQueries({ page: 1, pageSize: 101, sort: "occurred_at", order: "desc" }),
    ).rejects.toMatchObject({ status: 400, code: "INVALID_ARGUMENT" });
  });
});

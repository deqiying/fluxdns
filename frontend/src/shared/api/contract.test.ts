import { describe, expect, it } from "vitest";
import { queryRecordKeys, getQueries } from "@/modules/queries/api";
import { setMockAuthenticated } from "@/mocks/handlers";

describe("v2 解析查询契约", () => {
  it("query key 包含全部服务端参数", () => {
    const base = { filter: { from_ms: 1, to_ms: 2 }, cursor: null, direction: "older", page_size: 20, sort: "occurred_at", order: "desc" } as const;
    expect(queryRecordKeys.list(base)).not.toEqual(queryRecordKeys.list({ ...base, cursor: "opaque-next" }));
    expect(queryRecordKeys.list(base)).not.toEqual(queryRecordKeys.list({ ...base, filter: { ...base.filter, transport: "udp" } }));

  });

  it("mock contract 拒绝越界页大小", async () => {
    setMockAuthenticated(true);
    await expect(
      getQueries({ filter: { from_ms: 1, to_ms: 2 }, cursor: null, direction: "older", page_size: 101, sort: "occurred_at", order: "desc" }),
    ).rejects.toMatchObject({ status: 400, code: "INVALID_ARGUMENT" });
  });
});

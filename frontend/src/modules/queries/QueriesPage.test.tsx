import { describe, expect, it } from "vitest";
import { queryPageFixture } from "@/mocks/fixtures";
import { formatClient, formatResponseSummary, formatRoute } from "./QueriesPage";

describe("QueriesPage 展示语义", () => {
  const [cache, direct, timeout, legacy] = queryPageFixture.items;

  it("区分 cache producer、direct 与未归因 group", () => {
    expect(formatRoute(cache)).toBe("缓存来源：public-dns → alidns");
    expect(formatRoute(direct)).toBe("cloudflare");
    expect(formatRoute(timeout)).toBe("fallback-group → 未确定");
    expect(formatRoute(legacy)).toBe("记录产生时未保留路由");
  });

  it("客户端名称优先并在未命中时回退有效 IP", () => {
    expect(formatClient(cache)).toEqual({ primary: "office", secondary: "192.0.2.10", muted: false });
    expect(formatClient(direct)).toEqual({ primary: "2001:db8::25", secondary: undefined, muted: false });
    expect(formatClient(legacy)).toEqual({ primary: "未知客户端", secondary: undefined, muted: true });
  });

  it("显示首条 answer、截断总数和历史不可用状态", () => {
    expect(formatResponseSummary(direct)).toEqual({
      primary: "AAAA  2001:db8::80",
      meta: "仅保留 2 条，共 20 条",
    });
    expect(formatResponseSummary(timeout)).toEqual({ primary: "SERVFAIL · timeout", meta: "0 条结果" });
    expect(formatResponseSummary(legacy)).toEqual({ primary: "NOERROR · answered", meta: "结果未保留" });
  });
});

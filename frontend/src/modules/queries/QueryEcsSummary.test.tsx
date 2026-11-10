import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import type { components } from "@/shared/api/generated-v2";
import { formatCacheMatch, formatEcsSummary, formatEcsValue, QueryEcsDiagnostics, QueryEcsSummary } from "./QueryEcsSummary";

type Ecs = components["schemas"]["QueryEcs"];
type Exchange = components["schemas"]["EcsExchange"];
const observed = {
  state: "observed", query: { state: "present", cidr: "203.0.113.0/24", scope_prefix: 0 },
  response: { state: "present", cidr: "203.0.113.0/24", scope_prefix: 20 },
  policy: { source: "global", mode: "client", value_source: "client_ip" }, upstream_leaf_name: "inner",
} as const satisfies Exchange;
const ecs = {
  request: { state: "present", cidr: "203.0.113.42/32", scope_prefix: 0 },
  upstream: observed, cache_match: null, refresh: null,
} as const satisfies Ecs;

describe("ECS 四组执行事实", () => {
  it("缺省、null 与未观察绝不补算禁用或客户端子网", () => {
    expect(formatEcsSummary()).toEqual({ primary: "未记录", secondary: "" });
    expect(formatEcsSummary(null).primary).toBe("未记录");
    for (const reason of ["not_observed", "no_selected_response", "deadline", "budget"] as const) {
      expect(formatEcsSummary({ ...ecs, upstream: { state: "unrecorded", reason } }).primary).toBe("未记录");
    }
    expect(formatEcsValue({ state: "absent" })).toBe("未携带 ECS");
    expect(formatEcsValue({ state: "invalid" })).toBe("无效 ECS");
    expect(formatEcsValue({ state: "unrecorded" })).toBe("未记录");
  });

  it("前台实际 query 与原始输入分开；Scope 只读真实 response", () => {
    const summary = formatEcsSummary(ecs);
    expect(summary.primary).toBe("203.0.113.0/24 · 客户端地址 · 全局");
    expect(summary.secondary).toBe("用于上游请求 · 响应 Scope /20");
    expect(summary.primary).not.toContain("/32");
    for (const response of [{ state: "absent" }, { state: "unrecorded" }, { state: "invalid" }] as const) {
      expect(formatEcsSummary({ ...ecs, upstream: { ...observed, response } }).secondary).not.toContain("Scope");
    }
    render(<QueryEcsDiagnostics ecs={ecs} />);
    expect(screen.getByLabelText("ECS 诊断")).toHaveTextContent("原始请求 ECS：203.0.113.42/32");
    expect(screen.getByLabelText("ECS 诊断")).toHaveTextContent("实际 leaf：inner");
    expect(screen.getByLabelText("ECS 诊断")).toHaveTextContent("后台刷新：无关联刷新");
  });

  it("禁用 ECS 不等于 DNS 未请求上游，六种配置来源保持可见", () => {
    for (const source of ["default", "global", "upstream", "client", "strategy", "rule"] as const) {
      const value: Ecs = { ...ecs, upstream: { ...observed, query: { state: "absent" }, policy: { source, mode: "disabled", value_source: "none" } } };
      expect(formatEcsSummary(value).primary).toContain("未携带 ECS");
      expect(formatEcsSummary(value).primary).toContain("禁用");
      expect(formatEcsSummary(value).secondary).toContain("用于上游请求");
    }
  });

  it("缓存 /0 精确、覆盖、无 ECS 桶与异构成员不能混同", () => {
    expect(formatCacheMatch({ kind: "covering", cidr: "0.0.0.0/0" })).toBe("覆盖匹配 0.0.0.0/0");
    expect(formatCacheMatch({ kind: "exact", cidr: "0.0.0.0/0" })).toBe("精确匹配 0.0.0.0/0");
    expect(formatCacheMatch({ kind: "no_ecs", cidr: null })).toBe("无 ECS 桶");
    expect(formatCacheMatch({ kind: "members", cidr: null })).toBe("异构成员组合匹配");
    const cached: Ecs = { ...ecs, upstream: { state: "not_sent", reason: "cache_hit" }, cache_match: { kind: "covering", cidr: "203.0.112.0/20" } };
    expect(formatEcsSummary(cached)).toEqual({ primary: "本次未发送", secondary: "命中缓存 · 覆盖匹配 203.0.112.0/20" });
  });

  it("刷新 IPv6 优先但明确后台场景，旧缓存范围与刷新 Scope 独立", () => {
    const refreshed: Ecs = {
      ...ecs, upstream: { state: "not_sent", reason: "cache_hit" },
      cache_match: { kind: "exact", cidr: "203.0.113.0/24" },
      refresh: { ...observed, query: { state: "present", cidr: "2001:db8::/56", scope_prefix: 0 }, response: { state: "present", cidr: "2001:db8::/56", scope_prefix: 48 }, policy: { source: "rule", mode: "custom", value_source: "custom" } },
    };
    render(<QueryEcsSummary ecs={refreshed} />);
    const summary = screen.getByLabelText("ECS 摘要");
    expect(summary).toHaveTextContent("2001:db8::/56 · 自定义 · 规则");
    expect(summary).toHaveTextContent("后台刷新使用 · 响应 Scope /48 · 精确匹配 203.0.113.0/24");
    expect(summary).not.toHaveTextContent("用于上游请求");
    expect(formatEcsSummary({ ...ecs, refresh: { state: "unrecorded", reason: "deadline" } }).primary).toContain("203.0.113.0/24");
  });

  it.each(["client_ecs", "custom"] as const)("请求 ECS 优先模式按真实 value_source=%s 展示，不推导客户端 IP", (valueSource) => {
    const value: Ecs = { ...ecs, request: valueSource === "custom" ? { state: "invalid" } : ecs.request, upstream: { ...observed, policy: { source: "strategy", mode: "client_or_custom", value_source: valueSource } } };
    expect(formatEcsSummary(value).primary).toBe(`203.0.113.0/24 · ${valueSource === "client_ecs" ? "请求携带" : "自定义"} · 策略`);
    render(<QueryEcsDiagnostics ecs={value} />);
    const diagnostic = screen.getByLabelText("ECS 诊断");
    expect(diagnostic).toHaveTextContent(`策略 · client_or_custom · ${valueSource}`);
    expect(diagnostic).toHaveTextContent(valueSource === "client_ecs" ? "使用合法请求 ECS" : "使用自定义回退地址");
    expect(diagnostic).not.toHaveTextContent("客户端地址");
  });

  it("本地应答、共享请求与尚未启动保留独立原因", () => {
    expect(formatEcsSummary({ ...ecs, upstream: { state: "not_sent", reason: "local_answer" } })).toEqual({ primary: "不适用", secondary: "本地应答" });
    expect(formatEcsSummary({ ...ecs, upstream: { state: "not_sent", reason: "single_flight" } }).primary).toBe("本次未独立请求上游");
    expect(formatEcsSummary({ ...ecs, upstream: { state: "not_sent", reason: "not_started" } }).secondary).toBe("未启动");
  });
});

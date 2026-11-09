import type { components } from "@/shared/api/generated-v2";

type QueryEcs = components["schemas"]["QueryEcs"];
type EcsValue = components["schemas"]["EcsValue"];
type EcsExchange = components["schemas"]["EcsExchange"];
type EcsCacheMatch = components["schemas"]["EcsCacheMatch"];

const sourceLabels = { default: "默认", global: "全局", upstream: "上游", client: "客户端", strategy: "策略", rule: "规则" };
const valueLabels = { client_ecs: "请求携带", client_ip: "客户端地址", custom: "自定义", none: "未携带" };
const reasonLabels = {
  cache_hit: "命中缓存", local_answer: "本地应答", single_flight: "共享请求", not_started: "未启动",
  not_observed: "未观察", no_selected_response: "无可靠选中应答", deadline: "观测超时", budget: "容量限制",
};

/** 只格式化记录事实，不按客户端地址、配置或响应 Scope 推导子网。 */
export function formatEcsValue(value: EcsValue): string {
  switch (value.state) {
    case "present": return `${value.cidr} · Scope /${value.scope_prefix}`;
    case "absent": return "未携带 ECS";
    case "invalid": return "无效 ECS";
    case "unrecorded": return "未记录";
  }
}

export function formatCacheMatch(match: EcsCacheMatch | null): string | null {
  if (!match) return null;
  switch (match.kind) {
    case "covering": return `覆盖匹配 ${match.cidr}`;
    case "exact": return `精确匹配 ${match.cidr}`;
    case "no_ecs": return "无 ECS 桶";
    case "members": return "异构成员组合匹配";
  }
}

/** 刷新与前台交换独立呈现；未知刷新不能覆盖已知前台值。 */
export function formatEcsSummary(ecs?: QueryEcs | null): { primary: string; secondary: string } {
  if (!ecs) return { primary: "未记录", secondary: "" };
  const refreshObserved = ecs.refresh?.state === "observed";
  const exchange = refreshObserved ? ecs.refresh! : ecs.upstream;
  const cache = formatCacheMatch(ecs.cache_match);
  if (exchange.state === "observed") {
    const query = exchange.query;
    const primary = query.state === "present" ? query.cidr : formatEcsValue(query);
    const source = exchange.policy.mode === "disabled"
      ? `${sourceLabels[exchange.policy.source]}禁用`
      : `${valueLabels[exchange.policy.value_source]} · ${sourceLabels[exchange.policy.source]}`;
    const scope = exchange.response.state === "present" ? ` · 响应 Scope /${exchange.response.scope_prefix}` : "";
    return { primary: `${primary} · ${source}`, secondary: `${refreshObserved ? "后台刷新使用" : "用于上游请求"}${scope}${cache ? ` · ${cache}` : ""}` };
  }
  if (exchange.state === "unrecorded") return { primary: "未记录", secondary: reasonLabels[exchange.reason] };
  const primary = exchange.reason === "local_answer" ? "不适用"
    : exchange.reason === "single_flight" ? "本次未独立请求上游" : "本次未发送";
  return { primary, secondary: [reasonLabels[exchange.reason], cache].filter(Boolean).join(" · ") };
}

export function QueryEcsSummary({ ecs }: { ecs?: QueryEcs | null }) {
  const summary = formatEcsSummary(ecs);
  return <section className="query-ecs-summary" aria-label="ECS 摘要">
    <strong>ECS</strong><div>{summary.primary}</div>
    {summary.secondary ? <small>{summary.secondary}</small> : null}
  </section>;
}

function ExchangeDiagnostic({ label, exchange }: { label: string; exchange: EcsExchange }) {
  if (exchange.state !== "observed") return <div>{label}：{exchange.state === "not_sent" ? "未发送" : "未记录"} · {reasonLabels[exchange.reason]}</div>;
  return <div className="query-ecs-exchange">
    <strong>{label}</strong>
    <div>上游请求 ECS：{formatEcsValue(exchange.query)}</div>
    <div>上游应答 ECS：{formatEcsValue(exchange.response)}</div>
    <div>配置来源：{sourceLabels[exchange.policy.source]} · {exchange.policy.mode} · {exchange.policy.value_source}</div>
    <div>实际 leaf：{exchange.upstream_leaf_name}</div>
  </div>;
}

export function QueryEcsDiagnostics({ ecs }: { ecs?: QueryEcs | null }) {
  if (!ecs) return <div>ECS：未记录</div>;
  return <section className="query-ecs-diagnostics" aria-label="ECS 诊断">
    <div>原始请求 ECS：{formatEcsValue(ecs.request)}</div>
    <ExchangeDiagnostic label="本次上游" exchange={ecs.upstream} />
    <div>缓存匹配：{formatCacheMatch(ecs.cache_match) ?? "无缓存匹配"}</div>
    {ecs.refresh ? <ExchangeDiagnostic label="后台刷新" exchange={ecs.refresh} /> : <div>后台刷新：无关联刷新</div>}
  </section>;
}

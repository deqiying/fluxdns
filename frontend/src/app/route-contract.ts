import type { components } from "@/shared/api/generated-v2";

type ConfigModule = components["schemas"]["ConfigModule"];

/** P0 路由契约，不等于 App 已注册页面；正式壳层接线属于 FC-01。 */
export const managementRoutes = [
  { path: "/dashboard", title: "服务状态", group: "monitor", modules: [] },
  { path: "/queries", title: "解析记录", group: "monitor", modules: [] },
  { path: "/listeners", title: "监听入口", group: "dns", modules: ["listener"] },
  { path: "/upstreams", title: "DNS 上游", group: "dns", modules: ["upstreams"] },
  { path: "/dns-settings", title: "DNS 配置", group: "dns", modules: ["dns", "statistics"] },
  { path: "/strategies", title: "DNS 分流策略", group: "dns", modules: ["strategy"] },
  { path: "/hosts", title: "Hosts 配置", group: "dns", modules: ["hosts"] },
  { path: "/rule-sets", title: "规则集", group: "dns", modules: ["rule_set"] },
  { path: "/clients", title: "客户端配置", group: "dns", modules: ["clients"] },
  { path: "/proxies", title: "代理配置", group: "system", modules: ["outbound"] },
  { path: "/system-settings", title: "系统配置", group: "system", modules: ["logs"] },
  { path: "/system-runtime", title: "系统运行状态", group: "system", modules: [] },
] as const satisfies ReadonlyArray<{
  path: string;
  title: string;
  group: "monitor" | "dns" | "system";
  modules: readonly ConfigModule[];
}>;

export type ManagementPath = (typeof managementRoutes)[number]["path"];
export const upstreamTabs = ["upstreams", "groups"] as const;

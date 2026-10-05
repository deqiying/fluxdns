import type { components } from "@/shared/api/generated-v2";
import { durationToNanoseconds, normalizeDuration } from "@/shared/config/form-values";

type Schemas = components["schemas"];
export type Client = Schemas["Client"];
type Listener = Schemas["Listener"];

/** 列表“覆盖项”列只展示非继承项，缓存 / TTL / ECS 各至多一条。 */
export interface ClientOverride {
  key: "cache" | "ttl" | "ecs";
  label: string;
  value: string;
}

export interface ClientSummary {
  total: number;
  /** 未配置 IP/CIDR、只能靠精确 client_id 命中的客户端数。 */
  idOnly: number;
  customStrategy: number;
  withOverrides: number;
  ipEntries: number;
  clientsWithIps: number;
}

const CLIENT_ID_PLACEHOLDER = "{client_id}";

export function clientOverrides(client: Client): ClientOverride[] {
  const overrides: ClientOverride[] = [];
  if (client.cache) overrides.push({ key: "cache", label: "缓存", value: client.cache.enabled ? "启用" : "禁用" });
  if (client.ttl_override) {
    // enabled 缺省与 true 一致视为启用覆盖；未设置的边界显示 “—”，0s 表示该边界不设限。
    const value = client.ttl_override.enabled === false
      ? "禁用"
      : `${ttlBound(client.ttl_override.min)} – ${ttlBound(client.ttl_override.max)}`;
    overrides.push({ key: "ttl", label: "TTL", value });
  }
  const ecs = client.edns_client_subnet;
  if (ecs) {
    const value = ecs.mode === "disabled" ? "禁用" : ecs.mode === "client" ? "客户端地址" : `自定义 ${ecs.custom_ip ?? ""}`.trim();
    overrides.push({ key: "ecs", label: "ECS", value });
  }
  return overrides;
}

/** 接口回显为纳秒串，列表按紧凑单位展示；非法值原样保留，不静默改写后端事实。 */
function ttlBound(value: string | undefined): string {
  if (value === undefined || value === "") return "—";
  try {
    if (durationToNanoseconds(value) === 0n) return "不限";
  } catch {
    return value;
  }
  return normalizeDuration(value) ?? value;
}

export function clientSummary(items: Client[]): ClientSummary {
  const withIps = items.filter((item) => (item.match?.ips?.length ?? 0) > 0);
  return {
    total: items.length,
    idOnly: items.length - withIps.length,
    customStrategy: items.filter((item) => item.strategy).length,
    withOverrides: items.filter((item) => clientOverrides(item).length > 0).length,
    ipEntries: withIps.reduce((sum, item) => sum + (item.match?.ips?.length ?? 0), 0),
    clientsWithIps: withIps.length,
  };
}

/** 收集所有 DoH 路由中带 `{client_id}` 占位符的模板，按配置顺序去重。 */
export function clientRouteTemplates(listeners: Listener[]): string[] {
  const templates = listeners.flatMap((listener) => listener.type === "doh" ? listener.routes.map((route) => route.path) : []);
  return [...new Set(templates.filter((path) => path.includes(CLIENT_ID_PLACEHOLDER)))];
}

/** 把路由模板代入具体 ID，得到终端应配置的 DoH 路径；没有模板时返回空数组，由调用方隐藏该区块。 */
export function clientDohPaths(templates: string[], clientId: string): string[] {
  return templates.map((template) => template.replace(CLIENT_ID_PLACEHOLDER, clientId));
}

/** 过长 ID 只在辅助说明中截断，完整值仍通过 title 或复制获取。 */
export function abbreviateId(id: string, keep = 10): string {
  return id.length <= keep * 2 + 1 ? id : `${id.slice(0, keep)}…${id.slice(-keep)}`;
}

/**
 * 生成 UUID v4 形式的随机 ID。
 * WebUI 可能经由非安全上下文（局域网 HTTP）访问，此时没有 crypto.randomUUID，
 * 因此直接基于 getRandomValues 组装，结果只含 [0-9a-f-]，满足 client_id 字符集。
 */
export function generateClientId(): string {
  const bytes = crypto.getRandomValues(new Uint8Array(16));
  bytes[6] = (bytes[6] & 0x0f) | 0x40;
  bytes[8] = (bytes[8] & 0x3f) | 0x80;
  const hex = Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0")).join("");
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
}

/** 头像取名称首个字符；颜色按名称稳定散列，避免列表刷新时跳色。 */
export function clientAvatar(name: string): { text: string; tone: number } {
  const text = Array.from(name.trim())[0]?.toLocaleUpperCase() ?? "?";
  let hash = 0;
  for (const char of name) hash = (hash * 31 + char.charCodeAt(0)) >>> 0;
  return { text, tone: hash % 5 };
}

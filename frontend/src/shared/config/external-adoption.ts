import type { components } from "@/shared/api/generated-v2";
import { clientEditValue } from "./contract";

type Schemas = components["schemas"];
type ExternalEntry = Schemas["ExternalDiff"]["editable"][number];
type ModuleSource = Schemas["ModuleSource"];

export interface ExternalFieldChange {
  path: string;
  active: string;
  external: string;
}

export interface ExternalAdoptionItem {
  key: string;
  module: Schemas["ConfigModule"];
  label: string;
  action: "create" | "update" | "unsupported";
  change: Schemas["ConfigChange"] | null;
  fields: ExternalFieldChange[];
  note?: string;
}

const moduleLabels: Record<Schemas["ConfigModule"], string> = {
  listener: "Listener",
  upstreams: "上游/组",
  strategy: "策略",
  hosts: "Hosts",
  outbound: "代理",
  rule_set: "规则集",
  clients: "客户端",
  dns: "DNS",
  statistics: "数据保留",
  logs: "日志",
};

/** 把后端白名单差异转换为一次全局 Candidate；不会推断改名或删除。 */
export function externalAdoptionItems(diff: Schemas["ExternalDiff"]): ExternalAdoptionItem[] {
  return diff.editable.map((entry, index) => externalAdoptionItem(entry, index));
}

function externalAdoptionItem(entry: ExternalEntry, index: number): ExternalAdoptionItem {
  const source = entry.external ?? entry.active;
  if (!source) return unsupportedItem(index, "dns", "空差异", [], "差异不包含活动值或外部值");
  const name = sourceName(source);
  const key = `${source.module}:${name ?? "settings"}:${index}`;
  const label = name ? `${moduleLabels[source.module]} / ${name}` : moduleLabels[source.module];
  if (!entry.external) {
    return unsupportedItem(index, source.module, label, diffFields(entry.active?.value, undefined), "当前契约不授权删除仅活动资源");
  }
  if (entry.active && entry.active.module !== entry.external.module) {
    return unsupportedItem(index, source.module, label, [], "差异模块不一致，请重新读取");
  }

  const external = entry.external;
  switch (external.module) {
    case "listener": {
      const active = entry.active?.module === "listener" ? entry.active : null;
      return adoptable(key, external.module, label, active, external, active
        ? { module: "listener", change: { action: "update", original_name: active.value.name, value: external.value } }
        : { module: "listener", change: { action: "create", value: external.value } });
    }
    case "upstreams": {
      const active = entry.active?.module === "upstreams" ? entry.active : null;
      return adoptable(key, external.module, label, active, external, active
        ? { module: "upstreams", change: { action: "update", original_name: active.value.name, value: external.value } }
        : { module: "upstreams", change: { action: "create", value: external.value } });
    }
    case "strategy": {
      const active = entry.active?.module === "strategy" ? entry.active : null;
      return adoptable(key, external.module, label, active, external, active
        ? { module: "strategy", change: { action: "update", original_name: active.value.name, value: external.value } }
        : { module: "strategy", change: { action: "create", value: external.value } });
    }
    case "hosts": {
      const active = entry.active?.module === "hosts" ? entry.active : null;
      return adoptable(key, external.module, label, active, external, active
        ? { module: "hosts", change: { action: "update", original_name: active.value.name, value: external.value } }
        : { module: "hosts", change: { action: "create", value: external.value } });
    }
    case "outbound": {
      const active = entry.active?.module === "outbound" ? entry.active : null;
      return adoptable(key, external.module, label, active, external, active
        ? { module: "outbound", change: { action: "update", original_name: active.value.name, value: external.value } }
        : { module: "outbound", change: { action: "create", value: external.value } });
    }
    case "rule_set": {
      const active = entry.active?.module === "rule_set" ? entry.active : null;
      return adoptable(key, external.module, label, active, external, active
        ? { module: "rule_set", change: { action: "update", original_name: active.value.name, value: external.value } }
        : { module: "rule_set", change: { action: "create", value: external.value } });
    }
    case "clients": {
      const active = entry.active?.module === "clients" ? entry.active : null;
      const item = adoptable(key, external.module, label, active, external, active
        ? { module: "clients", change: { action: "update", original_name: active.value.name, value: clientEditValue(external.value) } }
        : { module: "clients", change: { action: "create", value: external.value } });
      if (active && active.value.client_id !== external.value.client_id) {
        item.note = "client_id 是只读匹配身份，本次采用不会修改";
        item.fields = diffFields(clientEditValue(active.value), clientEditValue(external.value));
      }
      return item;
    }
    case "dns":
      return adoptable(key, external.module, label, entry.active?.module === "dns" ? entry.active : null, external, { module: "dns", change: external.value });
    case "statistics":
      return adoptable(key, external.module, label, entry.active?.module === "statistics" ? entry.active : null, external, { module: "statistics", change: external.value });
    case "logs":
      return adoptable(key, external.module, label, entry.active?.module === "logs" ? entry.active : null, external, { module: "logs", change: external.value });
  }
}

function adoptable(
  key: string,
  module: Schemas["ConfigModule"],
  label: string,
  active: ModuleSource | null,
  external: ModuleSource,
  change: Schemas["ConfigChange"],
): ExternalAdoptionItem {
  return {
    key,
    module,
    label,
    action: active ? "update" : "create",
    change,
    fields: diffFields(active?.value, external.value),
  };
}

function unsupportedItem(
  index: number,
  module: Schemas["ConfigModule"],
  label: string,
  fields: ExternalFieldChange[],
  note: string,
): ExternalAdoptionItem {
  return { key: `${module}:unsupported:${index}`, module, label, action: "unsupported", change: null, fields, note };
}

function sourceName(source: ModuleSource): string | null {
  switch (source.module) {
    case "listener":
    case "upstreams":
    case "strategy":
    case "hosts":
    case "outbound":
    case "rule_set":
    case "clients":
      return source.value.name;
    case "dns":
    case "statistics":
    case "logs":
      return null;
  }
}

function diffFields(active: unknown, external: unknown, path = ""): ExternalFieldChange[] {
  if (Object.is(active, external)) return [];
  if (isPlainObject(active) && isPlainObject(external)) {
    return [...new Set([...Object.keys(active), ...Object.keys(external)])]
      .sort()
      .flatMap((key) => diffFields(active[key], external[key], path ? `${path}.${key}` : key));
  }
  return [{ path: path || "value", active: formatValue(active), external: formatValue(external) }];
}

function isPlainObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function formatValue(value: unknown): string {
  if (value === undefined) return "未配置";
  return JSON.stringify(value);
}

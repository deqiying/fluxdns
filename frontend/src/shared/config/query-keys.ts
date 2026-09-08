import type { QueryKey } from "@tanstack/react-query";
import type { ConfigChange } from "./contract";
import type { ConfigModule } from "./api";

export const configKeys = {
  all: ["config-v2"] as const,
  state: () => ["config-v2", "state"] as const,
  modules: () => ["config-v2", "module"] as const,
  moduleRoot: (module: ConfigModule) => ["config-v2", "module", module] as const,
  module: (module: ConfigModule, activeRevision: string, fileRevision: string) =>
    ["config-v2", "module", module, { activeRevision, fileRevision }] as const,
  references: () => ["config-v2", "references"] as const,
  overview: () => ["config-v2", "overview"] as const,
  operation: (operationId: string) => ["config-v2", "operation", operationId] as const,
  externalDiff: (activeRevision: string, fileRevision: string) =>
    ["config-v2", "external-diff", { activeRevision, fileRevision }] as const,
};

const dependentModules: Record<ConfigModule, readonly ConfigModule[]> = {
  listener: ["listener"],
  upstreams: ["upstreams", "strategy"],
  strategy: ["strategy", "listener", "clients"],
  hosts: ["hosts", "listener", "strategy"],
  outbound: ["outbound", "upstreams", "rule_set"],
  rule_set: ["rule_set", "strategy"],
  clients: ["clients"],
  dns: ["dns"],
  statistics: ["statistics"],
  logs: ["logs"],
};

/** 返回前缀 key，由调用方精确失效目标模块、引用选项和概览。 */
export function invalidationKeysForChanges(changes: readonly ConfigChange[]): QueryKey[] {
  const modules = new Set<ConfigModule>();
  changes.forEach((change) => dependentModules[change.module].forEach((module) => modules.add(module)));
  return [
    configKeys.state(),
    ...[...modules].map((module) => configKeys.moduleRoot(module)),
    configKeys.references(),
    configKeys.overview(),
  ];
}

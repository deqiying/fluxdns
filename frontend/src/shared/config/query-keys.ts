import type { ConfigModule } from "./api";

export const configKeys = {
  all: ["config-v2"] as const,
  state: () => ["config-v2", "state"] as const,
  moduleRoot: (module: ConfigModule) => ["config-v2", "module", module] as const,
  system: ["api", "v2", "config", "system"] as const,
};

/** 系统运行状态沿用独立的查询 key，写入后也需要刷新。 */
export const configInvalidationKeys = [configKeys.all, configKeys.system] as const;

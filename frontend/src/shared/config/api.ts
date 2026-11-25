import { apiV2Request } from "@/shared/api/client";
import type { components } from "@/shared/api/generated-v2";

type Schemas = components["schemas"];

export type ConfigModule = Schemas["ConfigModule"];
export type ConfigRead = Schemas["ConfigRead"];
export type ConfigState = Schemas["ConfigState"];
export type SystemConfigRead = Schemas["SystemConfigRead"];
// OpenAPI 的默认布尔值由后端填充；生成类型将其视为已填充。
type GeneratedSaveRequest = Schemas["SaveRequest"];
export type SaveRequest = Pick<GeneratedSaveRequest, "changes"> & Partial<Omit<GeneratedSaveRequest, "changes">>;

// Runtime prepare 可能等待远程规则下载，给服务端截止时间留出余量。
const PREPARE_TIMEOUT_MS = 40_000;

export function fetchConfigState(signal?: AbortSignal): Promise<ConfigState> {
  return apiV2Request("/config/state", { signal });
}

export function fetchConfigModule(module: ConfigModule, signal?: AbortSignal): Promise<ConfigRead> {
  return apiV2Request(`/config/modules/${encodeURIComponent(module)}`, { signal });
}

export function fetchSystemConfig(signal?: AbortSignal): Promise<SystemConfigRead> {
  return apiV2Request("/config/system", { signal });
}

/** 同步应用变更；persisted 为 false 时已生效但未写盘。 */
export function saveConfig(request: SaveRequest): Promise<Schemas["SaveResult"]> {
  return apiV2Request("/config/save", { method: "POST", body: request, timeoutMs: PREPARE_TIMEOUT_MS });
}

/** 验证并应用磁盘配置，不发送请求体。 */
export function reloadConfig(): Promise<Schemas["ReloadResult"]> {
  return apiV2Request("/config/reload", { method: "POST", timeoutMs: PREPARE_TIMEOUT_MS });
}

/** 用运行配置覆盖磁盘，不更换 Runtime。 */
export function persistConfig(): Promise<Schemas["PersistResult"]> {
  return apiV2Request("/config/persist", { method: "POST" });
}

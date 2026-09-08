import { apiV2Request } from "@/shared/api/client";
import type { components } from "@/shared/api/generated-v2";

type Schemas = components["schemas"];

export type ConfigModule = Schemas["ConfigModule"];
export type ConfigRead = Schemas["ConfigRead"];
export type ConfigState = Schemas["ConfigState"];
export type Candidate = Schemas["Candidate"];
export type ApplyRequest = Schemas["ApplyRequest"];
export type FileSyncRequest = Schemas["FileSyncRequest"];
export type ValidationResult = Schemas["ValidationResult"];
export type OperationResult = Schemas["OperationResult"];
export type ExternalDiff = Schemas["ExternalDiff"];
export type SystemConfigRead = Schemas["SystemConfigRead"];

export function fetchConfigState(signal?: AbortSignal): Promise<ConfigState> {
  return apiV2Request("/config/state", { signal });
}

export function fetchConfigModule(module: ConfigModule, signal?: AbortSignal): Promise<ConfigRead> {
  return apiV2Request(`/config/modules/${encodeURIComponent(module)}`, { signal });
}

export function fetchSystemConfig(signal?: AbortSignal): Promise<SystemConfigRead> {
  return apiV2Request("/config/system", { signal });
}

export function validateCandidate(
  candidate: Candidate,
  options: { module?: ConfigModule; signal?: AbortSignal } = {},
): Promise<ValidationResult> {
  const path = options.module
    ? `/config/modules/${encodeURIComponent(options.module)}/validate`
    : "/config/validate";
  return apiV2Request(path, { method: "POST", body: candidate, signal: options.signal });
}

/** operation_id 由调用方固定；失败后只允许查询操作结果，不能自动重放。 */
export function applyCandidate(
  request: ApplyRequest,
  options: { module?: ConfigModule; signal?: AbortSignal } = {},
): Promise<OperationResult> {
  const path = options.module
    ? `/config/modules/${encodeURIComponent(options.module)}/apply`
    : "/config/apply";
  return apiV2Request(path, { method: "POST", body: request, signal: options.signal });
}

export function fetchConfigOperation(operationId: string, signal?: AbortSignal): Promise<OperationResult> {
  return apiV2Request(`/config/operations/${encodeURIComponent(operationId)}`, { signal });
}

export function fetchExternalDiff(signal?: AbortSignal): Promise<ExternalDiff> {
  return apiV2Request("/config/files/diff", { signal });
}

export function restoreConfigFiles(request: FileSyncRequest, signal?: AbortSignal): Promise<OperationResult> {
  return apiV2Request("/config/files/restore", { method: "POST", body: request, signal });
}

export function retryConfigPersistence(request: FileSyncRequest, signal?: AbortSignal): Promise<OperationResult> {
  return apiV2Request("/config/files/retry", { method: "POST", body: request, signal });
}

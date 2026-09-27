import type { components } from "@/shared/api/generated-v2";
import { fetchConfigModule, fetchSystemConfig } from "@/shared/config/api";

type Schemas = components["schemas"];

export function getSystemConfig(signal?: AbortSignal): Promise<Schemas["SystemConfigRead"]> {
  return fetchSystemConfig(signal);
}

export function getLogsConfig(signal?: AbortSignal): Promise<Schemas["ConfigRead"]> {
  return fetchConfigModule("logs", signal);
}

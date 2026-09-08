import { apiV2Request } from "@/shared/api/client";
import type { components } from "@/shared/api/generated-v2";
import { fetchConfigModule } from "@/shared/config/api";

type Schemas = components["schemas"];

export const retentionStatusKey = ["api", "v2", "retention"] as const;

export function getDnsConfig(signal?: AbortSignal): Promise<Schemas["ConfigRead"]> {
  return fetchConfigModule("dns", signal);
}

export function getStatisticsConfig(signal?: AbortSignal): Promise<Schemas["ConfigRead"]> {
  return fetchConfigModule("statistics", signal);
}

export function getRetentionStatus(signal?: AbortSignal): Promise<Schemas["RetentionStatus"]> {
  return apiV2Request("/retention", { signal });
}

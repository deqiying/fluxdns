import { apiRequest } from "@/shared/api/client";
import type { HealthSnapshot } from "@/shared/api/types";

export const healthKey = ["api", "v1", "health"] as const;

export function getHealth(signal?: AbortSignal): Promise<HealthSnapshot> {
  return apiRequest<HealthSnapshot>("/health", { signal });
}

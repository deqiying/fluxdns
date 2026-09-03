import { apiRequest } from "@/shared/api/client";
import type { SystemInfo } from "@/shared/api/types";

export const systemKey = ["api", "v1", "system"] as const;

export function getSystem(signal?: AbortSignal): Promise<SystemInfo> {
  return apiRequest<SystemInfo>("/system", { signal });
}

import { apiRequest } from "@/shared/api/client";
import type { ResourceSnapshot } from "@/shared/api/types";

export const resourcesKey = ["api", "v1", "resources"] as const;

export function getResources(signal?: AbortSignal): Promise<ResourceSnapshot> {
  return apiRequest<ResourceSnapshot>("/resources", { signal });
}

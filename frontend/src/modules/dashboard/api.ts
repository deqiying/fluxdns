import { apiRequest } from "@/shared/api/client";
import type { Overview } from "@/shared/api/types";

export const overviewKey = ["api", "v1", "overview"] as const;

export function getOverview(signal?: AbortSignal): Promise<Overview> {
  return apiRequest<Overview>("/overview", { signal });
}

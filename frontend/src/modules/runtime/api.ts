import { apiRequest } from "@/shared/api/client";
import type { RuntimeSnapshot } from "@/shared/api/types";

export const runtimeKey = ["api", "v1", "runtime"] as const;

export function getRuntime(signal?: AbortSignal): Promise<RuntimeSnapshot> {
  return apiRequest<RuntimeSnapshot>("/runtime", { signal });
}

import { apiRequest, createSearchParams } from "@/shared/api/client";
import type { QueryPage, QueryParams } from "@/shared/api/types";

export const queryRecordKeys = {
  all: ["api", "v1", "queries"] as const,
  list: (params: QueryParams) => ["api", "v1", "queries", params] as const,
};

export function getQueries(params: QueryParams, signal?: AbortSignal): Promise<QueryPage> {
  const search = createSearchParams({
    page: params.page,
    page_size: params.pageSize,
    transport: params.transport,
    source: params.source,
    rcode: params.rcode,
    outcome: params.outcome,
    sort: params.sort,
    order: params.order,
  });
  return apiRequest<QueryPage>(`/queries?${search}`, { signal });
}

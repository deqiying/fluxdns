import { apiV2Request } from "@/shared/api/client";
import type { components } from "@/shared/api/generated-v2";

type Schemas = components["schemas"];

export type QueryFilter = Schemas["QueryFilter"];
export type QueryRequest = Schemas["QueryRequest"];
export type QueryPage = Schemas["QueryPage"];
export type QueryRecord = Schemas["QueryRecord"];
export type QueryDetail = Schemas["QueryDetail"];

export const queryRecordKeys = {
  all: ["api", "v2", "queries"] as const,
  list: (request: QueryRequest) => ["api", "v2", "queries", request] as const,
  detail: (recordId: string) => ["api", "v2", "queries", "detail", recordId] as const,
};

export function getQueries(request: QueryRequest, signal?: AbortSignal): Promise<QueryPage> {
  return apiV2Request<QueryPage>("/queries/search", { method: "POST", body: request, signal });
}

export function getQueryDetail(recordId: string, signal?: AbortSignal): Promise<QueryDetail> {
  return apiV2Request<QueryDetail>(`/queries/${encodeURIComponent(recordId)}`, { signal });
}

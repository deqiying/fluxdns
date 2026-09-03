import { apiRequest, createSearchParams } from "@/shared/api/client";
import type { StatisticsPage, StatisticsParams } from "@/shared/api/types";

export const statisticsKeys = {
  all: ["api", "v1", "statistics"] as const,
  list: (params: StatisticsParams) => ["api", "v1", "statistics", params] as const,
};

export function getStatistics(params: StatisticsParams, signal?: AbortSignal): Promise<StatisticsPage> {
  const search = createSearchParams({
    date_from: params.dateFrom,
    date_to: params.dateTo,
    dimension: params.dimension,
    page: params.page,
    page_size: params.pageSize,
  });
  return apiRequest<StatisticsPage>(`/statistics?${search}`, { signal });
}

import { keepPreviousData, useQuery } from "@tanstack/react-query";
import type { StatisticsParams } from "@/shared/api/types";
import { getStatistics, statisticsKeys } from "./api";

export function useStatistics(params: StatisticsParams) {
  return useQuery({
    queryKey: statisticsKeys.list(params),
    queryFn: ({ signal }) => getStatistics(params, signal),
    placeholderData: keepPreviousData,
  });
}

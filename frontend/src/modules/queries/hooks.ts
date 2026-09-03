import { keepPreviousData, useQuery } from "@tanstack/react-query";
import type { QueryParams } from "@/shared/api/types";
import { getQueries, queryRecordKeys } from "./api";

export function useQueries(params: QueryParams) {
  return useQuery({
    queryKey: queryRecordKeys.list(params),
    queryFn: ({ signal }) => getQueries(params, signal),
    placeholderData: keepPreviousData,
  });
}

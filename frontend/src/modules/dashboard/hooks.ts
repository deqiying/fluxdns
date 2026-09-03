import { useQuery } from "@tanstack/react-query";
import { getSummaryPollInterval } from "@/app/query-client";
import { getOverview, overviewKey } from "./api";

export function useOverview() {
  return useQuery({
    queryKey: overviewKey,
    queryFn: ({ signal }) => getOverview(signal),
    refetchInterval: () => getSummaryPollInterval(),
    refetchIntervalInBackground: false,
  });
}

import { useQuery } from "@tanstack/react-query";
import { getSummaryPollInterval } from "@/app/query-client";
import { getHealth, healthKey } from "./api";

export function useHealth() {
  return useQuery({
    queryKey: healthKey,
    queryFn: ({ signal }) => getHealth(signal),
    refetchInterval: () => getSummaryPollInterval(),
    refetchIntervalInBackground: false,
  });
}

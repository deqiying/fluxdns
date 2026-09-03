import { useQuery } from "@tanstack/react-query";
import { getSummaryPollInterval } from "@/app/query-client";
import { getRuntime, runtimeKey } from "./api";

export function useRuntime() {
  return useQuery({
    queryKey: runtimeKey,
    queryFn: ({ signal }) => getRuntime(signal),
    refetchInterval: () => getSummaryPollInterval(),
    refetchIntervalInBackground: false,
  });
}

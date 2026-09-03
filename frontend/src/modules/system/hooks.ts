import { useQuery } from "@tanstack/react-query";
import { getSystem, systemKey } from "./api";

export function useSystem() {
  return useQuery({
    queryKey: systemKey,
    queryFn: ({ signal }) => getSystem(signal),
    staleTime: 5 * 60_000,
  });
}

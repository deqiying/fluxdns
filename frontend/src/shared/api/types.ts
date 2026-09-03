import type { components } from "./generated";

type Schemas = components["schemas"];

export type ErrorEnvelope = Schemas["ErrorEnvelope"];
export type LoginRequest = Schemas["LoginRequest"];
export type Session = Schemas["Session"];
export type HealthStatus = Schemas["HealthStatus"];
export type Overview = Schemas["Overview"];
export type OverviewCard = Schemas["OverviewCard"];
export type Transport = Schemas["Transport"];
export type RuntimeSnapshot = Schemas["RuntimeSnapshot"];
export type BindEntry = Schemas["BindEntry"];
export type HealthSnapshot = Schemas["HealthSnapshot"];
export type ComponentHealth = Schemas["ComponentHealth"];
export type StatisticDimension = Schemas["StatisticDimension"];
export type StatisticsPage = Schemas["StatisticsPage"];
export type StatisticItem = Schemas["StatisticItem"];
export type QuerySource = Schemas["QuerySource"];
export type Rcode = Schemas["Rcode"];
export type QueryOutcome = Schemas["QueryOutcome"];
export type CacheOutcome = Schemas["CacheOutcome"];
export type QueryPage = Schemas["QueryPage"];
export type QueryRecord = Schemas["QueryRecord"];
export type ResourceSnapshot = Schemas["ResourceSnapshot"];
export type ResourceSummary = Schemas["ResourceSummary"];
export type SystemInfo = Schemas["SystemInfo"];
export type Capability = Schemas["Capability"];

export interface StatisticsParams {
  dateFrom: string;
  dateTo: string;
  dimension: StatisticDimension;
  page: number;
  pageSize: number;
}

export interface QueryParams {
  page: number;
  pageSize: number;
  transport?: Transport;
  source?: QuerySource;
  rcode?: Rcode;
  outcome?: QueryOutcome;
  sort: "occurred_at" | "duration_ms";
  order: "asc" | "desc";
}

export const statisticDimensions = [
  "total",
  "transport",
  "source",
  "rcode",
  "outcome",
  "cache",
] as const satisfies readonly StatisticDimension[];

export const transports = ["udp", "tcp", "doh"] as const satisfies readonly Transport[];
export const querySources = ["cache", "hosts", "rule", "upstream", "synthetic"] as const satisfies readonly QuerySource[];
export const rcodes = ["NOERROR", "FORMERR", "SERVFAIL", "NXDOMAIN", "NOTIMP", "REFUSED", "OTHER"] as const satisfies readonly Rcode[];
export const queryOutcomes = ["answered", "negative", "timeout", "rejected", "failed"] as const satisfies readonly QueryOutcome[];

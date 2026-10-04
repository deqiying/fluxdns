import type { components } from "@/shared/api/generated-v2";
import { normalizeDuration } from "@/shared/config/form-values";
import type { FormInstance } from "antd";
import type { ApiFieldError } from "@/shared/api/errors";

type Schemas = components["schemas"];
export type Strategy = Schemas["Strategy"];
type Ecs = Schemas["Ecs"];

export interface RuleFormValue {
  source_type: "hosts" | "rule_set";
  source: string;
  upstream?: string;
  ecs_mode: "inherit" | Ecs["mode"];
  ecs_custom_ip?: string;
}

export interface StrategyFormValues {
  name: string;
  default_upstream: string;
  rules: RuleFormValue[];
  cache_mode: "inherit" | "enabled" | "disabled";
  cache_optimistic_mode: "inherit" | "enabled" | "disabled";
  cache_answer_ttl?: string;
  cache_max_age?: string;
  /** 空应答（NODATA/NXDOMAIN）乐观窗口；`0s` 表示空应答过期即回源。 */
  cache_negative_max_age?: string;
  ttl_mode: "inherit" | "enabled" | "disabled";
  ttl_min?: string;
  ttl_max?: string;
  ecs_mode: "inherit" | Ecs["mode"];
  ecs_custom_ip?: string;
}

function ecsToForm(ecs: Ecs | undefined) {
  return {
    ecs_mode: ecs?.mode ?? "inherit",
    ecs_custom_ip: ecs?.custom_ip,
  } as const;
}

function ecsFromForm(mode: StrategyFormValues["ecs_mode"], customIp?: string): Ecs | undefined {
  if (mode === "inherit") return undefined;
  return {
    mode,
    ...(mode === "custom" && customIp ? { custom_ip: customIp } : {}),
  };
}

/** 将 API DTO 展开为可编辑的表单状态，同时保留覆盖对象的缺失字段语义。 */
export function strategyToForm(strategy?: Strategy): StrategyFormValues {
  if (!strategy) {
    return {
      name: "",
      default_upstream: "",
      rules: [{ source_type: "hosts", source: "", ecs_mode: "inherit" }],
      cache_mode: "inherit",
      cache_optimistic_mode: "inherit",
      ttl_mode: "inherit",
      ecs_mode: "inherit",
    };
  }
  return {
    name: strategy.name,
    default_upstream: strategy.default_upstream,
    rules: strategy.rules.map((rule) => ({
      source_type: rule.hosts ? "hosts" : "rule_set",
      source: rule.hosts ?? rule.rule_set ?? "",
      ...(rule.hosts ? {} : { upstream: rule.upstream }),
      ...ecsToForm(rule.edns_client_subnet),
    })),
    cache_mode: strategy.cache ? (strategy.cache.enabled ? "enabled" : "disabled") : "inherit",
    cache_optimistic_mode: strategy.cache?.optimistic ? (strategy.cache.optimistic.enabled ? "enabled" : "disabled") : "inherit",
    cache_answer_ttl: normalizeDuration(strategy.cache?.optimistic?.answer_ttl),
    cache_max_age: normalizeDuration(strategy.cache?.optimistic?.max_age),
    cache_negative_max_age: normalizeDuration(strategy.cache?.optimistic?.negative_max_age),
    ttl_mode: strategy.ttl_override ? (strategy.ttl_override.enabled === false ? "disabled" : "enabled") : "inherit",
    ttl_min: normalizeDuration(strategy.ttl_override?.min),
    ttl_max: normalizeDuration(strategy.ttl_override?.max),
    ...ecsToForm(strategy.edns_client_subnet),
  };
}

function sameOptionalDuration(value: string | undefined, current: string | undefined) {
  return (value ?? undefined) === (current ?? undefined);
}

/** 根据原 DTO 合并用户填写内容，避免名称等无关修改改写继承字段或丢失零值。 */
export function strategyFromForm(values: StrategyFormValues, original?: Strategy): Strategy {
  const source = original ?? undefined;
  const sourceForm = strategyToForm(source);
  const cacheUnchanged = values.cache_mode === sourceForm.cache_mode
    && values.cache_optimistic_mode === sourceForm.cache_optimistic_mode
    && sameOptionalDuration(values.cache_answer_ttl, sourceForm.cache_answer_ttl)
    && sameOptionalDuration(values.cache_max_age, sourceForm.cache_max_age)
    && sameOptionalDuration(values.cache_negative_max_age, sourceForm.cache_negative_max_age);
  const ttlUnchanged = values.ttl_mode === sourceForm.ttl_mode
    && sameOptionalDuration(values.ttl_min, sourceForm.ttl_min)
    && sameOptionalDuration(values.ttl_max, sourceForm.ttl_max);
  const negativeMaxAge = values.cache_negative_max_age ?? source?.cache?.optimistic?.negative_max_age;
  const cache = values.cache_mode === "inherit" ? undefined
    : cacheUnchanged ? source?.cache
    : {
      enabled: values.cache_mode === "enabled",
      ...(values.cache_optimistic_mode === "inherit" ? {} : {
        optimistic: {
          enabled: values.cache_optimistic_mode === "enabled",
          answer_ttl: values.cache_answer_ttl ?? source?.cache?.optimistic?.answer_ttl ?? "0s",
          max_age: values.cache_max_age ?? source?.cache?.optimistic?.max_age ?? "0s",
          // 字段在契约中可选：未填写且原值缺失时省略，由后端按默认值解析。
          ...(negativeMaxAge === undefined ? {} : { negative_max_age: negativeMaxAge }),
        },
      }),
    };
  const ttlOverride = values.ttl_mode === "inherit" ? undefined
    : ttlUnchanged ? source?.ttl_override
    : {
      ...(source?.ttl_override?.enabled === undefined ? {} : { enabled: source.ttl_override.enabled }),
      ...(values.ttl_mode === "disabled" ? { enabled: false } : {}),
      ...(values.ttl_mode === "enabled" && sourceForm.ttl_mode !== "enabled" ? { enabled: true } : {}),
      ...(values.ttl_min ? { min: values.ttl_min } : {}),
      ...(values.ttl_max ? { max: values.ttl_max } : {}),
    };
  return {
    name: values.name,
    default_upstream: values.default_upstream,
    rules: values.rules.map((rule) => ({
      ...(rule.source_type === "hosts" ? { hosts: rule.source } : { rule_set: rule.source, upstream: rule.upstream ?? "" }),
      ...(ecsFromForm(rule.ecs_mode, rule.ecs_custom_ip) ? { edns_client_subnet: ecsFromForm(rule.ecs_mode, rule.ecs_custom_ip) } : {}),
    })),
    ...(cache ? { cache } : {}),
    ...(ttlOverride ? { ttl_override: ttlOverride } : {}),
    ...(ecsFromForm(values.ecs_mode, values.ecs_custom_ip) ? { edns_client_subnet: ecsFromForm(values.ecs_mode, values.ecs_custom_ip) } : {}),
  };
}

export function ecsSummary(ecs: Ecs | undefined) {
  if (!ecs) return "ECS：继承";
  if (ecs.mode === "custom") return `ECS：自定义 ${ecs.custom_ip ?? ""}`;
  return `ECS：${ecs.mode === "client" ? "客户端地址" : "禁用"}`;
}

/** 只将已知 DTO 路径映射到表单字段；未知路径仍由弹窗统一展示。 */
export function strategyFieldErrors(errors: readonly ApiFieldError[]): Parameters<FormInstance<StrategyFormValues>["setFields"]>[0] {
  const fields: Parameters<FormInstance<StrategyFormValues>["setFields"]>[0] = [];
  for (const error of errors) {
    const path = error.path.replace(/\[(\d+)\]/g, ".$1").replaceAll("/", ".");
    const rule = path.match(/(?:^|\.)rules\.(\d+)\.(.+)$/);
    if (rule) {
      const mapping: Record<string, keyof RuleFormValue> = { hosts: "source", rule_set: "source", upstream: "upstream", "edns_client_subnet.mode": "ecs_mode", "edns_client_subnet.custom_ip": "ecs_custom_ip", edns_client_subnet: "ecs_custom_ip" };
      const key = mapping[rule[2]];
      if (key) fields.push({ name: ["rules", Number(rule[1]), key], errors: [error.code] });
    } else {
      const mapping: Record<string, Exclude<keyof StrategyFormValues, "rules">> = { name: "name", default_upstream: "default_upstream", "ttl_override.min": "ttl_min", "ttl_override.max": "ttl_max", "edns_client_subnet.custom_ip": "ecs_custom_ip", "edns_client_subnet.mode": "ecs_mode", "cache.optimistic.answer_ttl": "cache_answer_ttl", "cache.optimistic.max_age": "cache_max_age", "cache.optimistic.negative_max_age": "cache_negative_max_age" };
      const match = Object.keys(mapping).find((suffix) => path === suffix || path.endsWith(`.${suffix}`));
      if (match) fields.push({ name: [mapping[match]], errors: [error.code] });
    }
  }
  return fields;
}

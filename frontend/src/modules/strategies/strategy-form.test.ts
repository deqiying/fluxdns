import { describe, expect, it } from "vitest";
import { ecsSummary, strategyFromForm, strategyToForm, type StrategyFormValues } from "./strategy-form";

const strategy = {
  name: "default",
  default_upstream: "primary",
  rules: [
    { hosts: "local", edns_client_subnet: { mode: "custom" as const, custom_ip: "2001:db8::/48" } },
    { rule_set: "domains", upstream: "secure", edns_client_subnet: { mode: "client" as const } },
  ],
  cache: { enabled: true, optimistic: { enabled: false, answer_ttl: "0s", max_age: "1h" } },
  ttl_override: { min: "0s", max: "1h" },
  edns_client_subnet: { mode: "disabled" as const },
};

describe("strategy form conversion", () => {
  it("保留规则 ECS、cache optimistic 与 TTL 的缺失 enabled 和零值", () => {
    const form = strategyToForm(strategy);
    expect(form.rules[0]).toMatchObject({ source_type: "hosts", ecs_mode: "custom", ecs_custom_ip: "2001:db8::/48" });
    expect(form.rules[1]).toMatchObject({ source_type: "rule_set", upstream: "secure", ecs_mode: "client" });
    expect(strategyFromForm({ ...form, name: "renamed" }, strategy)).toMatchObject({
      name: "renamed",
      cache: strategy.cache,
      ttl_override: { min: "0s", max: "1h" },
      rules: strategy.rules,
    });
  });

  it("继承不发送 ECS，无自定义地址的模式不保留 custom_ip，分支字段随类型清理", () => {
    const values: StrategyFormValues = {
      ...strategyToForm(strategy),
      rules: [
        { source_type: "hosts", source: "local", upstream: "stale", ecs_mode: "disabled", ecs_custom_ip: "stale" },
        { source_type: "rule_set", source: "domains", upstream: "secure", ecs_mode: "custom", ecs_custom_ip: "192.0.2.0/24" },
      ],
      ecs_mode: "inherit",
      ecs_custom_ip: "stale",
    };
    expect(strategyFromForm(values, strategy)).toMatchObject({
      rules: [
        { hosts: "local", edns_client_subnet: { mode: "disabled" } },
        { rule_set: "domains", upstream: "secure", edns_client_subnet: { mode: "custom", custom_ip: "192.0.2.0/24" } },
      ],
    });
    expect(strategyFromForm(values, strategy).edns_client_subnet).toBeUndefined();
    expect(ecsSummary({ mode: "client" })).toBe("ECS：客户端地址");
  });

  it("策略和规则的请求 ECS 优先模式无损往返，切换模式后清除地址", () => {
    const ecs = { mode: "client_or_custom" as const, custom_ip: "2001:db8::/48" };
    const original = { ...strategy, edns_client_subnet: ecs, rules: [{ rule_set: "domains", upstream: "secure", edns_client_subnet: ecs }] };
    const form = strategyToForm(original);
    expect(form).toMatchObject({ ecs_mode: "client_or_custom", ecs_custom_ip: ecs.custom_ip, rules: [{ ecs_mode: "client_or_custom", ecs_custom_ip: ecs.custom_ip }] });
    expect(strategyFromForm(form, original)).toEqual(original);
    expect(ecsSummary(ecs)).toBe("ECS：请求 ECS 优先，自定义兜底 2001:db8::/48");
    for (const mode of ["inherit", "client", "disabled"] as const) {
      const saved = strategyFromForm({ ...form, ecs_mode: mode, rules: form.rules.map((rule) => ({ ...rule, ecs_mode: mode })) }, original);
      expect(saved.edns_client_subnet).toEqual(mode === "inherit" ? undefined : { mode });
      expect(saved.rules[0].edns_client_subnet).toEqual(mode === "inherit" ? undefined : { mode });
    }
    const custom = strategyFromForm({ ...form, ecs_mode: "custom" }, original);
    expect(custom.edns_client_subnet).toEqual({ mode: "custom", custom_ip: ecs.custom_ip });
  });

  it("清空 TTL 边界移除旧值，仍保留 enabled 缺失", () => {
    const form = strategyToForm(strategy);
    const saved = strategyFromForm({ ...form, ttl_min: undefined }, strategy);
    expect(saved.ttl_override).toEqual({ max: "1h" });
  });

  it("空应答乐观窗口零值往返保真，缺失时不凭空写入", () => {
    const withNegative = { ...strategy, cache: { enabled: true, optimistic: { enabled: true, answer_ttl: "30s", max_age: "1h", negative_max_age: "0s" } } };
    const form = strategyToForm(withNegative);
    expect(form.cache_negative_max_age).toBe("0s");
    expect(strategyFromForm({ ...form, cache_max_age: "2h" }, withNegative).cache?.optimistic).toEqual({ enabled: true, answer_ttl: "30s", max_age: "2h", negative_max_age: "0s" });
    expect(strategyFromForm({ ...form, cache_negative_max_age: "5m" }, withNegative).cache?.optimistic?.negative_max_age).toBe("5m");

    const legacy = strategyToForm(strategy);
    expect(legacy.cache_negative_max_age).toBeUndefined();
    expect(strategyFromForm({ ...legacy, cache_max_age: "2h" }, strategy).cache?.optimistic).toEqual({ enabled: false, answer_ttl: "0s", max_age: "2h" });
  });
});

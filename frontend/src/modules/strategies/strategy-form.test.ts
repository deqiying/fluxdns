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

  it("继承不发送 ECS，非 custom 不保留 custom_ip，分支字段随类型清理", () => {
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

  it("清空 TTL 边界移除旧值，仍保留 enabled 缺失", () => {
    const form = strategyToForm(strategy);
    const saved = strategyFromForm({ ...form, ttl_min: undefined }, strategy);
    expect(saved.ttl_override).toEqual({ max: "1h" });
  });
});

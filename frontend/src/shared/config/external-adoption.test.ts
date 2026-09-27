import { describe, expect, it } from "vitest";
import type { components } from "@/shared/api/generated-v2";
import { externalAdoptionItems } from "./external-adoption";

type Schemas = components["schemas"];

describe("外部差异组合采用", () => {
  it("十个模块都转换为类型化变更并保留明确旧名", () => {
    const editable: Schemas["ExternalDiff"]["editable"] = [
      pair({ module: "listener", value: { name: "dns", type: "udp", addresses: ["127.0.0.1"], port: 53, strategy: "main" } }, { module: "listener", value: { name: "dns", type: "udp", addresses: ["127.0.0.1"], port: 5353, strategy: "main" } }),
      pair({ module: "upstreams", value: { name: "local", type: "hosts", format: "hosts", hosts: "127.0.0.1 localhost" } }, { module: "upstreams", value: { name: "local", type: "hosts", format: "hosts", hosts: "127.0.0.2 localhost" } }),
      pair({ module: "strategy", value: { name: "main", default_upstream: "local", rules: [] } }, { module: "strategy", value: { name: "main", default_upstream: "local", rules: [], cache: { enabled: false } } }),
      pair(null, { module: "hosts", value: { name: "lan", type: "const", format: "hosts", hosts: "127.0.0.1 lan" } }),
      pair(null, { module: "outbound", value: { name: "proxy", type: "socks5", proxy_url: { env: "PROXY_URL" } } }),
      pair(null, { module: "rule_set", value: { name: "domains", type: "const", format: "clash", rule: "+.example.test" } }),
      pair({ module: "clients", value: { name: "desktop", client_id: "desktop-1", match: { ips: [] } } }, { module: "clients", value: { name: "desktop", client_id: "changed-id", match: { ips: ["192.0.2.1"] } } }),
      pair({ module: "dns", value: {} }, { module: "dns", value: { cache: { enabled: false, memory: { max_size_bytes: 1024 }, failure_ttl: "5s", optimistic: { enabled: false, answer_ttl: "10s", max_age: "1h" }, persistence: { enabled: false, path: "cache", snapshot_interval: "5m" } } } }),
      pair({ module: "statistics", value: {} }, { module: "statistics", value: { retention: { days: 8, grace_days: 2, reference_size_bytes: 1024 } } }),
      pair({ module: "logs", value: { enable: true, level: "info", path: "old.log" } }, { module: "logs", value: { enable: true, level: "debug", path: "new.log" } }),
    ];
    const items = externalAdoptionItems(diff(editable));
    expect(items.map((item) => item.module)).toEqual(["listener", "upstreams", "strategy", "hosts", "outbound", "rule_set", "clients", "dns", "statistics", "logs"]);
    expect(items.every((item) => item.change !== null)).toBe(true);
    expect(items[0]?.change).toMatchObject({ module: "listener", change: { action: "update", original_name: "dns" } });
    expect(items[3]?.change).toMatchObject({ module: "hosts", change: { action: "create" } });
    expect(items[6]?.change).not.toHaveProperty("change.value.client_id");
    expect(items[6]?.note).toMatch(/client_id/);
    expect(items[9]?.fields).toContainEqual({ path: "level", active: "\"info\"", external: "\"debug\"" });
  });

  it("仅活动资源禁用，且不推断删除或改名", () => {
    const items = externalAdoptionItems(diff([
      pair({ module: "hosts", value: { name: "removed", type: "const", format: "hosts", hosts: "" } }, null),
    ]));
    expect(items[0]).toMatchObject({ module: "hosts", action: "unsupported", change: null });
    expect(items[0]?.note).toMatch(/不授权删除/);
  });
});

function pair(active: Schemas["ModuleSource"] | null, external: Schemas["ModuleSource"] | null) {
  return { active, external };
}

function diff(editable: Schemas["ExternalDiff"]["editable"]): Schemas["ExternalDiff"] {
  return {
    expected: { active_revision: "active-1", observed_file_revision: "files-2" },
    editable,
    protected_changes: [],
    parse_error: null,
  };
}

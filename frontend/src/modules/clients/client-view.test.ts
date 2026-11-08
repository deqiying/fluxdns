import { describe, expect, it, vi } from "vitest";
import type { components } from "@/shared/api/generated-v2";
import * as uuid from "@/shared/uuid";
import { abbreviateId, clientAvatar, clientDohPaths, clientOverrides, clientRouteTemplates, clientSummary, generateClientId, type Client } from "./client-view";

const clients: Client[] = [
  { name: "desktop", client_id: "Desktop-01", match: { ips: ["192.0.2.10", "2001:db8::/64"] }, strategy: "default", cache: { enabled: false }, edns_client_subnet: { mode: "client" } },
  { name: "phone", client_id: "phone", ttl_override: { min: "60000000000ns", max: "3600000000000ns" } },
  { name: "nas", client_id: "nas", match: { ips: ["192.0.2.20/32"] } },
];

describe("client view helpers", () => {
  it("只列出非继承的覆盖项并保留 TTL 未设边界", () => {
    expect(clientOverrides(clients[0])).toEqual([
      { key: "cache", label: "缓存", value: "禁用" },
      { key: "ecs", label: "ECS", value: "客户端地址" },
    ]);
    expect(clientOverrides(clients[1])).toEqual([{ key: "ttl", label: "TTL", value: "1m – 1h" }]);
    expect(clientOverrides({ name: "x", client_id: "x", ttl_override: { enabled: false } })).toEqual([{ key: "ttl", label: "TTL", value: "禁用" }]);
    expect(clientOverrides({ name: "x", client_id: "x", ttl_override: { min: "0s", max: "1h" } })[0].value).toBe("不限 – 1h");
    expect(clientOverrides({ name: "x", client_id: "x", ttl_override: { max: "1h" } })[0].value).toBe("— – 1h");
    expect(clientOverrides(clients[2])).toEqual([]);
  });

  it("汇总概览卡片数据", () => {
    expect(clientSummary(clients)).toEqual({ total: 3, idOnly: 1, customStrategy: 1, withOverrides: 2, ipEntries: 3, clientsWithIps: 2 });
    expect(clientSummary([])).toMatchObject({ total: 0, ipEntries: 0 });
  });

  it("从 DoH 裸路由和显式模板生成 ID 地址并去重", () => {
    const listeners = [
      { name: "udp", type: "udp", addresses: ["0.0.0.0"], port: 53, strategy: "default" },
      { name: "doh-a", type: "doh", routes: [{ path: "/dns/inner/{client_id}", strategy: "a" }, { path: "/dns-query", strategy: "a" }], endpoints: [] },
      { name: "doh-b", type: "doh", routes: [{ path: "/dns/inner/{client_id}", strategy: "a" }, { path: "/dns/inner", strategy: "a" }, { path: "/dns/outside/{client_id}", strategy: "b" }], endpoints: [] },
    ] as unknown as components["schemas"]["Listener"][];
    const templates = clientRouteTemplates(listeners);
    expect(templates).toEqual(["/dns/inner/{client_id}", "/dns-query/{client_id}", "/dns/outside/{client_id}"]);
    expect(clientDohPaths(templates, "singbox-home")).toEqual(["/dns/inner/singbox-home", "/dns-query/singbox-home", "/dns/outside/singbox-home"]);
    expect(clientDohPaths([], "x")).toEqual([]);
  });

  it("裸根路径及尾随斜杠复用分隔符，中间占位符保持不变", () => {
    const listeners = [{ name: "doh", type: "doh", routes: [
      { path: "/", strategy: "default" },
      { path: "/dns/", strategy: "default" },
      { path: "/client/{client_id}/query", strategy: "default" },
    ], endpoints: [] }] as unknown as components["schemas"]["Listener"][];
    expect(clientDohPaths(clientRouteTemplates(listeners), "client-a")).toEqual([
      "/client-a", "/dns/client-a", "/client/client-a/query",
    ]);
    expect(listeners[0]).toMatchObject({ routes: [{ path: "/" }, { path: "/dns/" }, { path: "/client/{client_id}/query" }] });
  });

  it("生成满足 client_id 字符集的 UUID v4", () => {
    const id = generateClientId();
    expect(id).toMatch(/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/);
    expect(generateClientId()).not.toBe(id);
  });

  it("客户端 ID 复用共享 UUID v4 生成器，不改变结果", () => {
    const id = "ffffffff-ffff-4fff-bfff-ffffffffffff";
    const generate = vi.spyOn(uuid, "generateUuidV4").mockReturnValue(id);
    expect(generateClientId()).toBe(id);
    expect(generate).toHaveBeenCalledExactlyOnceWith();
  });

  it("截断长 ID 与稳定头像", () => {
    expect(abbreviateId("bae9239c-20c7-48ae-88c5-62ab1a9c1894")).toBe("bae9239c-2…ab1a9c1894");
    expect(abbreviateId("short-id")).toBe("short-id");
    expect(clientAvatar("nas-home")).toEqual(clientAvatar("nas-home"));
    expect(clientAvatar("nas-home").text).toBe("N");
    expect(clientAvatar(" ").text).toBe("?");
  });
});

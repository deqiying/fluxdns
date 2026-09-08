import { describe, expect, it } from "vitest";
import { configKeys, invalidationKeysForChanges } from "./query-keys";

describe("配置 query key", () => {
  it("模块 key 固定活动和文件 revision", () => {
    expect(configKeys.module("clients", "active-1", "file-2")).toEqual([
      "config-v2",
      "module",
      "clients",
      { activeRevision: "active-1", fileRevision: "file-2" },
    ]);
  });

  it("只失效目标模块、引用依赖和概览", () => {
    const keys = invalidationKeysForChanges([
      { module: "upstreams", change: { action: "create", value: { name: "edge", type: "HostsUpstream", format: "json", hosts: "{}" } } },
    ]);
    expect(keys).toContainEqual(configKeys.moduleRoot("upstreams"));
    expect(keys).toContainEqual(configKeys.moduleRoot("strategy"));
    expect(keys).toContainEqual(configKeys.references());
    expect(keys).toContainEqual(configKeys.overview());
    expect(keys).not.toContainEqual(configKeys.moduleRoot("logs"));
  });
});

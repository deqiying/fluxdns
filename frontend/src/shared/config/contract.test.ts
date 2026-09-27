import { describe, expect, it } from "vitest";
import { managementRoutes, upstreamTabs } from "@/app/route-contract";
import { clientEditValue, decimalToSafeInteger, operationDisposition } from "./contract";
import type { components } from "@/shared/api/generated-v2";

type Schemas = components["schemas"];

describe("P0 v2 路由和表单契约", () => {
  it("十二模块和上游页内 tab，不提前增加第十三个页面", () => {
    expect(managementRoutes).toHaveLength(12);
    expect(new Set(managementRoutes.map((route) => route.path)).size).toBe(12);
    expect(managementRoutes.map((route) => route.path)).toContain("/dashboard");
    expect(managementRoutes.map((route) => route.path)).toContain("/queries");
    expect(managementRoutes.flatMap((route) => [...route.modules]).sort()).toEqual(
      ["clients", "dns", "hosts", "listener", "logs", "outbound", "rule_set", "statistics", "strategy", "upstreams"].sort(),
    );
    expect(upstreamTabs).toEqual(["upstreams", "groups"]);
  });

  it("客户端编辑移除 ID，保留缺失继承与显式禁用", () => {
    const client = { name: "desktop", client_id: "Desktop-01", cache: {enabled: false} } satisfies Schemas["Client"];
    const value = clientEditValue(client);
    expect(JSON.parse(JSON.stringify(value))).toEqual({name: "desktop", cache: {enabled: false}});
    expect(value).not.toHaveProperty("client_id");
    const change = {module: "clients", change: {action: "update", original_name: "desktop", value: {...value, name: "renamed"}}} satisfies Schemas["ConfigChange"];
    expect(change.change.original_name).toBe("desktop");
    // @ts-expect-error 普通客户端编辑不接受 client_id。
    const invalid: Schemas["ClientEdit"] = {name: "desktop", client_id: "altered"};
    expect(invalid).toBeDefined();
  });

  it("操作状态不把未同步和结果未知误报为成功", () => {
    expect(operationDisposition({state: "applied_unpersisted", active_revision: "a-2", persisted_revision: "a-1", error: "PERSISTENCE_FAILED"})).toBe("retry_persistence");
    expect(operationDisposition({state: "unknown"})).toBe("read_active_state");
    expect(operationDisposition({state: "applied_synced", active_revision: "a-2", persisted_revision: "a-2"})).toBe("synchronized");
    expect(operationDisposition({state: "compensation_failed", active_revision: null, error: "COMPENSATION_FAILED"})).toBe("blocked");
  });

  it("大整数不会经 Number 静默丢失精度", () => {
    expect(decimalToSafeInteger("1073741824")).toBe(1073741824);
    expect(decimalToSafeInteger("9007199254740991")).toBe(Number.MAX_SAFE_INTEGER);
    for (const value of ["9007199254740993", "18446744073709551615", "-1", "1.5", "01", "1e3"]) {
      expect(() => decimalToSafeInteger(value)).toThrow();
    }
  });
});

import { expect, it } from "vitest";
import { systemConfigKey } from "@/modules/system/api";
import { configInvalidationKeys, configKeys } from "./query-keys";

it("模块 key 不固定版本，配置写入覆盖整个前缀并刷新系统运行态", () => {
  expect(configKeys.moduleRoot("clients")).toEqual(["config-v2", "module", "clients"]);
  expect(configInvalidationKeys).toEqual([["config-v2"], ["api", "v2", "config", "system"]]);
  expect(configInvalidationKeys).toContainEqual(systemConfigKey);
});

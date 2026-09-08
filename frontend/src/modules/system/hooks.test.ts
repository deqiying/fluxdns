import { describe, expect, it } from "vitest";
import { calculateDisplayedUptime } from "./hooks";

describe("system runtime uptime", () => {
  it("只从有效响应基准递增运行时长", () => {
    expect(calculateDisplayedUptime(100, 1_000, 3_999)).toBe(102);
    expect(calculateDisplayedUptime(100, 3_000, 2_000)).toBe(100);
    expect(calculateDisplayedUptime(-1, 1_000, 2_000)).toBeUndefined();
    expect(calculateDisplayedUptime(100, 0, 2_000)).toBeUndefined();
  });
});

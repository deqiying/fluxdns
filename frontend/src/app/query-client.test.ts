import { describe, expect, it } from "vitest";
import { getSummaryPollInterval, SUMMARY_POLL_INTERVAL_MS } from "./query-client";

describe("summary polling", () => {
  it("页面隐藏时暂停轮询", () => {
    expect(getSummaryPollInterval(false)).toBe(false);
    expect(getSummaryPollInterval(true)).toBe(SUMMARY_POLL_INTERVAL_MS);
  });
});

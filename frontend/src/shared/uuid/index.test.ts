import { afterEach, describe, expect, it, vi } from "vitest";
import { generateUuidV4 } from "./index";

afterEach(() => vi.unstubAllGlobals());

describe("共享 UUID v4", () => {
  it.each([
    { bytes: Array<number>(16).fill(0), expected: "00000000-0000-4000-8000-000000000000" },
    { bytes: Array<number>(16).fill(255), expected: "ffffffff-ffff-4fff-bfff-ffffffffffff" },
    { bytes: Array.from({ length: 16 }, (_, index) => index), expected: "00010203-0405-4607-8809-0a0b0c0d0e0f" },
  ])("缺少 randomUUID 时保留随机字节并设置 v4 / variant 位：$expected", ({ bytes, expected }) => {
    const getRandomValues = vi.fn((target: Uint8Array) => {
      target.set(bytes);
      return target;
    });
    vi.stubGlobal("crypto", { randomUUID: undefined, getRandomValues });

    expect(crypto.randomUUID).toBeUndefined();
    expect(generateUuidV4()).toBe(expected);
    expect(getRandomValues).toHaveBeenCalledExactlyOnceWith(expect.any(Uint8Array));
    expect(getRandomValues.mock.calls[0][0]).toHaveLength(16);
  });

  it("缺少 randomUUID 时仍使用真实 getRandomValues 每次生成新 ID", () => {
    const getRandomValues = vi.fn(crypto.getRandomValues.bind(crypto));
    vi.stubGlobal("crypto", { randomUUID: undefined, getRandomValues });

    const first = generateUuidV4();
    const second = generateUuidV4();
    const pattern = /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;
    expect(first).toMatch(pattern);
    expect(second).toMatch(pattern);
    expect(second).not.toBe(first);
    expect(getRandomValues).toHaveBeenCalledTimes(2);
  });
});

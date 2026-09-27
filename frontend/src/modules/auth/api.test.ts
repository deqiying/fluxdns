import { expect, it, vi } from "vitest";
import { sessionFixture } from "@/mocks/fixtures";
import { setMockSetupRequired } from "@/mocks/handlers";
import { getSession, getSetupStatus, initializeWebUi, login, logout } from "./api";

it("登录签发的 Bearer 仅在内存使用，业务请求不携带 Cookie 或 URL token", async () => {
  setMockSetupRequired(true);
  const fetchSpy = vi.spyOn(globalThis, "fetch");
  const credentials = { username: sessionFixture.user.name, password: "test-only-password-for-auth" };
  try {
    await expect(getSetupStatus()).resolves.toEqual({ state: "required" });
    await initializeWebUi(credentials);
    await expect(getSession()).resolves.toEqual(sessionFixture);
    await logout();
    await expect(getSession()).resolves.toBeNull();
    await login(credentials);
    await logout();

    expect(fetchSpy.mock.calls.map(([input]) => String(input))).toEqual([
      "/api/v2/auth/setup",
      "/api/v2/auth/setup",
      "/api/v2/auth/session",
      "/api/v2/auth/logout",
      "/api/v2/auth/login",
      "/api/v2/auth/logout",
    ]);
    for (const [input, options] of fetchSpy.mock.calls) {
      expect(new URL(String(input), window.location.origin).search).toBe("");
      const headers = new Headers(options?.headers);
      if (String(input).endsWith("/auth/session")) {
        expect(options?.credentials).toBe("omit");
        expect(headers.get("authorization")).toMatch(/^Bearer [A-Za-z0-9_-]{43}$/);
      } else if (String(input).endsWith("/auth/logout")) {
        expect(options?.credentials).toBe("same-origin");
        expect(headers.get("authorization")).toMatch(/^Bearer [A-Za-z0-9_-]{43}$/);
      } else {
        expect(options?.credentials).toBe("same-origin");
        expect(headers.has("authorization")).toBe(false);
      }
      if (options?.body !== undefined) {
        expect(options.method).toBe("POST");
        expect(JSON.parse(String(options.body))).toEqual(credentials);
      }
    }
    expect(window.localStorage.length).toBe(0);
    expect(window.sessionStorage.length).toBe(0);
  } finally {
    fetchSpy.mockRestore();
  }
});

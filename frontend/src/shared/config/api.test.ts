import { http, HttpResponse } from "msw";
import { beforeEach, expect, it } from "vitest";
import { server } from "@/mocks/server";
import { setMockAuthenticated } from "@/mocks/handlers";
import { validateCandidate } from "./api";

beforeEach(() => setMockAuthenticated(true));

it("模块校验使用 v2 typed endpoint 并原样发送双 revision", async () => {
  server.use(http.post("/api/v2/config/modules/logs/validate", async ({ request }) => {
    const body = await request.json();
    expect(body).toMatchObject({
      expected: { active_revision: "active-1", observed_file_revision: "file-1" },
      discard_external_changes: false,
    });
    return HttpResponse.json({
      validation_token: "validation-1",
      expected: { active_revision: "active-1", observed_file_revision: "file-1" },
      expires_at_ms: 1_000,
      required_confirmations: [],
      affected_names: [],
    });
  }));

  await expect(validateCandidate({
    expected: { active_revision: "active-1", observed_file_revision: "file-1" },
    changes: [{ module: "logs", change: { enable: false, level: "warn", path: "logs/fluxdns.log" } }],
    discard_external_changes: false,
  }, { module: "logs" })).resolves.toMatchObject({ validation_token: "validation-1" });
});

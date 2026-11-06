import { http, HttpResponse } from "msw";
import { beforeEach, expect, it } from "vitest";
import { server } from "@/mocks/server";
import { setMockAuthenticated } from "@/mocks/handlers";
import { validateCandidate } from "./api";

beforeEach(() => setMockAuthenticated(true));

it("可选模块校验只发送 changes，并返回无状态影响预览", async () => {
  server.use(http.post("/api/v2/config/modules/logs/validate", async ({ request }) => {
    const body = await request.json();
    expect(Object.keys(body as object)).toEqual(["changes"]);
    return HttpResponse.json({
      observed_file_revision: "file-1",
      required_confirmations: [],
      affected_names: [],
    });
  }));

  await expect(validateCandidate({
    changes: [{ module: "logs", change: { enable: false, level: "warn", path: "logs/fluxdns.log" } }],
  }, { module: "logs" })).resolves.toMatchObject({ observed_file_revision: "file-1" });
});

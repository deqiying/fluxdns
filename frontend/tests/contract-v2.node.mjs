import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import Ajv2020 from "@redocly/ajv/dist/2020.js";
import yaml from "js-yaml";

const openapi = yaml.load(readFileSync(new URL("../openapi/management-api-v2.yaml", import.meta.url), "utf8"));
const definitions = JSON.parse(JSON.stringify(openapi.components.schemas).replaceAll("#/components/schemas/", "#/$defs/"));
const ajv = new Ajv2020({ strict: false, validateFormats: false, allErrors: true });
ajv.addSchema({ $id: "fluxdns-v2", $defs: definitions });
const fixtures = JSON.parse(readFileSync(new URL("../../backend/tests/fixtures/management-v2.json", import.meta.url), "utf8"));
const validate = (name, value) => {
  const validator = ajv.getSchema(`fluxdns-v2#/$defs/${name}`);
  assert.ok(validator, name);
  assert.ok(validator(value), `${name}: ${JSON.stringify(validator.errors)}`);
};

test("共享 JSON 夹具符合正式 v2 schema", () => {
  for (const [key, schema] of Object.entries({candidate: "Candidate", state: "ConfigState", query: "QueryRequest", record: "QueryRecord", metrics: "ServiceMetrics", config_read: "ConfigRead", external_diff: "ExternalDiff", validation: "ValidationResult", snapshot: "CacheSnapshotStatus"})) {
    validate(schema, fixtures[key]);
  }
  for (const item of fixtures.operations) validate("OperationResult", item);
  for (const item of fixtures.client_messages) validate("ClientMessage", item);
  for (const item of fixtures.server_messages) validate("ServerMessage", item);
  for (const item of fixtures.module_sources) validate("ModuleSource", item);
  validate("Configuration", yaml.load(readFileSync(new URL("../../backend/tests/fixtures/config-v2.yaml", import.meta.url), "utf8")));
});

test("schema 拒绝只读字段、身份变更、旧字段和类型残留", () => {
  const candidateValidator = ajv.getSchema("fluxdns-v2#/$defs/Candidate");
  const invalidChanges = [
    {module: "work", change: {path: "other"}},
    {module: "clients", change: {action: "update", original_name: "desktop", value: {name: "renamed", client_id: "altered"}}},
    {module: "clients", change: {action: "delete", original_name: "desktop"}},
    {module: "dns", change: {resolve_log: {enable: true, max_records: 100}}},
    {module: "dns", change: {cache: null}},
  ];
  for (const change of invalidChanges) {
    assert.equal(candidateValidator({...fixtures.candidate, changes: [change]}), false, JSON.stringify(change));
  }
  assert.equal(candidateValidator({...fixtures.candidate, changes: []}), false);
  assert.equal(candidateValidator({...fixtures.candidate, changes: Array(129).fill(fixtures.candidate.changes[0])}), false);
  assert.equal(ajv.getSchema("fluxdns-v2#/$defs/QueryRequest")({...fixtures.query, page_size: 101}), false);
  assert.equal(ajv.getSchema("fluxdns-v2#/$defs/QueryRequest")({...fixtures.query, cursor: "a".repeat(2049)}), false);
  const decimal = ajv.getSchema("fluxdns-v2#/$defs/DecimalU64");
  for (const valid of ["0", "9007199254740992", "10000000000000000000", "18446744073709551615"]) {
    assert.equal(decimal(valid), true, valid);
  }
  for (const invalid of ["18446744073709551616", "19999999999999999999", "20000000000000000000", "01", "-1", "1e3", ""]) {
    assert.equal(decimal(invalid), false, invalid);
  }
});

test("全部 schema 可编译，v2 不声明角色/通用 YAML/顶层删除接口", () => {
  for (const name of Object.keys(definitions)) assert.ok(ajv.getSchema(`fluxdns-v2#/$defs/${name}`), name);
  assert.equal(openapi.servers[0].url, "/api/v2");
  assert.equal(openapi["x-implementation"], "staged");
  assert.deepEqual(openapi["x-implemented-operations"], [
    "getConfigState",
    "getSystemConfig",
    "getConfigModule",
    "validateCandidate",
    "applyCandidate",
    "getConfigOperation",
    "getExternalDiff",
    "restoreConfigFiles",
    "retryConfigPersistence",
    "getRetentionStatus",
    "getServiceMetrics",
    "getProcessMetrics",
    "searchQueries",
    "getQueryDetail",
  ]);
  assert.equal(Object.keys(openapi.paths).some((path) => /roles|users|restart|stop|clear/.test(path)), false);
  for (const path of Object.values(openapi.paths)) assert.equal("delete" in path || "patch" in path, false);
});

test("v2 业务认证只接受 Bearer，Cookie 仅用于认证刷新且 token 不进入普通 session", () => {
  assert.deepEqual(openapi.security, [{bearerAuth: []}]);
  assert.deepEqual(openapi.components.securitySchemes, {
    bearerAuth: {type: "http", scheme: "bearer", bearerFormat: "opaque"},
    refreshCookie: {type: "apiKey", in: "cookie", name: "fluxdns_session"},
  });
  assert.deepEqual(openapi.paths["/auth/refresh"].post.security, [{refreshCookie: []}]);
  for (const [path, operations] of Object.entries(openapi.paths)) {
    if (!path.startsWith("/auth/") && path !== "/events") continue;
    for (const operation of Object.values(operations)) {
      assert.equal((operation.parameters ?? []).some((parameter) => parameter.in === "query"), false, path);
    }
  }
  const session = ajv.getSchema("fluxdns-v2#/$defs/Session");
  assert.ok(session);
  const value = {user: {name: "admin"}, expires_at: "2026-09-07T00:00:00Z"};
  assert.equal(session(value), true);
  assert.equal(session({...value, token: "test-only-token"}), false);
  const auth = {session: value, access_token: "A".repeat(43), token_type: "Bearer", access_expires_at_ms: 1800000000000};
  validate("AuthSession", auth);
  const validator = ajv.getSchema("fluxdns-v2#/$defs/AuthSession");
  assert.equal(validator({...auth, refresh_token: "B".repeat(43)}), false);
  assert.equal(validator({...auth, access_expires_at_ms: 9007199254740992}), false);
  assert.equal(validator({...auth, access_token: "invalid"}), false);
});

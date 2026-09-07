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
  assert.equal(openapi["x-implementation"], "contract-only");
  assert.equal(Object.keys(openapi.paths).some((path) => /roles|users|restart|stop|clear/.test(path)), false);
  for (const path of Object.values(openapi.paths)) assert.equal("delete" in path || "patch" in path, false);
});

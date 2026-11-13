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
  for (const [key, schema] of Object.entries({candidate: "Candidate", state: "ConfigState", query: "QueryRequest", record: "QueryRecord", metrics: "ServiceMetrics", websocket_ticket: "WebSocketTicket", config_read: "ConfigRead", external_diff: "ExternalDiff", validation: "ValidationResult", snapshot: "CacheSnapshotStatus"})) {
    validate(schema, fixtures[key]);
  }
  for (const item of fixtures.operations) validate("OperationResult", item);
  for (const item of fixtures.client_messages) validate("ClientMessage", item);
  for (const item of fixtures.server_messages) validate("ServerMessage", item);
  for (const item of fixtures.module_sources) validate("ModuleSource", item);
  validate("Configuration", yaml.load(readFileSync(new URL("../../backend/tests/fixtures/config-v2.yaml", import.meta.url), "utf8")));
});

test("运行时展示时区必填，UTC 降级原因使用安全枚举且不改时间戳", () => {
  validate("ProcessMetrics", fixtures.runtime);
  validate("ProcessMetrics", {...fixtures.runtime, timezone: "UTC", timezone_fallback_reason: "invalid_tz"});
  const validator = ajv.getSchema("fluxdns-v2#/$defs/ProcessMetrics");
  for (const key of ["timezone", "timezone_fallback_reason"]) {
    const missing = {...fixtures.runtime};
    delete missing[key];
    assert.equal(validator(missing), false, key);
  }
  assert.equal(validator({...fixtures.runtime, timezone: ""}), false);
  assert.equal(validator({...fixtures.runtime, timezone_fallback_reason: "raw-system-error"}), false);
  assert.equal(validator({...fixtures.runtime, started_at_ms: "2026-11-11T00:00:00+08:00"}), false);
});

test("内存分析区分 OS 实测、已知载荷和计费权重，未采集不填零", () => {
  const memory = JSON.parse(readFileSync(new URL("../../backend/tests/fixtures/memory-analysis-v2.json", import.meta.url), "utf8"));
  validate("MemoryAnalysis", memory);
  validate("MemoryAnalysis", {...memory, snapshot: null, previous_snapshot: null, collection_state: "collecting"});
  validate("MemoryAnalysis", {...memory, collection_state: "failed", last_failure: "timeout"});
  const check = ajv.getSchema("fluxdns-v2#/$defs/MemoryAnalysis");
  assert.equal(check({...memory, trend: Array(122).fill(memory.os)}), false);
  assert.equal(check({...memory, os: {...memory.os, rss_bytes: {state: "available", value: 1024}}}), false);
  assert.equal(check({...memory, last_failure: "raw-private-error"}), false);
  assert.equal(memory.snapshot.modules.find(module => module.id === "allocator").estimated_bytes, null);
  assert.equal(memory.snapshot.modules.find(module => module.id === "dns_cache").estimated_bytes, null);
  assert.equal(memory.snapshot.modules.find(module => module.id === "dns_cache").counters[1].kind, "billing_bytes");
});

test("schema 拒绝只读字段、非法身份、旧字段和类型残留", () => {
  const candidateValidator = ajv.getSchema("fluxdns-v2#/$defs/Candidate");
  const invalidChanges = [
    {module: "work", change: {path: "other"}},
    {module: "clients", change: {action: "update", original_name: "desktop", value: {name: "renamed", client_id: "bad id"}}},
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
    "validateModuleCandidate",
    "applyModuleCandidate",
    "validateCandidate",
    "applyCandidate",
    "getConfigOperation",
    "getExternalDiff",
    "restoreConfigFiles",
    "retryConfigPersistence",
    "getRetentionStatus",
    "previewRetention",
    "getServiceMetrics",
    "getProcessMetrics",
    "getMemoryAnalysis",
    "collectMemorySnapshot",
    "searchQueries",
    "getQueryDetail",
    "issueWebSocketTicket",
    "subscribeEvents",
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
    if (!path.startsWith("/auth/") && !path.startsWith("/events")) continue;
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
  assert.deepEqual(openapi.paths["/events"].get.security, []);
  assert.equal(openapi.paths["/events"].get["x-websocket-protocol"], "fluxdns.v1");
});

test("简化写接口只接受 changes 与显式风险确认，保留只读 revision", () => {
  const changes = [{module: "logs", change: {enable: true, level: "info", path: "logs/fluxdns.log"}}];
  const apply = {operation_id: "operation-1", changes};
  validate("Candidate", {changes});
  validate("ApplyRequest", apply);
  validate("ApplyRequest", {...apply, confirmations: ["discard_external_changes"], observed_file_revision: "files-2"});
  validate("ValidationResult", {observed_file_revision: "files-2", required_confirmations: ["retention_shortening"], affected_names: []});
  validate("FileSyncRequest", {operation_id: "operation-1", observed_file_revision: "files-2"});
  validate("RetentionPreviewRequest", {policy: {retention: {days: 7}}});
  validate("RetentionPreview", {sampled_at_ms: 1, detail_bytes: "0", proposed_cutoff_utc_date: "2026-11-01", shortens_history: false});
  const rejects = (schema, value) => assert.equal(ajv.getSchema(`fluxdns-v2#/$defs/${schema}`)(value), false, schema);
  for (const old of [{expected: fixtures.external_diff.expected}, {candidate: {changes}}, {validation_token: "ticket"}, {discard_external_changes: true}]) {
    rejects("ApplyRequest", {...apply, ...old});
  }
  rejects("Candidate", {changes, expected: fixtures.external_diff.expected});
  for (const impact of ["rename_references", "listener_rebind"]) rejects("ApplyRequest", {...apply, confirmations: [impact]});
  rejects("ValidationResult", {...fixtures.validation, validation_token: "ticket"});
  rejects("FileSyncRequest", {operation_id: "operation-1", observed_file_revision: "files-2", expected: fixtures.external_diff.expected});
  rejects("RetentionPreviewRequest", {policy: {}, expected: fixtures.external_diff.expected});
  rejects("RetentionPreview", {sampled_at_ms: 1, detail_bytes: "0", proposed_cutoff_utc_date: "2026-11-01", shortens_history: false, expected: fixtures.external_diff.expected});
  for (const code of ["CONFIRMATION_REQUIRED", "EXTERNAL_CHANGES_REQUIRE_CONFIRMATION"]) {
    validate("ErrorEnvelope", {code, request_id: "confirm-1", message: "confirmation required", retryable: false, field_errors: []});
  }
  assert.ok(definitions.ConfigState.properties.active_revision);
  assert.ok(definitions.OperationStatus.oneOf.some((variant) => variant.properties.active_revision));
  assert.ok(definitions.ExternalDiff.properties.expected);
  assert.ok(definitions.ErrorCode.enum.includes("CONFIRMATION_REQUIRED"));
  assert.ok(definitions.ErrorCode.enum.includes("ACTIVE_REVISION_CONFLICT"));
  assert.ok(!definitions.ErrorCode.enum.includes("VALIDATION_EXPIRED"));
});

test("安全 diagnostic 可选且独立于原错误码和 operation 状态", () => {
  const diagnostic = {stage: "prepare", code: "RESOURCE_PARSE_FAILED", message: "规则格式不合法", resource: "guard"};
  const envelope = {code: "APPLY_FAILED", request_id: "request-safe", message: "request could not be completed", retryable: false, field_errors: []};
  validate("ErrorEnvelope", envelope);
  validate("ErrorEnvelope", {...envelope, diagnostic});
  for (const operation of fixtures.operations) {
    validate("OperationResult", operation);
    validate("OperationResult", {...operation, diagnostic});
  }
  const validator = ajv.getSchema("fluxdns-v2#/$defs/Diagnostic");
  for (const [field, limit] of Object.entries({stage: 64, code: 64, message: 1024, resource: 128})) {
    assert.equal(definitions.Diagnostic.properties[field]["x-max-utf8-bytes"], limit);
    assert.equal(validator({...diagnostic, [field]: "x".repeat(limit + 1)}), false, field);
  }
  assert.equal(validator({stage: "prepare", code: "CODE"}), false);
  assert.equal(validator({...diagnostic, unsafe_detail: "not allowed"}), false);
});

test("ECS 状态、IPv6、缓存刷新和旧缺省使用同一记录契约", () => {
  for (const ecs of fixtures.ecs_cases) {
    validate("QueryEcs", ecs);
    validate("QueryRecord", {...fixtures.record, ecs});
    validate("QueryDetail", {record: {...fixtures.record, ecs}, directory_revision: "directory-1"});
    validate("ServerMessage", {type: "queries", subscription_id: "queries-ecs", cursor: {epoch: "stream-ecs", sequence: "1"}, directory_revision: "directory-1", items: [{...fixtures.record, ecs}]});
  }
  for (const ecs of fixtures.ecs_invalid_cases) {
    assert.equal(ajv.getSchema("fluxdns-v2#/$defs/QueryEcs")(ecs), false);
  }
  const legacy = {...fixtures.record};
  delete legacy.ecs;
  validate("QueryRecord", legacy);
  validate("QueryRecord", {...legacy, ecs: null});
  assert.equal(definitions.QueryRecord.required.includes("ecs"), false);
  assert.equal(definitions.QueryEcs["x-max-json-bytes"], 4096);
  for (const cidr of ["::/0", "::1/128", "2001:db8::/32", "::ffff:192.0.2.0/120"]) {
    validate("EcsValue", {state: "present", cidr, scope_prefix: 128});
  }
  for (const cidr of [":::/0", "1::2::/64", "2001:db8::/129", "999.0.0.0/24", "192.0.2.0/33"]) {
    assert.equal(ajv.getSchema("fluxdns-v2#/$defs/EcsValue")({state: "present", cidr, scope_prefix: 0}), false, cidr);
  }
  for (const field of ["request", "upstream", "cache_match", "refresh"]) {
    const invalid = {...fixtures.ecs_cases[0]};
    delete invalid[field];
    assert.equal(ajv.getSchema("fluxdns-v2#/$defs/QueryEcs")(invalid), false, field);
  }
});

test("ECS 请求优先回退配置约束与诊断 mode 同步", () => {
  const validator = ajv.getSchema("fluxdns-v2#/$defs/Ecs");
  for (const mode of ["custom", "client_or_custom"]) {
    for (const custom_ip of ["192.0.2.0/24", "2001:db8::/48"]) validate("Ecs", {mode, custom_ip});
    assert.equal(validator({mode}), false, mode);
    assert.equal(validator({mode, custom_ip: ""}), false, mode);
  }
  for (const mode of ["client", "disabled"]) {
    validate("Ecs", {mode});
    assert.equal(validator({mode, custom_ip: "192.0.2.0/24"}), false, mode);
  }
  for (const value_source of ["client_ecs", "custom"]) {
    validate("EcsPolicy", {source: "rule", mode: "client_or_custom", value_source});
  }
  assert.equal(validator({mode: "unknown"}), false);
});

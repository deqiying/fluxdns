// 仅操作显式指定的 _fluxdns 本地夹具；凭据从该目录的 test-context.json 读取，不写报告。
// 夹具需含 guard Hosts、local hosts 上游、default 策略及 client-01..10；见本地测试规范。
import assert from 'node:assert/strict';
import { readFile, writeFile, stat, realpath } from 'node:fs/promises';
import { resolve, relative, sep, isAbsolute } from 'node:path';
import { fileURLToPath } from 'node:url';
import { randomUUID } from 'node:crypto';
import { createSocket } from 'node:dgram';
import { connect } from 'node:net';
import { request as httpRequest } from 'node:http';
import { setTimeout as delay } from 'node:timers/promises';

const root = resolve(fileURLToPath(new URL('..', import.meta.url)));
const work = await realpath(resolve(root, process.argv[2] ?? ''));
const inside = relative(await realpath(resolve(root, '_fluxdns')), work);
assert(inside && !isAbsolute(inside) && inside !== '..' && !inside.startsWith(`..${sep}`), '必须指定 _fluxdns 下的独立测试目录');
const context = JSON.parse(await readFile(resolve(work, 'test-context.json'), 'utf8'));
assert.equal(context.kind, 'fluxdns-webui-local-acceptance');
const { username, password, webPort, dnsPort, dohPort } = context;
for (const port of [webPort, dnsPort, dohPort]) assert(Number.isInteger(port) && port > 1024 && port < 65536);
const origin = `http://127.0.0.1:${webPort}`;
const report = { startedAt: new Date().toISOString(), node: process.version, checks: [], status: 'incomplete' };
let token;
let cookie;
let dnsId = 0;
const mark = (name, details = {}) => { report.checks.push({ name, ...details }); console.log(`PASS ${name}`); };

async function api(path, { method = 'GET', body, auth = true, headers = {}, status = 200 } = {}) {
  const response = await fetch(`${origin}${path.startsWith('/api/') ? path : `/api/v2${path}`}`, {
    method, headers: { Origin: origin, ...(auth && token ? { Authorization: `Bearer ${token}` } : {}),
      ...(body !== undefined ? { 'Content-Type': 'application/json' } : {}), ...headers },
    body: body === undefined ? undefined : typeof body === 'string' ? body : JSON.stringify(body), signal: AbortSignal.timeout(10000),
  }).catch(error => { throw new Error(`${method} ${path}: ${error.cause?.code ?? error.message}`); });
  const text = await response.text();
  const data = text ? JSON.parse(text) : undefined;
  assert.equal(response.status, status, `${method} ${path} HTTP status (${data?.code ?? 'response'})`);
  if (status >= 400) {
    assert.equal(typeof data.request_id, 'string');
    assert(Array.isArray(data.field_errors));
    assert(!text.includes(password) && !text.includes('$argon2'));
  }
  return { data, response };
}
const get = async path => (await api(path)).data;
const expected = state => ({ active_revision: state.active_revision, observed_file_revision: state.observed_file_revision });
async function candidate(changes) {
  return { expected: expected(await get('/config/state')), changes, discard_external_changes: false };
}
async function settle(operation) {
  for (let attempt = 0; ['preparing', 'applying', 'persisting'].includes(operation.status.state) && attempt < 50; attempt++) {
    await delay(100);
    operation = await get(`/config/operations/${operation.operation_id}`);
  }
  assert.equal(operation.status.state, 'applied_synced', '配置必须真正应用且持久化');
  return operation;
}
async function apply(value) {
  const validation = (await api('/config/validate', { method: 'POST', body: value })).data;
  return settle((await api('/config/apply', { method: 'POST', status: 202, body: {
    operation_id: randomUUID(), candidate: value, validation_token: validation.validation_token,
    confirmations: validation.required_confirmations,
  } })).data);
}
const update = (module, original_name, value) => ({ module, change: { action: 'update', original_name, value } });

function queryWire(name) {
  const header = Buffer.alloc(12); header.writeUInt16BE(++dnsId % 65536); header.writeUInt16BE(0x100, 2); header.writeUInt16BE(1, 4);
  return Buffer.concat([header, ...name.replace(/\.$/, '').split('.').map(label => {
    const bytes = Buffer.from(label); assert(bytes.length <= 63); return Buffer.concat([Buffer.from([bytes.length]), bytes]);
  }), Buffer.from([0, 0, 1, 0, 1])]);
}
// DNS body 断言与 HTTP 状态独立；用唯一目标 IPv4 的 A RDATA 核对真实生效。
function checkAnswer(wire, body, answer) {
  assert(body.length >= 12); assert.equal(body.readUInt16BE(), wire.readUInt16BE());
  assert.equal(body[3] & 15, answer ? 0 : 2, 'DNS RCODE');
  if (answer) {
    assert(body.readUInt16BE(6) > 0);
    assert(body.includes(Buffer.from(answer.split('.').map(Number))), 'DNS answer 未实际改变');
  }
}
async function dns(transport, name, answer, client = 'client-01') {
  const wire = queryWire(name);
  let body;
  if (transport === 'udp') {
    body = await new Promise((accept, reject) => {
      const socket = createSocket('udp4');
      const timeout = setTimeout(() => { socket.close(); reject(new Error('UDP timeout')); }, 5000);
      socket.once('error', error => { clearTimeout(timeout); socket.close(); reject(error); });
      socket.once('message', value => { clearTimeout(timeout); socket.close(); accept(value); });
      socket.send(wire, dnsPort, '127.0.0.1');
    });
  } else if (transport === 'tcp') {
    body = await new Promise((accept, reject) => {
      const socket = connect({ port: dnsPort, host: '127.0.0.1' }); let bytes = Buffer.alloc(0);
      socket.setTimeout(5000, () => socket.destroy(new Error('TCP timeout'))); socket.once('error', reject);
      socket.once('connect', () => { const length = Buffer.alloc(2); length.writeUInt16BE(wire.length); socket.write(Buffer.concat([length, wire])); });
      socket.on('data', value => { bytes = Buffer.concat([bytes, value]); if (bytes.length >= 2 && bytes.length >= bytes.readUInt16BE() + 2) { socket.end(); accept(bytes.subarray(2, bytes.readUInt16BE() + 2)); } });
    });
  } else {
    const url = `http://127.0.0.1:${dohPort}/dns-query/${client}`;
    const response = await fetch(transport === 'doh-get' ? `${url}?dns=${wire.toString('base64url')}` : url, {
      method: transport === 'doh-get' ? 'GET' : 'POST', headers: { 'Content-Type': 'application/dns-message', Accept: 'application/dns-message' },
      body: transport === 'doh-get' ? undefined : wire, signal: AbortSignal.timeout(5000),
    });
    assert.equal(response.status, 200); assert.equal(response.headers.get('content-type'), 'application/dns-message');
    body = Buffer.from(await response.arrayBuffer());
  }
  try { checkAnswer(wire, body, answer); }
  catch (error) { throw new Error(`${transport} ${name} rcode=${body[3] & 15}: ${error.message}`); }
}
async function page(filter) {
  return (await api('/queries/search', { method: 'POST', body: { filter, cursor: null, direction: 'older', page_size: 100, sort: 'occurred_at', order: 'desc' } })).data;
}

try {
  const login = await api('/auth/login', { method: 'POST', auth: false, body: { username, password } });
  token = login.data.access_token; cookie = login.response.headers.get('set-cookie')?.split(';')[0];
  assert(token && cookie); assert.match(login.response.headers.get('set-cookie'), /HttpOnly/i);
  mark('login-httpOnly-cookie');
  // 真实在线身份和 QPS 的首个完整窗口为 60 秒；先验证缺数，再进入可核算流量段。
  let readyMetrics = await get('/service/metrics');
  const warmupDeadline = Date.now() + 65_000;
  while (readyMetrics.online_clients.state === 'unavailable' && Date.now() < warmupDeadline) {
    assert.equal(readyMetrics.online_clients.reason, 'warmup');
    assert(Number.isSafeInteger(readyMetrics.online_clients.observed_seconds));
    await delay(1000);
    readyMetrics = await get('/service/metrics');
  }
  assert.equal(readyMetrics.online_clients.state, 'available', '首个采样窗口未在预算内完成');
  await api('/service/metrics', { auth: false, headers: { Cookie: cookie }, status: 401 });
  await api('/events/ticket', { method: 'POST', headers: { Origin: 'http://invalid.test' }, status: 403 });
  // 声明超限 Content-Length 后先等响应，避免客户端仍上传时 TCP 关闭表现为 ECONNRESET。
  const oversizedStatus = await new Promise((accept, reject) => {
    const request = httpRequest(`${origin}/api/v2/config/validate`, { method: 'POST', headers: {
      Origin: origin, Authorization: `Bearer ${token}`, 'Content-Type': 'application/json', 'Content-Length': 2 * 1024 * 1024 + 1,
    } }, response => { response.resume(); response.once('end', () => accept(response.statusCode)); });
    request.setTimeout(5000, () => request.destroy(new Error('oversize response timeout')));
    request.once('error', reject); request.flushHeaders();
  });
  assert.equal(oversizedStatus, 413);
  const injection = await candidate([{ module: 'work', change: { path: '../escape' } }]);
  await api('/config/validate', { method: 'POST', body: injection, status: 400 });
  for (const path of ['/api/v1/runtime', '/api/v1/auth/login', '/api/v2/unknown']) {
    await api(path, { headers: { Accept: 'text/html' }, status: 404 });
  }
  const system = JSON.stringify(await get('/config/system'));
  assert(!system.includes('$argon2') && !system.includes(password) && !system.includes('password_hash'));
  mark('bearer-origin-readonly-size-legacy-boundaries');
  const hosts = (await get('/config/modules/hosts')).values.find(row => row.value.name === 'guard').value;
  const changedHosts = { ...hosts, hosts: '192.0.2.2 sentinel.p5.test' };
  const stale = await candidate([update('hosts', 'guard', hosts)]);
  await apply(await candidate([update('hosts', 'guard', changedHosts)]));
  for (const transport of ['udp', 'tcp', 'doh-get', 'doh-post']) await dns(transport, 'sentinel.p5.test', '192.0.2.2');
  await api('/config/validate', { method: 'POST', body: stale, status: 409 });
  const invalid = await candidate([update('hosts', 'guard', { ...hosts, name: '<img src=x onerror=alert(1)>' })]);
  await api('/config/validate', { method: 'POST', body: invalid, status: 422 });
  await dns('udp', 'sentinel.p5.test', '192.0.2.2');
  await apply(await candidate([update('hosts', 'guard', hosts)]));
  mark('hot-apply-four-transports-conflict-invalid-candidate');
  const upstream = (await get('/config/modules/upstreams')).values.find(row => row.value.name === 'local').value;
  await apply(await candidate([update('upstreams', 'local', { ...upstream, name: 'local-renamed' })]));
  assert((await get('/config/modules/strategy')).values.some(row => row.value.default_upstream === 'local-renamed'));
  await dns('doh-post', 'cache.p5.test', '198.51.100.9');
  await apply(await candidate([update('upstreams', 'local-renamed', upstream)]));
  mark('upstream-rename-reference-and-dns');
  // 将这组请求与前序 DNS 分开；进程冻结墙钟与客户端 Date.now 不要求同一毫秒采样。
  await delay(200);
  const start = Date.now() - 100;
  for (let client = 1; client <= 10; client++) await dns('doh-post', 'cache.p5.test', '198.51.100.9', `client-${String(client).padStart(2, '0')}`);
  await dns('doh-get', 'cache.p5.test', '198.51.100.9', 'unknown-client');
  await dns('udp', 'cache.p5.test', '198.51.100.9');
  await dns('udp', '<svg onload=alert(1)>.p5.test');
  const filter = { from_ms: start, to_ms: Date.now() + 100 };
  let history;
  for (let attempt = 0; attempt < 50; attempt++) { history = await page(filter); if (history.items.length >= 13) break; await delay(100); }
  assert.equal(history.items.length, 13);
  for (let client = 1; client <= 10; client++) {
    const id = `client-${String(client).padStart(2, '0')}`;
    assert(history.items.some(row => row.identity.client_id === id && row.matched.matched_client_id === id && row.matched.source === 'id'));
  }
  const fallback = history.items.find(row => row.identity.client_id === 'unknown-client');
  assert.equal(fallback.matched.matched_client_id, 'client-01'); assert.equal(fallback.matched.source, 'ip');
  const frozen = history.items.find(row => row.identity.client_id === 'client-02');
  const client = (await get('/config/modules/clients')).values.find(row => row.value.client_id === 'client-02').value;
  const { client_id: ignored, ...editable } = client;
  await apply(await candidate([update('clients', client.name, { ...editable, name: 'client-02-renamed' })]));
  const detail = (await get(`/queries/${frozen.id}`)).record;
  assert.deepEqual(detail.matched, frozen.matched); assert.equal(detail.current_client_name, 'client-02-renamed');
  await apply(await candidate([update('clients', 'client-02-renamed', editable)]));
  mark('ten-identities-unknown-id-fallback-historical-rename', { records: history.items.length });
  const sourcePath = resolve(work, 'config.yaml');
  const source = await readFile(sourcePath, 'utf8');
  const stateBefore = await get('/config/state');
  await writeFile(sourcePath, `${source}\n# local acceptance external observation\n`);
  let observed;
  for (let attempt = 0; attempt < 50; attempt++) { observed = await get('/config/state'); if (observed.files.source === 'changed') break; await delay(200); }
  assert.equal(observed.files.source, 'changed'); assert.equal(observed.runtime_revision, stateBefore.runtime_revision);
  await get('/config/files/diff');
  await settle((await api('/config/files/restore', { method: 'POST', body: {
    operation_id: randomUUID(), expected: expected(observed), discard_external_changes: true,
  } })).data);
  assert.equal((await get('/config/state')).runtime_revision, stateBefore.runtime_revision);
  mark('external-file-observe-restore-without-reload');
  const service = await get('/service/metrics'); const processInfo = await get('/system/runtime');
  assert.equal(service.rss_bytes.state, 'available'); assert.equal(processInfo.rss_bytes.state, 'available');
  assert.equal(service.rss_bytes.value, processInfo.rss_bytes.value);
  assert(service.online_clients.value >= 10); assert(processInfo.uptime_seconds > 0);
  assert((await stat(resolve(work, 'data/cache.snapshot'))).size > 0);
  const retention = await get('/retention'); assert(Number.isSafeInteger(retention.next_scheduled_at_ms));
  mark('shared-process-metrics-snapshot-and-retention', { retentionLastCompletedAt: retention.last_completed_at_ms });
  await api('/auth/logout', { method: 'POST', headers: { Cookie: cookie }, status: 204 });
  await api('/service/metrics', { status: 401 });
  await api('/auth/refresh', { method: 'POST', auth: false, headers: { Cookie: cookie }, status: 401 });
  mark('logout-revokes-access-and-refresh');
  report.status = 'passed';
} catch (error) {
  report.status = 'failed'; report.failure = error.message; process.exitCode = 1;
} finally {
  // 异常时也关闭本脚本创建的会话；不覆盖失败前留下的运行态/证据。
  if (report.status !== 'passed' && token) {
    try { await api('/auth/logout', { method: 'POST', headers: { Cookie: cookie }, status: 204 }); }
    catch (error) { report.logoutFailure = error.message; }
  }
  report.finishedAt = new Date().toISOString();
  const path = resolve(work, `http-report-${Date.now()}.json`);
  await writeFile(path, `${JSON.stringify(report, null, 2)}\n`);
  console.log(JSON.stringify({ status: report.status, report: path, failure: report.failure }));
}

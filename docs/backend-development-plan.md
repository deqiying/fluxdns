# FluxDNS 后端开发计划

> 状态：v1 模块方案已完成，阶段 1、阶段 2 已完成；阶段 3 基础服务编排、阶段 4 UDP/TCP 基础链路、阶段 5 upstream 首轮小阶段、阶段 6 cache 首轮切片、阶段 7 resource/policy/DNS Core 首轮接线、阶段 8 DoH plain HTTP 首轮接入和阶段 9 统计/观测纯领域切片已实现
>
> 更新日期：2026-09-01
>
> 总体架构：[backend-architecture.md](backend-architecture.md)
>
> 配置契约：[configuration-reference.md](configuration-reference.md)

## 1. 当前进度结论

仓库已固定 `backend/` 与 `frontend/` 两个独立代码主目录；根目录不作为任一端的工程目录。`backend/` 已具备单 binary crate、核心契约、Config 配置系统、Runtime 候选骨架和基础服务启动闭环；阶段 2 记录起点为 69 个单元测试，当前全量测试为 319 个。Config 已完成自身的严格加载、v1 空迁移 registry、路径/SecretRef source normalization、semantic validation、reference graph、bind plan、安全快照和不可变 `ResolvedConfig`；Runtime 已完成 `RuntimeSnapshot`、`PreparedRuntime`、无 socket preflight、基于 `SocketFactory` 的 BindPlan 全成/全退、`ArcSwap` ActiveRuntime coordinator/CAS、请求 guard、Supervisor task tree 基础、系统 socket capability、Application CLI/校验接线和服务任务编排；Transport/DNS Core 已完成共享 wire boundary、固定 SERVFAIL/hosts core、UDP/TCP adapter、UDP 截断、TCP 持久 session 和 DoH plain HTTP adapter/service 首轮链路，并已将 const/file hosts 资源接入本地 Core；Upstream 已完成内联 hosts exchange、可注入 DoH exchange、plain HTTP DoH transport、可注入地址解析 port、bootstrap 引用元数据透传、bootstrap 响应地址提取、hosts/plain HTTP DoH registry、PolicyCore direct request path、group member selection 和结果聚合/fallback 判定，但 HTTPS/TLS、bootstrap/proxy、真实 outbound 和 Runtime snapshot 接线仍未完成；Cache 已完成无外部依赖的内存 `CacheStore`、容量淘汰、响应准入/TTL、稳定 key builder、`CacheFacade`、single-flight 和版本化文件快照 persistence 边界；Policy 已完成 client/strategy/route immutable index、const/file hosts/rule-set loader 接线、direct hosts/plain HTTP DoH registry wiring、注入式 DoH request path、请求级资源规则匹配和安全的 matched-rule 摘要；Resource 已完成 hosts/rule parser、受限 regex、const/file loader、资源 snapshot/CAS、远程 manifest/content 原子落盘和恢复校验，以及 scheduler/coordinator 的 Runtime-facing 纯逻辑编排；Storage 已完成纯内存统计 epoch/batch ledger、业务 migration schema 和可替换 stats writer contract；Observability 已完成有界 metrics/health registry。DoH TLS/PROXY/forwarded、bootstrap/连接执行、Moka/SQLite persistence、真实 resource fetch/parse/persist worker、完整 DNS Core→Policy→Cache→Upstream 管线、真实 SQLite/detail/telemetry writer 仍未实现。

| 口径 | 当前值 | 说明 |
| --- | ---: | --- |
| 模块方案覆盖率 | 100% | 本计划覆盖 12 个后端顶层模块，每个模块均有独立方案文档 |
| 后端代码实现进度 | **53.4%** | Config 达到 100% 模块验收口径；DNS Core/Policy/Resource 已具备资源 hosts/rule 的主 happy path 和 focused tests，Cache 已增加容量淘汰与文件快照边界，Upstream 已增加结果聚合、可注入 DoH exchange、plain HTTP transport、地址解析 port、bootstrap 引用透传与响应地址提取、registry wiring 和注入式 PolicyCore request path，Storage 已增加业务 schema 与 writer contract，Resource 已增加 scheduler/coordinator 的 Runtime-facing 编排边界；仍缺少 TLS、代理信任、HTTPS 出站、bootstrap 实际查询、Runtime snapshot 资源接线、Moka/SQLite persistence、真实 resource worker、完整 upstream/Core 管线和完整故障验收 |
| v1 交付总进度 | **58.1%** | 设计阶段 10% 已完成，加上实现与验收部分的 `90% × 53.4%` |

后续日常更新以“后端代码实现进度”为主指标，避免文档完成造成进度虚高。v1 交付总进度按以下公式计算：

```text
v1 交付总进度 = 10% × 设计阶段完成度 + 90% × 后端代码实现进度
```

## 2. v1 范围

v1 交付范围：

- 单 Rust binary、单进程、Tokio 异步运行时；
- 严格的 `version: 1` 配置加载、迁移框架、归一化和语义校验；
- UDP、TCP、DoH 入站；
- DoH、内联 hosts 和上游组；
- client、strategy、hosts、rule_set 策略分流；
- 内存缓存、独立 SQLite 持久化缓存；
- SQLite 聚合统计、可选解析详情；
- 资源首次加载、定时刷新、原子 snapshot 发布；
- 结构化日志、故障分级、优雅停机和故障注入验证。

明确不计入 v1：

- DoT、DoQ；
- WebUI、管理 API、认证和 session；
- 配置热加载的外部命令或管理接口；
- 上游主动健康检查；
- 远程规则 expected checksum/version pin 配置；
- Prometheus、OpenTelemetry 等外部 metrics exporter。

架构为未来能力保留 port 和 runtime 边界，不代表 v1 要实现对应功能。

## 3. 模块方案与实现进度

模块进度是代码与验证进度，不包含方案文档本身。权重用于计算后端代码实现总进度，合计 100%。

| 模块 | 目标代码 | 方案文档 | 设计状态 | 实现状态 | 实现进度 | 权重 |
| --- | --- | --- | --- | --- | ---: | ---: |
| Application | `backend/src/main.rs`、`backend/src/app.rs` | [application.md](backend-modules/application.md) | 已完成 | 实现中 | 45% | 4% |
| Ports | `backend/src/ports/*` | [ports.md](backend-modules/ports.md) | 已完成 | 实现中 | 35% | 8% |
| Config | `backend/src/config/*` | [config.md](backend-modules/config.md) | 已完成 | 已验证 | 100% | 10% |
| Runtime | `backend/src/runtime/*` | [runtime.md](backend-modules/runtime.md) | 已完成 | 实现中 | 35% | 12% |
| Transport | `backend/src/transport/*` | [transport.md](backend-modules/transport.md) | 已完成 | 实现中 | 50% | 11% |
| DNS Core | `backend/src/dns/*` | [dns-core.md](backend-modules/dns-core.md) | 已完成 | 实现中 | 50% | 10% |
| Policy | `backend/src/policy/*` | [policy.md](backend-modules/policy.md) | 已完成 | 实现中 | 55% | 8% |
| Upstream | `backend/src/upstream/*` | [upstream.md](backend-modules/upstream.md) | 已完成 | 实现中 | 69% | 10% |
| Cache | `backend/src/cache/*` | [cache.md](backend-modules/cache.md) | 已完成 | 实现中 | 50% | 9% |
| Resource | `backend/src/resource/*` | [resource.md](backend-modules/resource.md) | 已完成 | 实现中 | 65% | 7% |
| Storage | `backend/src/storage/*`、`backend/migrations/*` | [storage.md](backend-modules/storage.md) | 已完成 | 实现中 | 35% | 8% |
| Observability | `backend/src/observability.rs` | [observability.md](backend-modules/observability.md) | 已完成 | 实现中 | 30% | 3% |

后端代码实现总进度：

```text
4% × 45% + 8% × 35% + 10% × 100% + 12% × 35% + 11% × 50% + 10% × 50% + 8% × 55% + 10% × 69% + 9% × 50% + 7% × 65% + 8% × 35% + 3% × 30% ≈ 53.4%
```

## 4. 进度判定规则

每个模块只能按可核验里程碑提升进度，不能手工估算：

| 进度 | 最低证据 |
| ---: | --- |
| 0% | 尚无可运行代码 |
| 20% | 模块骨架和公共契约可编译，基础错误类型已建立 |
| 50% | 主 happy path 已实现，并有针对性单元测试 |
| 70% | 已接入 `RuntimeSnapshot` / `DnsCore` 等真实跨模块链路 |
| 85% | 异常、取消、并发、资源限制和安全边界测试通过 |
| 100% | 集成测试、故障注入、验收项和文档回链全部完成 |

状态枚举：

- `未开始`：0%，没有实现证据；
- `实现中`：20%～84%；
- `已实现待验证`：85%～99%；
- `已验证`：100%；
- `阻塞`：存在会改变契约或无法继续实现的外部决策。

跨模块验收失败时，相关模块不能标记 100%。例如 DoH 请求可以返回结果但 cancellation、PROXY trust boundary 或 DNS/HTTP 错误分层未验证时，Transport 仍不得标记“已验证”。

## 5. 依赖与实现顺序

依赖方向保持为：

```text
DNS / policy / cache / resource domain model
                 ↓
               ports
                 ↓
transport / upstream / storage / observability adapters
                 ↓
              runtime
                 ↓
          application / binary
```

推荐分阶段推进：

### 阶段 0：方案基线

状态：**已完成**

- 完成总体架构、配置契约、12 个模块方案和本计划；
- 固定 v1 范围、模块所有权、失败语义和验证口径；
- 后续变更若修改配置字段，先更新 [configuration-reference.md](configuration-reference.md)。

### 阶段 1：项目骨架与核心契约

状态：**已完成**

涉及：Application、Ports、DNS Core、Observability。

- 在 `backend/` 创建 binary crate、锁定依赖和 feature；
- 定义 canonical DNS message、request context、deadline/cancellation；
- 建立 port、错误分类、测试 fake 和最小日志初始化；
- 验收证据：
  - `cargo fmt --manifest-path backend/Cargo.toml --all -- --check`：通过；
  - `cargo check --manifest-path backend/Cargo.toml --locked`：通过；
  - `cargo test --manifest-path backend/Cargo.toml --locked`：46 passed，0 failed；
  - `cargo clippy --manifest-path backend/Cargo.toml --locked -- -D warnings`：通过；
  - `cargo run --manifest-path backend/Cargo.toml --locked`：输出一条名为 `scaffold_ready` 的 bootstrap INFO 日志并正常退出，不加载配置、不绑定 listener。

### 阶段 2：配置系统

涉及：Config。

状态：**已完成**

- 已完成 version 1 strict DTO、bounded UTF-8 YAML loader、字段路径错误和未知字段拒绝；
- 已完成 v1 空 migration registry/report，保留未来显式迁移链边界；
- 已完成路径和 SecretRef source normalization。SecretRef 实际值不在普通 YAML load 中读取，仅由后续 adapter 通过显式 accessor 请求，并保留脱敏边界；
- 已完成 semantic validation、reference graph/cycle、继承、bind plan 和 WebUI feature gate；
- 已生成不可变 `ResolvedConfig` 与 redacted view；
- 已完成安全配置快照的 no-op、冲突拒绝、并发发布、symlink 防护、临时文件 fsync 和 Unix owner-only 权限路径；
- 已完成配置示例的离线 strict load golden test。阶段 2 只验证配置与 prepare 输入，不执行资源网络首次 snapshot，也不接线 Runtime/App 启动闭环；
- 阶段 2 记录起点为 69 tests；当前后端全量测试为 135 passed、0 failed；`cargo clippy --manifest-path backend/Cargo.toml --locked --all-targets -- -D warnings` 和 `cargo fmt --manifest-path backend/Cargo.toml --all -- --check` 均通过。配置示例的 strict load 仍不访问远程资源，也不执行资源首次 snapshot。

### 阶段 3：Runtime 与启动闭环

涉及：Runtime、Application、Ports。

- 实现 `PreparedRuntime`、`RuntimeSnapshot`、`ActiveRuntime`、`BindPlan`；
- 实现 task supervisor、故障等级和优雅停机；
- 候选 prepare/bind 失败不得发布半成品；
- 验收：原子切换、失败保留旧 runtime、shutdown deadline 测试通过。

首个小阶段（已完成）：新增 `RuntimeSnapshot`、`PreparedRuntime` 和无 socket preflight；只消费 `Arc<ResolvedConfig>`，校验 revision、bind plan 端点和重复项，不绑定 listener。

第二个小阶段（已完成）：新增 `BoundCandidate`/`BoundListenerSet` 和 `bind_prepared`；`SocketSpec` 显式传递 IPv6 `v6_only`，先准备全部 socket，再统一激活，准备或激活任一步失败都回滚本轮对象。

第三个小阶段（已完成）：引入 `ArcSwap` `RuntimeCoordinator`，实现 ActiveRuntime 的原子激活、revision CAS、旧实例 draining 和请求 guard/lease；CAS 失败会返还候选供调用方重试。Runtime targeted tests：13 passed；真实 Tokio/socket2 adapter、supervisor 和 Application 启动接线留在阶段 3 后续小阶段。

第四个小阶段（已完成）：新增 `Supervisor`、task ID/故障等级/重启策略元数据和 shutdown 回收报告；所有 task 由 `JoinSet` 持有，重复注册被拒绝，正常退出、失败、取消和 panic 均有明确分类。Runtime targeted tests：3 passed；有界重启、完整 drain/flush 顺序和 Application 启动接线留在阶段 3 后续小阶段。

第五个小阶段（已完成）：扩展 `ports::effects` 的 UDP/TCP 不透明 socket capability，接入 `socket2`/Tokio `SystemSocketFactory`，并由 `BoundListenerSet::endpoint_handles` 以 `Arc` clone 方式交给后续 Transport；I/O 保留 deadline、cancellation 和安全错误分类，公共 API 不泄漏 Tokio 类型。新增 UDP/TCP activation tests；Transport framing、Application 接线和完整 shutdown 顺序留在阶段 3/4 后续小阶段。

第六个小阶段（已完成）：Application 接入严格 CLI 解析、默认 `config.yaml`、`validate` 只读命令和配置错误/启动错误映射；`validate` 不创建配置快照、不读取 SecretRef 实际值，`run` 完成 Config → Runtime preflight → bind → service 装配，并等待 Ctrl-C 后执行有界 shutdown。

第七个小阶段（已完成）：为 UDP/TCP service 接入真实 `InboundAdapter`、固定 Core 和 response encoder；最小配置可在非特权端口启动并返回内联 hosts 答案。`DnsService::shutdown` 先调用 `ActiveRuntime::begin_drain`，再取消 supervisor task。

第八个小阶段（已完成）：TCP adapter 拆分 listener/session，连接 task 由 listener 内部 `JoinSet` 持有；同一连接按读取顺序处理连续 frame，clean EOF、半帧和 admission 拒绝都限制在连接级，不终止其他连接。DoH endpoint 额外保留 `BindTransport::Doh`，在 HTTP adapter 完成前由 service 装配显式拒绝。

### 阶段 4：DNS Core 与 UDP/TCP

涉及：DNS Core、Transport、Policy 的最小默认策略。

第一个小阶段（已完成）：新增共享 `transport::wire` codec，固定原始 DNS ID 与 canonical query/response 分离，decode/encode 的 65,535 字节绝对上限和安全错误分类；响应编码只在副本上恢复请求 ID，不修改 canonical response。新增 wire codec 单测。

第二个小阶段（已完成）：新增固定 `SERVFAIL` Core、`dispatch_inbound` 和内联 hosts 解析器，并由 `ConfiguredDnsCore` 按已解析配置选择 hosts 或安全 fallback。Core 不读取 transport envelope，响应关联仍由 `ResponseHandle` exactly-once 管理。

第三个小阶段（已完成）：接入 UDP datagram adapter 和 TCP 两字节 length framing，完成 request context、原始 DNS ID 恢复、peer identity、deadline/cancellation 传递和 response encoder。UDP 响应使用 RR-boundary truncation 并在需要时设置 `TC`。

第四个小阶段（已完成）：补齐 TCP exact-read 的 clean EOF/partial EOF 分类、持久 session、连续 frame 的 `ConnectionId`/`StreamId` 语义和连接级顺序响应；新增同连接双 frame、半帧和 clean EOF 测试。

- 打通 UDP/TCP framing → canonical request → core → response encoder；
- 完成 DNS ID、EDNS、截断、deadline 和错误响应语义；
- 验收：UDP/TCP 一致性、并发、畸形报文和取消测试通过。

当前边界：DoH plain HTTP GET/POST 与基本 HTTP/DNS 错误分层已实现；TLS terminate、PROXY protocol、forwarded header 和完整跨 transport 错误验收尚未实现。DoH endpoint 不会退化为 raw DNS/TCP。

阶段 3/4 当前验收证据：

- `cargo fmt --manifest-path backend/Cargo.toml --all -- --check`：通过；
- `cargo check --manifest-path backend/Cargo.toml --locked`：通过；
- `cargo test --manifest-path backend/Cargo.toml --locked`：149 passed，0 failed；
- `cargo clippy --manifest-path backend/Cargo.toml --locked --all-targets -- -D warnings`：通过；
- 本机 smoke：临时配置在 UDP `127.0.0.1:8353`、TCP `127.0.0.1:8354` 启动成功，内联 hosts 查询返回 `127.0.0.1`；同一 TCP 连接连续发送 DNS ID `0x1111`/`0x2222`，按序收到对应响应；发送 `SIGINT` 后 `service_shutdown` 日志出现且进程以 0 退出。
- 本机 DoH smoke：临时 plain HTTP 配置在 `127.0.0.1:8355` 启动成功，直接 HTTP POST/GET 均返回 `200`、DNS ID `0x1234`、RCODE `0`；发送 `SIGINT` 后 `service_shutdown` 日志出现且进程以 0 退出。未测试 nginx、TLS 证书或特权端口。

### 阶段 5：上游解析

涉及：Upstream、Ports。

- 实现内联 hosts、单 DoH connector、bootstrap、connect_ip；
- 实现 `parallel`、`round-robin`、`load-balance`、`failover` 和 fallback；
- 验收：Host/SNI、HTTP/DNS 错误分层、超时与确定性选择测试通过。

首个小阶段（已完成）：修正 `ConfiguredDnsCore` 的 hosts 所有权，只加载顶层本地 hosts；新增内联 hosts `DnsExchange`、JSON/hosts 格式边界、DNS positive/NODATA/NXDOMAIN、取消/超时 outcome 和 typed `UpstreamRegistry`。Registry 首轮构造 hosts connector，对尚未接入的 DoH/Group 在构建边界显式返回 `UnsupportedUpstream`。

第二个小阶段（已完成）：新增无网络副作用的 `GroupSelector`，固定 failover/parallel 配置顺序、smooth weighted round-robin、weighted least-in-flight、平局轮转和 `SelectionLease` 生命周期；该阶段尚未接入真实 exchange、fallback aggregator 或 DNS Core。

第三个小阶段（已完成）：新增按 attempt index 聚合的 outcome/fallback 判定，固定 terminal response、retryable transport failure、SERVFAIL/TC、取消优先级和 fallback connector 去重；定向 outcome 测试 7 项通过。真实网络 exchange、bootstrap/connect_ip 和 DNS Core 接线仍未实现。

第四个小阶段（已完成）：新增可注入 `DohHttpTransport` 与 `DohExchange`，固定 DoH POST、Host/SNI/connect_ip、内部 DNS ID、deadline/cancellation 和稳定错误映射；`upstream::doh` 定向测试 7 项通过。真实 HTTP/TLS/socket adapter、bootstrap/connect_ip、SOCKS5/SOCKS5H、fallback 执行和 group 与策略/Core 的跨模块接线仍未实现。

第五个小阶段（已完成）：新增 `TokioDohHttpTransport` plain HTTP/1.1 adapter，固定 Host/path、Content-Length、bounded header/body、deadline/cancellation 和 chunked 拒绝；`upstream::http::tests` 3 项通过。HTTPS/TLS、proxy、连接池、bootstrap、fallback 执行和 group 与策略/Core 的跨模块接线仍未实现。

第六个小阶段（已完成）：将 plain HTTP DoH connector 接入 `UpstreamRegistry`，默认使用 `TokioDohHttpTransport`，并提供可注入 transport 构造入口；registry 在构造边界拒绝 HTTPS、bootstrap、proxy 和启用的 ECS 覆盖，允许归一化后的 `EcsMode::Disabled`，新增 `upstream::registry` 4 项定向测试。该阶段只完成 connector registry wiring，不改变 `PolicyDnsCore` 的装配路径。

第七个小阶段（已完成）：将 `PolicyDnsCore::UpstreamRuntime` 的 direct connector 构造统一切换到 `UpstreamRegistry`，使 hosts/plain HTTP DoH 使用同一 typed registry 边界；新增 3 项 Policy focused tests，验证 ConfigLoader 生成的 disabled ECS、plain HTTP DoH 注册和 unsupported feature 错误传播。该阶段不实现 Cache、bootstrap、fallback 或 Runtime snapshot 接线。

第八个小阶段（已完成）：增加 protocol-neutral 的 `PolicyDnsCore::from_config_with_registry`，通过 fake DoH transport 验证策略选择、DoH request envelope、connect_ip、内部 DNS ID 和响应转换；新增 1 项 Policy focused test，整个请求路径不访问真实网络。该阶段不实现 RuntimeSnapshot、Cache、真实 outbound 或 bootstrap。

第九个小阶段（已完成）：从 `TokioDohHttpTransport` 抽出可注入的 `DohAddressResolver` port，默认实现仍使用 Tokio `lookup_host`；新增 resolver 注入和显式 `connect_ip` 旁路测试，验证地址解析不被写死在 HTTP adapter 内。该阶段不实现 bootstrap 查询、HTTPS/TLS、SOCKS5/SOCKS5H 或真实 Runtime 接线。

第十个小阶段（已完成）：在 `DohHttpRequest`/`DohExchange` 中透传可选 bootstrap 引用，默认 system resolver 对未配置 bootstrap adapter 的请求明确 fail-closed；新增未接入 bootstrap 时不偷偷回退 system resolver 的测试。该阶段不实现 bootstrap 查询、HTTPS/TLS、SOCKS5/SOCKS5H 或真实 Runtime 接线。

第十一个小阶段（已完成）：新增 `bootstrap_answer_from_response`，从已校验 `CanonicalResponse` 提取 question owner 匹配的 A/AAAA 地址，并按地址记录最低 TTL 建立 `BootstrapAnswer`；补充正向响应提取和非正向响应拒绝测试。该阶段不实现 bootstrap 查询 I/O、HTTPS/TLS、SOCKS5/SOCKS5H 或真实 Runtime 接线。

当前阶段 5 边界：HTTPS/TLS DoH adapter、bootstrap resolver/outbound 的实际执行、SOCKS5/SOCKS5H、真实 fallback/重试链路、Cache 接线和 Runtime snapshot 接线仍未实现。

### 阶段 6：缓存

涉及：Cache、DNS Core、Storage 的独立缓存 adapter。

- 实现 namespace、key、TTL、single-flight、optimistic refresh 和 CAS；
- 接入 Moka 与独立 SQLite cache store；
- 验收：缓存准入、质量替换、恢复降级和资源变化不全局失效测试通过。

首个小阶段（已完成）：新增无外部依赖的内存 `CacheStore` adapter，覆盖 fresh/stale/expiry lookup、质量感知 CAS、显式失效、single-flight leader/follower、独立 waiter cancellation、leader abandon/drop 和 shutdown 生命周期；定向测试 7 项通过。当前 adapter 使用 `HashMap + Mutex`，后续容量淘汰仍独立于 persistence。

第二个小阶段（已完成）：新增纯逻辑响应准入 helper，按 canonical response class 计算 cache quality、origin/failure/negative TTL、optimistic stale 窗口和稳定 checksum，明确拒绝 REFUSED、未知类、缺失 TTL 和零 TTL；定向测试 4 项通过。该 helper 尚未接入 DNS Core 的请求管线。

第三个小阶段（已完成）：新增稳定 `CacheKey` 编码与 `CacheFacade` 首轮编排，固定 namespace、canonical query、strategy/client/ECS 维度、transport compatibility 和 format version；Facade 将 disabled/miss/fresh/stale/store-unavailable 分层，并以一次性 refresh permit 与 typed write request 暴露 adapter 边界；定向 key/facade 测试通过。Moka 容量淘汰、optimistic 后台刷新和 SQLite persistence 仍未实现。

第四个小阶段（已完成）：为内存 adapter 增加共享 weight 上限、确定性 oldest eviction、oversized entry 拒绝和 eviction 计数；定向测试 3 项通过。Moka adapter、optimistic 后台刷新和 SQLite persistence 仍未实现。

第五个小阶段（已完成）：新增无外部依赖的 `FilePersistentCacheStore`，固定 `FDCP` 版本化快照格式、canonical wire/checksum/response class 校验、wall-clock expiry 恢复、record 级损坏隔离、文件预算和 oldest eviction；`cache::persistence::tests` 6 项通过。Moka/SQLite persistence、last-access bucket、WAL/SHM 观测和 degraded recovery 仍未实现。

### 阶段 7：完整策略与资源

涉及：Policy、Resource、DNS Core。

- 实现 client、strategy、hosts、rule_set 编译索引；
- 完成本地/远程资源首次快照、每资源 revision 和原子发布；
- 验收：匹配优先级、资源解析、首次失败和乱序刷新测试通过。

首个小阶段（已完成）：新增 `ClientIndex` 和 `StrategyIndex`，覆盖 exact ID 优先、IPv4/IPv6 longest-prefix、unknown、重复匹配拒绝和 immutable strategy lookup；定向测试 5 项通过。

第二个小阶段（已完成）：新增 listener/stream 与 DoH route 编译索引，固定 route template 校验、`{client_id}` segment 提取、基础 strategy 引用和 listener hosts 元数据；定向 route 测试通过。规则资源 matcher 尚未接入。

第三个小阶段（已完成）：新增 `PolicyIndex::evaluate` 与不可变 `ResolutionPlan` 首轮组合，覆盖 client strategy override、cache tri-state、client digest namespace、strategy/global fallback、TTL/ECS effective value 和 upstream target；定向 plan 测试 3 项通过。后续第四至第七个小阶段已补齐 rule/hosts/resource matcher、loader 和 DNS Core/Policy 首轮接线；Runtime snapshot 原子发布仍未实现。

第四个小阶段（已完成）：实现 hosts/rule 资源 parser 与 immutable matcher，支持 A/AAAA/CNAME、wildcard、JSON/hosts/Clash、exact/suffix/regex 优先级、输入和 program size 限制；定向 Resource 测试通过。

第五个小阶段（已完成）：实现 const/file hosts 与 rule-set loader、稳定文件 fingerprint、UTF-8/大小/symlink/稳定读取边界，以及 `ResourceSnapshot`/registry 的版本 CAS 发布；资源 loader、snapshot 定向测试通过。remote fetch、manifest/content 原子落盘与恢复校验已补齐，scheduler/coordinator 的 Runtime-facing 纯逻辑编排也已完成，真实 fetch/parse/persist worker 仍未接入 Runtime。

第八个小阶段（已完成）：新增 `ResourceRefreshRuntime`，将 per-resource schedule、single-flight reservation、CAS publish、failure backoff、cancel 和 shutdown 组合为 Runtime-facing facade；`resource::orchestrator::tests` 4 项通过。真实 Runtime supervisor、资源 I/O worker 和跨 Runtime snapshot 发布仍未实现。

第六个小阶段（已完成）：`ConfiguredDnsCore` 接入 Resource hosts index，支持 const/file、JSON、CNAME、wildcard 和 exact/NODATA/NXDOMAIN 语义；DNS Core focused tests 14 项通过。

第七个小阶段（已完成）：`PolicyIndex` 接入 const/file hosts/rule-set loader，按 listener hosts → strategy rule 顺序生成安全的 `ResolutionPlan` matched-rule 摘要，并对 remote/dat/selector/缺失资源返回显式错误；Policy focused tests 6 项通过。Runtime snapshot 原子接线和完整 upstream/cache 管线仍未实现。

### 阶段 8：DoH 接入与代理安全边界

涉及：Transport、Upstream、Config。

- 实现 DoH GET/POST、TLS terminate/external、forwarded header；
- 实现 PROXY v1/v2、SOCKS5/SOCKS5H 和 SecretRef 防泄漏；
- 验收：协议边界、可信代理、Host/SNI 和大消息限制测试通过。

首个小阶段（已完成）：为 DoH bind plan 增加 typed endpoint binding，补充 opaque TCP byte-stream capability，实现 plain HTTP/1.x GET/POST codec、无填充 base64url、路由 `{client_id}` 匹配、固定 HTTP 错误状态和 DNS `application/dns-message` 响应；service 以受监督 listener/session task 接入。当前只接受 `tls.mode: external` 与 `client_ip.source: peer`，`terminate`、`forwarded_header`、`proxy_protocol` 会在装配阶段明确拒绝。定向 codec/session 测试 9 项，真实 smoke 使用 `127.0.0.1:8355` 直接 HTTP POST/GET。

当前边界：HTTP/1.x 仍按读取顺序处理，未实现 TLS terminate/external 握手、PROXY v1/v2、forwarded header 信任链、HTTP/2、上游 DoH HTTPS/TLS adapter 和完整资源/故障注入验收。

### 阶段 9：统计、详情日志与观测

涉及：Storage、Observability、DNS Core。

- 完成 SQLite migration、daily stats、batch ledger 和独立详情 writer；
- 完成 degraded 状态、persistence gap、低基数 metrics 和脱敏日志；
- 验收：跨午夜、幂等重试、队列溢出、数据库 busy/磁盘故障测试通过。

当前已完成的纯领域小阶段：Storage 建立 UTC day、sharded stats accumulator、epoch snapshot、persistence gap、batch ledger、业务 migration schema 与可替换 stats writer contract；Observability 建立有界 metric registry、原子 counter/gauge、health 状态、retry/gap 计数和 typed event 脱敏。真实 SQLite pool/migration 执行、详情 writer、最终 tracing writer、flush/backpressure 和故障注入仍未实现。

### 阶段 10：刷新、故障注入和 v1 验收

涉及：全部模块。

- 完成资源定时刷新、退避、stale 状态和并发 CAS；
- 完成 listener、数据库、缓存、资源和 telemetry/log sink 故障注入；
- 完成协议 conformance、压力测试和长期运行检查；
- 同步 README、配置示例、模块进度和最终验收证据。

## 6. 阶段提交规则

本计划中的每个编号阶段都必须形成一次独立的本地提交。若某个阶段继续拆分为多个可独立实现和验收的小阶段，则每个小阶段也必须各自提交一次，不能等到整个大阶段结束后合并成一个无法审计的提交。

每个小阶段按以下顺序闭环：

1. 只实现该小阶段约定的范围，不提前混入下一阶段；
2. 执行与风险相称的测试、lint、配置或文档检查；
3. 更新本文对应模块的状态、进度和实际验证证据；
4. 使用显式路径暂存，审核 staged 文件和 `git diff --cached --check`；
5. 创建一个 scope 与该小阶段一致的 Conventional Commit；
6. 提交后确认工作树只剩其他尚未提交的阶段内容。

提交约束：

- 一个提交只对应一个可说明、可验证、可回滚的小阶段；
- 不使用 `git add .` 混入无关文件；
- 模块实现与直接相关的测试、配置和进度更新放在同一提交；
- 纯文档阶段使用 `docs`，项目骨架使用 `build`，功能实现使用 `feat`，修复使用 `fix`；
- 默认只创建本地提交，不自动 push；
- 阶段未通过最低验收时不得为了更新进度而提前提交“完成”状态。

建议的阶段提交 scope：

| 阶段 | 建议 scope |
| --- | --- |
| 阶段 0 | `docs(backend)` |
| 阶段 1 | `build(backend)` 或 `feat(ports)` |
| 阶段 2 | `feat(config)` |
| 阶段 3 | `feat(runtime)` |
| 阶段 4 | `feat(dns)`、`feat(transport)` |
| 阶段 5 | `feat(upstream)` |
| 阶段 6 | `feat(cache)` |
| 阶段 7 | `feat(policy)`、`feat(resource)` |
| 阶段 8 | `feat(doh)`、`feat(outbound)` |
| 阶段 9 | `feat(storage)`、`feat(observability)` |
| 阶段 10 | `test(backend)` 或与修复内容一致的 scope |

表中的多个 scope 表示该阶段应按可独立验收的小阶段拆成多个提交，而不是在一个提交中同时使用多个 scope。

## 7. v1 验收门槛

全部满足后，后端代码实现总进度才可标记 100%：

1. 配置示例可被严格加载，所有错误包含稳定分类和字段路径。
2. UDP、TCP、DoH 对同一 canonical query 的策略结果一致。
3. 请求只捕获一次 runtime revision，资源并发更新不丢失已发布版本。
4. 所有后台任务受 supervisor 管理，无 detached task。
5. 数据库、详情日志或缓存持久化故障不阻塞 DNS 数据面；启动必需数据库失败会拒绝启动。
6. 日志和 metrics 不暴露 SecretRef、完整 client ID、原始 IP、query string 或 raw DNS wire。
7. 缓存、上游组、资源刷新和统计具备确定性并发测试。
8. `cargo fmt --manifest-path backend/Cargo.toml --all -- --check`、`cargo test --manifest-path backend/Cargo.toml --locked`、`cargo clippy --manifest-path backend/Cargo.toml --locked -- -D warnings` 和 `git diff --check` 通过。

## 8. 计划维护方式

每完成一个实现切片，应在同一次阶段提交中同步更新：

1. 本文对应模块的状态、进度和验证证据；
2. 对应模块方案中的“实现检查清单”；
3. 若契约改变，更新总体架构或配置字段参考；
4. 重新按权重计算后端代码实现总进度和 v1 交付总进度。

进度证据应记录实际命令、测试名称和结果；仅创建文件、声明类型或写 TODO 不算完成核心实现。

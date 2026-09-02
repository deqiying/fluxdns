# FluxDNS 后端开发计划

> 状态：v1 模块方案已完成，阶段 1、阶段 2 已完成；阶段 3 基础服务编排、阶段 4 UDP/TCP 基础链路、阶段 5 upstream 首轮小阶段、阶段 6 cache 首轮切片、阶段 7 resource/policy/DNS Core 首轮接线、阶段 8 DoH plain HTTP 首轮接入、阶段 9 统计/观测纯领域切片和阶段 10 resource refresh、coordinator ownership、supervisor restart、candidate activation、stale refresh guard、Application reload trigger、service fault escalation、scoped cancellation 及 listener rebind 首轮切片已实现
>
> 更新日期：2026-09-02
>
> 总体架构：[backend-architecture.md](backend-architecture.md)
>
> 配置契约：[configuration-reference.md](configuration-reference.md)

## 1. 当前进度结论

仓库已固定 `backend/` 与 `frontend/` 两个独立代码主目录；根目录不作为任一端的工程目录。`backend/` 已具备单 binary crate、核心契约、Config 配置系统、Runtime 候选骨架和基础服务启动闭环；阶段 2 记录起点为 69 个单元测试，当前全量测试为 411 个。Config 已完成自身的严格加载、v1 空迁移 registry、路径/SecretRef source normalization、semantic validation、reference graph、bind plan、安全快照和不可变 `ResolvedConfig`；Runtime 已完成 `RuntimeSnapshot`、`PreparedRuntime`、无 socket preflight、基于 `SocketFactory` 的 BindPlan 全成/全退、`ArcSwap` ActiveRuntime coordinator/CAS、请求 guard、Supervisor task tree 基础、可重建 task 的有界 transient restart/backoff、候选 bind/CAS 激活入口、stale-active refresh guard、系统 socket capability、Application CLI/校验接线和服务任务编排，并已让 snapshot 通过 `ArcSwap` 持有按配置生成的资源元数据摘要、让 service 从 active snapshot 自动取得同 revision 的 `DnsCore`，并由 `PreparedRuntime`/`ActiveRuntime` 持有生产 `ResourceFetcher`；正式 async prepare 已在 bind 前恢复或下载 remote rule-set、加载 file hosts/rule-set 初始 snapshot，并为 `auto_update=true` 的三类可刷新资源注册同一个 service Supervisor 下的长期 worker，成功候选在同一 ActiveRuntime 内完成 Policy live publish 和 Runtime 元数据原子更新，失败进入 backoff，取消和 shutdown 释放 schedule reservation；`Application` 与 `DnsService` 现已共享持有 `RuntimeCoordinator`，资源刷新 task 每轮通过 coordinator 查询当前活动实例，Application 已提供无 snapshot 副作用的配置文件 reload 触发 API和 service-aware reload 入口，`DnsService` 已观察 Supervisor 终止 task 并按 fault level 升级不可恢复故障，Supervisor 已提供受管 task-scoped cancellation，DnsService 的 UDP/TCP/DoH listener task 已使用独立 scoped token，显式 reload 可为新 revision 重建 listener task 并取消旧 token；外部配置变更事件、资源 worker 集合重建和完整跨 Runtime 配置候选发布尚未完成；Transport/DNS Core 已完成共享 wire boundary、固定 SERVFAIL/hosts core、UDP/TCP adapter、UDP 截断、TCP 持久 session 和 DoH plain HTTP adapter/service 首轮链路，并已将 const/file hosts 资源接入本地 Core；Upstream 已完成内联 hosts exchange、可注入 DoH exchange、plain HTTP DoH transport、Reqwest Rustls HTTP/2 direct/proxy HTTPS DoH adapter、adapter-owned bounded client pool、可注入地址解析 port、bootstrap 引用元数据透传、bootstrap 响应地址提取、注入 connector 的 bootstrap A/AAAA 查询、默认 DoH transport/Registry bootstrap 接线、Outbound profile/target 规划、SOCKS5/SOCKS5H protocol codec、协议无关 outbound stream port 与握手认证编排、Tokio TCP dial adapter、profile credential 装配、proxy hostname resolver、最小 SOCKS connector 闭环、standalone plain HTTP SOCKS5/SOCKS5H DoH transport adapter、配置驱动的 proxy Registry/Policy/Runtime prepare 接线、hosts/plain HTTP DoH registry、PolicyCore direct request path、direct group primary/fallback exchange 与 group timeout、group member selection、parallel late window、nested group 和结果聚合/fallback 判定，以及 Reqwest/Rustls loopback live TLS handshake 验证；parallel 快速完整 Positive 路径已接入协议中立的 typed late-result sink，并将合法 late response 交给有界 `LateCacheFinalizer`，但 nested late propagation、完整 late-window 候选语义、共享 Runtime finalizer owner 和完整 resource/service lifecycle 仍未完成。Cache 已完成无外部依赖的内存 `CacheStore`、容量淘汰、响应准入/TTL、稳定 key builder、`CacheFacade`、single-flight、可取消有界 `LateCacheFinalizer`、基础 Cache/Core fresh/miss/single-flight/CAS 接线、当前 PolicyDnsCore snapshot-local optimistic refresh 和版本化文件快照 persistence 边界；Policy 已完成 client/strategy/route immutable index、const/file hosts/rule-set loader 接线、direct hosts/plain HTTP DoH registry wiring、配置驱动 proxy DoH 与 group fallback request path、请求级资源规则匹配和安全的 matched-rule 摘要，以及 Policy compiled resource snapshot 的版本化 atomic live swap，并可在 supplied compiled remote/file snapshot 上构造初始 Policy；Resource 已完成 hosts/rule parser、受限 regex、const/file loader、资源 snapshot/CAS、远程 manifest/content 原子落盘和恢复校验、scheduler/coordinator 的 Runtime-facing 纯逻辑编排、一次性 remote refresh worker、file hosts/rule-set refresh worker、生产 ReqwestResourceFetcher、async PreparedRuntime 首次 remote restore/fetch、file snapshot load 和 service Supervisor 长期 refresh task；Storage 已完成纯内存统计 epoch/batch ledger、业务 migration schema 和可替换 stats writer contract；Observability 已完成有界 metrics/health registry。DoH 入站 TLS/PROXY/forwarded、bootstrap/连接执行的完整接线、Moka/SQLite persistence、完整跨 Runtime resource worker 生命周期、基于最新 Runtime snapshot 的完整 optimistic refresh、共享 Runtime finalizer owner、真实 SQLite/detail/telemetry writer 和完整服务级故障验收仍未实现。

| 口径 | 当前值 | 说明 |
| --- | ---: | --- |
| 模块方案覆盖率 | 100% | 本计划覆盖 12 个后端顶层模块，每个模块均有独立方案文档 |
| 后端代码实现进度 | **62.8%** | Config 达到 100% 模块验收口径；Runtime 已增加原子资源元数据发布、三类 file/remote refresh worker、Application/DnsService 共享 coordinator 的资源刷新入口、可重建 task 的有界 transient restart/backoff、候选 bind/CAS 激活入口和 stale-active refresh guard；Application 已增加无 snapshot 副作用的配置文件 reload API 和 service-aware reload 入口，DnsService 可为新 revision 重建 transport listener 与 resource refresh task 并取消旧 scoped token；DnsService 已观察 Supervisor task completion 并按 fault level 升级不可恢复故障；Policy/Resource 已补齐 supplied compiled file/remote snapshot、失败 backoff、取消释放和 Runtime live publish；仍缺少 `run` 外部配置变更事件接入、完整跨 Runtime 配置候选发布、共享 Runtime finalizer owner、基于最新 Runtime snapshot 的完整 optimistic refresh、入站 DoH TLS/PROXY/forwarded、Moka/SQLite persistence、真实 SQLite/detail/telemetry writer 和完整服务级故障验收 |
| v1 交付总进度 | **66.5%** | 设计阶段 10% 已完成，加上实现与验收部分的 `90% × 62.8%` |

截至 2026-09-02，阶段 45 已将显式 service reload 的 listener 与 resource refresh task 集合按新 Runtime revision 重建；当前全量测试为 406 个，`run` 自动配置变更事件、完整跨 Runtime 候选发布和 flush 生命周期仍未完成。

截至 2026-09-02，阶段 46 已修正 Supervisor 对 JoinSet panic/abort 结果的 task ID 归因，避免按注册表顺序误删 sibling task；当前全量测试为 407 个，`run` 自动配置变更事件、完整跨 Runtime 候选发布和 flush 生命周期仍未完成。

截至 2026-09-02，阶段 47 已将 UDP/TCP/DoH transport task 接入带 scoped cancellation 的三次有界瞬时重试；该阶段只复用已绑定 socket，不实现自动 rebind，当前全量测试为 409 个，`run` 自动配置变更事件、完整跨 Runtime 候选发布和 flush 生命周期仍未完成。

截至 2026-09-02，阶段 48 已让 Supervisor 的 JoinSet 异常结果保留原始 `TaskSpec` component/fault level；当前全量测试仍为 409 个，`run` 自动配置变更事件、完整跨 Runtime 候选发布和 flush 生命周期仍未完成。

截至 2026-09-02，阶段 49 已修复配置快照并发测试的临时目录命名碰撞；当前全量测试为 409 个，`run` 自动配置变更事件、完整跨 Runtime 候选发布和 flush 生命周期仍未完成。

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
| Runtime | `backend/src/runtime/*` | [runtime.md](backend-modules/runtime.md) | 已完成 | 实现中 | 60% | 12% |
| Transport | `backend/src/transport/*` | [transport.md](backend-modules/transport.md) | 已完成 | 实现中 | 50% | 11% |
| DNS Core | `backend/src/dns/*` | [dns-core.md](backend-modules/dns-core.md) | 已完成 | 实现中 | 55% | 10% |
| Policy | `backend/src/policy/*` | [policy.md](backend-modules/policy.md) | 已完成 | 实现中 | 70% | 8% |
| Upstream | `backend/src/upstream/*` | [upstream.md](backend-modules/upstream.md) | 已完成 | 实现中 | 99% | 10% |
| Cache | `backend/src/cache/*` | [cache.md](backend-modules/cache.md) | 已完成 | 实现中 | 50% | 9% |
| Resource | `backend/src/resource/*` | [resource.md](backend-modules/resource.md) | 已完成 | 实现中 | 90% | 7% |
| Storage | `backend/src/storage/*`、`backend/migrations/*` | [storage.md](backend-modules/storage.md) | 已完成 | 实现中 | 35% | 8% |
| Observability | `backend/src/observability.rs` | [observability.md](backend-modules/observability.md) | 已完成 | 实现中 | 30% | 3% |

后端代码实现总进度：

```text
4% × 45% + 8% × 35% + 10% × 100% + 12% × 60% + 11% × 50% + 10% × 55% + 8% × 70% + 10% × 99% + 9% × 50% + 7% × 90% + 8% × 35% + 3% × 30% ≈ 62.8%
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

### 后续开发路线（基于 2026-09-02 当前状态）

当前后端代码实现进度为 62.8%，v1 交付进度为 66.5%。Config、Upstream、Resource 的基础链路和可信测试基线已经建立，剩余风险主要集中在 Runtime 生命周期、跨模块状态一致性、持久化故障语义和最终验收。因此后续不按模块百分比从低到高补齐，而采用“先运行时生命周期、再数据与观测、最后协议和 v1 验收”的垂直切片。

| 顺序 | 目标 | 主要范围 | 退出条件 |
| --- | --- | --- | --- |
| 0 | 开发环境与验证门 | 直接调用项目 Rust 1.98.0；执行 `cargo fmt --manifest-path backend/Cargo.toml --all -- --check`、`cargo check --manifest-path backend/Cargo.toml --locked`、`cargo clippy --manifest-path backend/Cargo.toml --locked --all-targets -- -D warnings` 和 `cargo test --manifest-path backend/Cargo.toml --locked` | 版本来源可追溯，基线保持 `411 passed、0 failed`，后续阶段不得用 `--ignore-rust-version` 替代正式验证 |
| 1 | Runtime 候选与跨 Runtime 生命周期 | 在 `PreparedRuntime → BindPlan → ActiveRuntime` 中统一组合 Config、Policy、Resource registry、Runtime metadata 和 worker 集合；完成候选 registry 的跨 Runtime 合并与 revision CAS | 候选 prepare/bind/CAS 失败均保留旧 Runtime；并发资源更新不丢失；同一请求只使用一个 Runtime revision；无 detached task |
| 2 | 配置变更与自动 rebind | 为 `run` 接入已确定的配置变更事件源和去抖/失败语义；区分 resource-only、listener rebind 与必须重启的变更；重建 listener/resource task 集合 | 新 revision 的 listener 全部 bind 成功后才切换；失败保留旧端点；旧 task 可取消并完成 drain；显式 reload 与自动 reload 复用同一入口 |
| 3 | DNS Core/Cache 一致性 | 让 optimistic refresh 捕获最新 Runtime snapshot；完成共享 `LateCacheFinalizer` owner、完整 late-window/nested sink 传播、Policy/Core/Cache/Upstream 跨 transport contract tests | stale refresh、取消、CAS、旧 Runtime drain 和 shutdown 行为确定；资源更新只影响后续请求，不破坏已开始请求 |
| 4 | Storage 与 Observability 生产切片 | 先接 SQLx pool、migration、health probe、stats checkpoint/ledger 和 resolve-log writer；再接正式 telemetry subscriber、health/event writer、flush/backpressure，并纳入 Supervisor | stats 幂等重试、detail 有界丢弃、DB busy/磁盘故障降级、脱敏日志和 deadline-aware flush 均有测试与分项报告 |
| 5 | Cache 持久化 | 先以现有 `CacheStore`/`PersistentCacheStore` port 接入 Moka adapter，再实现独立 SQLite cache persistence；不复用业务 Storage 数据库 | Moka weight/expiry、文件/SQLite 恢复校验、容量预算、corrupt/busy/disk-full 降级和 shutdown 持久化通过 adapter contract tests |
| 6 | DoH 入站安全边界 | 实现 TLS terminate、证书/私钥校验、握手 timeout/cancellation、PROXY v1/v2、trusted proxy/forwarded header 信任链，以及计划要求的 HTTP/2 边界 | 非法或不可信输入 fail-closed；Host/SNI、客户端地址恢复、GET/POST、HTTP/DNS 错误分类和 rebind 行为有端到端证据 |
| 7 | v1 最终验收 | 补齐 listener、resource、cache、storage、telemetry 故障注入；执行 UDP/TCP/DoH conformance、压力/长期运行、完整 drain/flush/shutdown 和文档回链 | 所有 v1 验收门槛有可复现记录，进度按实际代码与验证证据更新，不以文档完成替代实现 |

实施约束：

- 第 1～3 步是主线，优先完成后再扩大持久化和协议范围；不要单独继续打磨 Upstream 的剩余 1%，其 late-window 和 finalizer 工作应并入 Runtime/Cache 生命周期切片。
- 第 4、5 步共享故障、flush 和 shutdown 边界，但业务 SQLite 与 cache SQLite 必须保持独立文件、schema、writer 和健康状态。
- 第 6 步不引入 WebUI、管理 API、主动健康检查或其他明确排除在 v1 之外的能力；配置热加载只复用已定义的内部 reload 入口。
- 每个小阶段都必须同时更新直接受影响的模块文档、测试证据和本计划，并完成最小充分验证后再标记完成。

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

第九个小阶段（已完成）：`RuntimeSnapshot` 持有按 `ResolvedConfig` 生成的不可变 `ResourceRegistrySnapshot<()>` 元数据摘要，并在 summary 中报告资源数量；`DnsService` 新增从 `ActiveRuntime` snapshot 自动取得同 revision `DnsCore` 的构造入口，Application 不再单独从 snapshot 外传递 core。该阶段只建立 Runtime-facing immutable handle 边界，资源真实 fetch/parse worker、资源级原子 reload、共享 listener 的 resource-only swap、cache/telemetry flush 仍留在后续阶段。

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

第十二个小阶段（已完成）：新增 `BootstrapResolver`，通过调用方注入的 `DnsExchange` 顺序执行 A/AAAA 查询，合并合法地址并取最低 TTL；对无地址、transport failure、取消和非法 host 返回稳定错误，补充 hosts connector、fake failure、取消和非法 host 测试。该阶段不把 resolver 接入 DoH address resolver、Registry、outbound 或 Runtime。

第十三个小阶段（已完成）：为默认 `UpstreamRegistry::from_resolved` 创建共享 bootstrap connector registry，将 hosts/DoH connector 登记后交给 `TokioDohAddressResolver`；DoH address resolver 按 bootstrap 引用执行 A/AAAA 查询、转换请求端口并完成 plain HTTP loopback path。补充 resolver registry 和真实 Registry→DoH→hosts bootstrap 测试。自定义 transport 的 resolver 注入、HTTPS/TLS、outbound 和 Runtime 接线仍延后。

第十四个小阶段（已完成）：新增 `OutboundProfile`/`OutboundTarget`，在显式边界解析 SecretRef 代理 URL，固化 socks5/socks5h scheme、代理端点、credential 脱敏和本地/远程 hostname resolution 规划；对非法 host/port 及 socks5h 与 bootstrap 组合返回稳定错误，补充 4 项 focused tests。该阶段不实现 SOCKS5/SOCKS5H 握手、认证、socket dial 或 Runtime 接线。

第十五个小阶段（已完成）：新增协议无关的 `socks5` codec，固定 method negotiation、username/password authentication、CONNECT 请求、IPv4/IPv6/domain 地址编码、reply 与 bound address 解析，并对截断、尾随字节、版本、保留字段、认证失败和地址类型错误返回稳定错误；`upstream::socks5::tests` 6 项通过。该阶段不建立 outbound stream、不执行实际握手/认证 I/O、socket dial 或 Runtime 接线。

第十六个小阶段（已完成）：在 `ports::effects` 增加独立的 `OutboundStream` byte-stream port，并实现受 deadline/cancellation 约束的 SOCKS5 method、username/password、CONNECT 握手编排；按 address type 读取固定或可变长度 response，区分 credentials 缺失、proxy reply、clean EOF 和底层 port 错误；新增 3 项异步 focused tests。该阶段不提供系统 socket dial、连接池、HTTPS/TLS 或 Runtime 接线。

第十七个小阶段（已完成）：增加 `OutboundDialer` port 和 `TokioOutboundDialer`，以 bounded read/write、deadline/cancellation 和脱敏 `PortError` 包装 Tokio TCP stream；`Socks5Connector` 在调用方提供已解析 proxy `SocketAddr` 后完成 dial → handshake → CONNECT 最小闭环，并以 loopback SOCKS server 验证真实 socket path。该阶段不负责 proxy hostname 解析、SecretRef credential 到 connector 的完整装配、连接池、HTTPS/TLS 或 Runtime 接线。

第十八个小阶段（已完成）：`OutboundProfile` 在 prepare 边界解析 URL userinfo，百分号解码并保存长度受限的 `OutboundCredentials`；`Socks5Connector::connect_profile` 自动把脱敏 credential material 传递给握手，拒绝缺失 password、空值、非法百分号和超过 255 字节的字段；新增 profile credential 与 loopback username/password tests。该阶段不负责 proxy hostname 解析、连接池、HTTPS/TLS 或 Runtime 接线。

第十九个小阶段（已完成）：增加 `OutboundAddressResolver` port 与 `TokioOutboundAddressResolver`，限制 proxy endpoint 的 host/port、地址数量和 deadline/cancellation；`Socks5Connector::connect_profile_with_resolver` 将 proxy hostname 解析为候选 `SocketAddr` 后复用已有 dial/handshake path，并用注入 resolver + loopback SOCKS server 验证。该阶段不实现连接池、HTTPS/TLS、DoH/Runtime proxy 接线或 bootstrap resolver 复用。

第二十个小阶段（已完成）：增加 `OutboundStream::read_chunk` byte-stream 能力和 `TokioSocks5DohHttpTransport`，将 proxy hostname resolver、target `DohAddressResolver`、`OutboundProfile`、SOCKS5/SOCKS5H handshake、HTTP/1.1 request/response bounded path 组合为 plain HTTP DoH adapter；通过 loopback SOCKS server 验证本地解析后的 IPv4 CONNECT、`socks5h` 域名 CONNECT、Host/path/body 和 Content-Length response。该阶段未接入 Registry/Runtime snapshot、连接池、HTTPS/TLS 或完整 group/fallback 执行。

第二十一个小阶段（已完成）：增加 `UpstreamRegistry::from_resolved_with_outbounds` 和配置驱动的 `ConfiguredDohTransport`，从 `ResolvedConfig.outbounds` 解析 proxy profile，按 DoH upstream 选择 direct 或 SOCKS5/SOCKS5H transport；`PolicyDnsCore::from_config` 和 `PreparedRuntime::prepare_with_policy_core` 使用同一 Registry 路径。新增 Registry 的 SOCKS5+bootstrap loopback、missing/invalid outbound 与 `socks5h + bootstrap` fail-fast 测试，PolicyCore 的配置驱动 proxy DoH loopback 交换，以及 Runtime prepare 的错误传播测试。该阶段不引入新依赖，不实现 HTTPS/TLS、连接池、group/fallback 实际执行或 Runtime live resource/service snapshot 接线。

第二十二个小阶段（已完成）：将 direct hosts/DoH group 的 primary/fallback exchange 接入 `UpstreamGroupExecutor`，消费 `timeout`/`fallback_timeout` 并为每个 phase 收紧请求 deadline；主组仅在全部尝试为 SERVFAIL/TC 或可重试 transport failure 时进入独立 fallback window。Policy prepare 现在绑定 primary/fallback direct connector，缺失成员、非法 selector 或 timeout 在 prepare 边界返回错误，不再静默跳过。新增 executor fallback/terminal/timeout 测试和 PolicyCore DoH SERVFAIL→hosts fallback loopback 测试。nested group、parallel late window、late cache finalizer、HTTPS/TLS 与连接池仍未实现。

第二十三个小阶段（已完成）：新增锁定版本的 `reqwest 0.13.4` direct dependency，显式启用 Rustls、HTTP/2 和 SOCKS features；增加 `ReqwestDohHttpTransport`，支持 direct HTTP/HTTPS、`connect_ip`/bootstrap resolver、`no_proxy`、禁用 redirect、deadline/cancellation 和 bounded response body；配置驱动 `UpstreamRegistry`/`PolicyDnsCore` 可构造 direct HTTPS DoH connector，旧的注入式 custom transport API 仍对 HTTPS 明确返回 unsupported。新增 reqwest loopback HTTP envelope、`connect_ip`、bootstrap resolver、cancellation、Registry HTTPS 构造和 Policy HTTPS prepare 测试。proxy HTTPS、连接池 key/复用、TLS live handshake 验证、nested group、parallel late window 与 late cache finalizer 仍未实现。

第二十四个小阶段（已完成）：扩展 `ReqwestDohHttpTransport` 支持配置驱动 SOCKS5/SOCKS5H proxy，代理端点通过 `OutboundAddressResolver` 解析，目标地址覆盖与 proxy endpoint、SOCKS 本地/远程解析模式共同组成 adapter-owned bounded LRU client pool key；`socks5h + connect_ip` 使用等价的本地 SOCKS5 目标模式，仍保留 URL Host/SNI，`socks5h + bootstrap` 在 Registry 边界拒绝。新增 proxy loopback exchange、pool entry 复用、HTTPS+proxy Registry 构造和 HTTPS+socks5h+bootstrap fail-fast 测试。TLS live handshake 验证、nested group、parallel late window 与 late cache finalizer 仍未实现。

第二十五个小阶段（已完成）：修正 parallel group 执行时序，使用 `JoinSet` 按完成顺序消费 attempt；完整 `Positive` response 立即作为客户端终态并取消剩余成员，非完整 `NoData`/SERVFAIL/TC 继续在 group deadline 内收集，聚合器在 parallel late window 中优先完整 `Positive`，再按配置顺序选择其他可缓存终态。新增快速终态不等待慢成员和 late window 选择测试。TLS live handshake 验证、nested group、late cache finalizer 与 Runtime live resource/service snapshot 仍未实现。

第二十六个小阶段（已完成）：`UpstreamRuntime` 改为按 upstream definition 递归构造 group，nested group member 通过 `GroupExchange` 复用统一 `DnsExchange` 边界，保留 group timeout/fallback 语义；新增 nested group 成功执行和递归 cycle guard 测试，避免绕过 Config 校验时静默跳过或无限递归。TLS live handshake 验证、group late-result sink、Runtime 级 late cache finalizer 生命周期与 Runtime live resource/service snapshot 仍未实现。

当前阶段 5 边界：Reqwest/Rustls live TLS handshake 已通过 loopback server 和验证根证书完成；parallel 快速完整 Positive 路径已接入 typed late-result sink，当前 PolicyDnsCore finalizer 可由 DnsService 在 supervisor drain 后按 deadline 关闭，但完整 late-window 候选选择、nested group 透传、共享 Runtime owner 和严格 encoder 完成 hook 仍未实现。阶段 6 已接入当前 PolicyDnsCore snapshot-local optimistic refresh 与有界 `LateCacheFinalizer`；最新 Runtime snapshot 捕获和完整 resource/service snapshot 生命周期仍未实现。

第二十七个小阶段（已完成）：为 `ReqwestDohHttpTransport` 增加仅测试使用的根证书注入入口，以 `rcgen` 生成 SAN 为 `resolver.example.test` 的测试证书，并用 `tokio-rustls` loopback server 完成真实 TLS handshake、Host/path/body 和 `application/dns-message` 响应验证；生产构造仍使用系统信任链，不接受任意证书。该阶段不实现入站 DoH TLS terminate/external、PROXY/forwarded、parallel group late-result sink 或 Runtime live resource/service snapshot。
第二十八个小阶段（已完成）：为 `UpstreamGroupExecutor` 增加协议中立的 `LateResultSink` 入口；parallel 快速完整 Positive 返回客户端终态后，剩余 attempt 在现有 group deadline 内由受控 drainer 完成，合法 response 交给 sink。`PolicyDnsCore` 将其转换为 `CacheCondition::Absent` 的有界 `LateCacheFinalizer` 写请求，既不阻塞客户端响应，也不覆盖已存在 cache entry。新增 executor sink 非阻塞测试和 Policy late cache 写入测试；该阶段不实现非 Positive 快速终态、nested group sink 透传、独立 Runtime owner 或严格 encoder 完成 hook。
第二十九个小阶段（已完成）：为 `LateCacheFinalizer` 增加 `JoinSet` 任务归属、提交/关闭互斥边界、panic-safe active 计数和 `shutdown_until(Deadline)`；`PolicyDnsCore` 暴露 crate 内 shutdown 入口，`DnsService::shutdown` 在 supervisor drain 后关闭当前 runtime snapshot 的 finalizer，并将超时并入 `ShutdownReport`。该阶段只覆盖当前 Policy core 的 service owner，不实现跨 runtime 共享后台 owner、resource/telemetry/cache flush 或 detached late drainer 的完整监督。
第三十个小阶段（已完成）：新增 `ResourceRefreshWorker`，在 due reservation 内调用 `ResourceFetcher` 和 remote rule-set bounded fetch/parse/persist，按 coordinator 分配的 epoch 重绑定候选并执行 CAS publish；取消/截止时间释放 reservation，解析失败进入 backoff。新增 worker 成功发布和失败退避测试；Runtime supervisor task、跨 Runtime snapshot 发布和 Runtime live resource swap 仍未接线。
第三十一个小阶段（已完成）：`PolicyDnsCore` 以 `ArcSwap` 持有包含 hosts/rule-set compiled index 与 per-resource version 的 immutable state，新增按资源类型的版本检查、CAS live swap 和 stale candidate 拒绝；新请求读取新 rule-set，已持有旧 core 的请求不受影响。新增 Policy live swap/stale version 测试；Runtime snapshot 原子发布、Resource worker 到 Policy/Runtime 的跨模块接线仍未实现。
第三十二个小阶段（已完成）：新增生产 `ReqwestResourceFetcher`，在 prepare 边界装配已解析的 outbound profile，固定 direct HTTP/HTTPS、SOCKS5/SOCKS5H、禁用环境代理与重定向、bounded body、safe URL、deadline/cancellation 和安全错误分类；`PreparedRuntime`/`ActiveRuntime` 持有 shared fetcher，但请求 `RuntimeSnapshot` 不包含 HTTP client。新增 7 项 fetcher focused tests，覆盖 direct HTTP、HTTPS TLS handshake、SOCKS5H proxy、body limit、非 2xx、取消、未知 proxy 和 SecretRef 脱敏；Runtime supervisor 长期调度、跨 Runtime resource publish 和 Policy/Runtime service lifecycle 仍未接线。
第三十二个小阶段（已完成）：新增生产 `ReqwestResourceFetcher`，在 prepare 边界装配已解析的 outbound profile，固定 direct HTTP/HTTPS、SOCKS5/SOCKS5H、禁用环境代理与重定向、bounded body、safe URL、deadline/cancellation 和安全错误分类；`PreparedRuntime`/`ActiveRuntime` 持有 shared fetcher，但请求 `RuntimeSnapshot` 不包含 HTTP client。新增 7 项 fetcher focused tests，覆盖 direct HTTP、HTTPS TLS handshake、SOCKS5H proxy、body limit、非 2xx、取消、未知 proxy 和 SecretRef 脱敏；Runtime supervisor 长期调度、跨 Runtime resource publish 和 Policy/Runtime service lifecycle 仍未接线。
第三十三个小阶段（已完成）：新增 async `PreparedRuntime` prepare 路径，在 bind 前为每个 remote rule-set 先校验落盘 content/manifest fallback，失败后通过生产 `ResourceFetcher` 下载、解析并原子持久化；Policy 使用本阶段得到的 compiled snapshot 构造初始 rule index，Application `run` 已切换到该路径，候选失败不会绑定 listener。新增 restore/fetch 两次启动测试；长期 timer、ResourceRefreshWorker supervisor ownership、跨 Runtime Policy live publish 和 resource-only service lifecycle 仍未接线。
第三十四个小阶段（已完成）：为每个 `auto_update=true` 的 remote rule-set 建立独立 `ResourceRefreshWorker` 和 service Supervisor task；task 按逻辑时间检查 due/backoff，调用 bounded fetch/parse/persist worker，并在成功后通过 `PolicyDnsCore::publish_rule_set_resource` 做同一 ActiveRuntime 内的版本化 live swap；取消和 shutdown 由同一 Supervisor 回收。跨 Runtime 配置发布、file/hosts 长期刷新、resource-only listener/service 生命周期和完整故障注入仍未接线。

第三十五个小阶段（已完成）：在 Resource 层新增 file hosts/rule-set refresh worker，复用稳定文件读取、content hash、parser version 和 per-resource epoch/CAS；async `PreparedRuntime` 在 bind 前加载 file snapshot，并把 supplied typed indexes 交给 Policy 初始构造；service Supervisor 统一调度 remote、file rule-set 和 file hosts worker，成功候选同时更新当前 Policy 与 RuntimeSnapshot 的原子资源摘要，失败进入 backoff，取消/停机释放 reservation。新增 file hosts/rule-set worker、Runtime metadata publish、缺失文件失败退避和 async prepare 集成测试；仍未完成真正跨 `ActiveRuntime` 配置候选发布、共享 listener set 的独立 resource-only runtime swap、完整 service-level fault matrix 和长期压力验收。

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

第六个小阶段（已完成）：新增可取消、有界的 `LateCacheFinalizer`，以 typed `CacheWriteRequest` 接收客户端响应完成后的 cache write；容量不足时拒绝提交，shutdown 会取消并等待已提交任务退出，不阻塞客户端响应。新增 finalizer 构造、异步写入和 shutdown 测试，并扩展 `submit_task` 供后台 refresh 复用同一容量边界；parallel 快速完整 Positive late sink 已在第二十八个小阶段消费该 finalizer，但 共享 Runtime shutdown owner 仍留在后续阶段；当前 snapshot 的 DnsService owner 已接入。

第七个小阶段（已完成）：`PolicyDnsCore` 为 upstream 请求构造稳定 cache key，接入 fresh lookup、miss single-flight、leader admission/CAS 写回和 follower 共享结果；缓存 store 失败时降级到上游，hosts[] 本地命中仍绕过 response cache。新增配置启用缓存后的 upstream 命中、snapshot-local optimistic stale refresh 和 fast-positive late sink 写入测试；当前 snapshot-local optimistic stale refresh 已通过有界 finalizer 和 `CacheCondition::Version` 写回接入并验证，仍缺最新 Runtime snapshot 捕获、Moka/SQLite persistence 和 Runtime live resource reload。

### 阶段 7：完整策略与资源

涉及：Policy、Resource、DNS Core。

- 实现 client、strategy、hosts、rule_set 编译索引；
- 完成本地/远程资源首次快照、每资源 revision 和原子发布；
- 验收：匹配优先级、资源解析、首次失败和乱序刷新测试通过。

首个小阶段（已完成）：新增 `ClientIndex` 和 `StrategyIndex`，覆盖 exact ID 优先、IPv4/IPv6 longest-prefix、unknown、重复匹配拒绝和 immutable strategy lookup；定向测试 5 项通过。

第二个小阶段（已完成）：新增 listener/stream 与 DoH route 编译索引，固定 route template 校验、`{client_id}` segment 提取、基础 strategy 引用和 listener hosts 元数据；定向 route 测试通过。规则资源 matcher 尚未接入。

第三个小阶段（已完成）：新增 `PolicyIndex::evaluate` 与不可变 `ResolutionPlan` 首轮组合，覆盖 client strategy override、cache tri-state、client digest namespace、strategy/global fallback、TTL/ECS effective value 和 upstream target；定向 plan 测试 3 项通过。后续第四至第七个小阶段已补齐 rule/hosts/resource matcher、loader 和 DNS Core/Policy 首轮接线；Runtime snapshot 原子发布仍未实现。

第四个小阶段（已完成）：实现 hosts/rule 资源 parser 与 immutable matcher，支持 A/AAAA/CNAME、wildcard、JSON/hosts/Clash、exact/suffix/regex 优先级、输入和 program size 限制；定向 Resource 测试通过。

第五个小阶段（已完成）：实现 const/file hosts 与 rule-set loader、稳定文件 fingerprint、UTF-8/大小/symlink/稳定读取边界，以及 `ResourceSnapshot`/registry 的版本 CAS 发布；资源 loader、snapshot 定向测试通过。remote fetch、manifest/content 原子落盘与恢复校验已补齐，scheduler/coordinator 的 Runtime-facing 纯逻辑编排也已完成，一次性 remote fetch/parse/persist worker 已接入 refresh reservation，async PreparedRuntime 首次 remote restore/fetch 已接入，真实 Runtime supervisor task 仍未接入。

第八个小阶段（已完成）：新增 `ResourceRefreshRuntime`，将 per-resource schedule、single-flight reservation、CAS publish、failure backoff、cancel 和 shutdown 组合为 Runtime-facing facade；`resource::orchestrator::tests` 4 项通过。一次性 `ResourceRefreshWorker` 已在后续小阶段补齐；真实 Runtime supervisor、资源 I/O worker 的长期调度和跨 Runtime snapshot 发布仍未实现。

第六个小阶段（已完成）：`ConfiguredDnsCore` 接入 Resource hosts index，支持 const/file、JSON、CNAME、wildcard 和 exact/NODATA/NXDOMAIN 语义；DNS Core focused tests 14 项通过。

第七个小阶段（已完成）：`PolicyIndex` 接入 const/file hosts/rule-set loader，按 listener hosts → strategy rule 顺序生成安全的 `ResolutionPlan` matched-rule 摘要，并对 remote/dat/selector/缺失资源返回显式错误；随后增加 supplied compiled remote snapshot 的初始构造路径，供 async PreparedRuntime 在 bind 前完成 remote rule-set 编译。Policy focused tests 和 async prepare 测试通过；Runtime snapshot 原子接线和完整 upstream/cache 管线仍未实现。

### 阶段 8：DoH 接入与代理安全边界

涉及：Transport、Upstream、Config。

- 实现 DoH GET/POST、TLS terminate/external、forwarded header；
- 实现 PROXY v1/v2、SOCKS5/SOCKS5H 和 SecretRef 防泄漏；
- 验收：协议边界、可信代理、Host/SNI 和大消息限制测试通过。

首个小阶段（已完成）：为 DoH bind plan 增加 typed endpoint binding，补充 opaque TCP byte-stream capability，实现 plain HTTP/1.x GET/POST codec、无填充 base64url、路由 `{client_id}` 匹配、固定 HTTP 错误状态和 DNS `application/dns-message` 响应；service 以受监督 listener/session task 接入。当前只接受 `tls.mode: external` 与 `client_ip.source: peer`，`terminate`、`forwarded_header`、`proxy_protocol` 会在装配阶段明确拒绝。定向 codec/session 测试 9 项，真实 smoke 使用 `127.0.0.1:8355` 直接 HTTP POST/GET。

当前边界：HTTP/1.x 仍按读取顺序处理，未实现入站 TLS terminate/external 握手、PROXY v1/v2、forwarded header 信任链、HTTP/2 和完整资源/故障注入验收；上游 DoH HTTPS/TLS adapter 已完成 live handshake 验证。

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

第三十六个小阶段（已完成）：将 `RuntimeCoordinator` 的资源 worker 查询、刷新和关闭入口提升为 coordinator-facing API；`Application` 创建并共享 `Arc<RuntimeCoordinator>`，`DnsService` 持有该 coordinator，资源 refresh task 每轮读取当前活动 runtime，同时保留启动时 listener task 的固定 runtime 句柄。新增 coordinator 级 file hosts 刷新测试，验证资源 metadata 在当前活动实例上完成版本化发布；当前配置候选 prepare/bind/activate、动态 worker 集合重建、独立 listener swap 和完整服务故障矩阵仍留在后续小阶段。阶段完成后全量测试为 389 passed，0 failed。

第三十七个小阶段（已完成）：为 `Supervisor` 增加 `spawn_with_factory`，允许可重建 task 在 `TaskError::Transient` 下按 `RestartPolicy::Transient { max_restarts }` 进行可取消的指数退避和有界重试；`TaskCompletion` 暴露 `restart_count` 与 `restart_exhausted()`，`ShutdownReport` 聚合已发生的重试次数。新增成功重试、上限耗尽和 shutdown 计数测试；当前 service listener/resource task 的具体 factory 故障矩阵接入、fatal/degraded 分项升级和完整 shutdown 报告仍留在后续小阶段。阶段完成后全量测试为 392 passed，0 failed。

第三十八个小阶段（已完成）：为 `RuntimeCoordinator` 增加候选 `bind_and_activate` 入口，统一调用 `bind_prepared`、revision CAS 和旧 runtime drain；bind 失败不发布候选，CAS 失败通过 `RuntimeReloadError` 返回已绑定 candidate 供调用方重试。新增 fake `SocketFactory` 成功激活与 CAS 失败保留旧 runtime 测试；Application 配置变更触发、transport listener 重建和完整跨 Runtime service reload 仍留在后续小阶段。阶段完成后全量测试为 394 passed，0 failed。

第三十九个小阶段（已完成）：为 `RuntimeCoordinator` 增加 `refresh_resource_if_current`，以 captured `Arc<ActiveRuntime>` 做刷新前后活动实例校验；service refresh loop 使用同一 captured runtime 计算 due/backoff，检测到候选切换后跳过旧结果并重新读取当前实例。新增 stale-active rejection 测试；跨 Runtime resource candidate merge、动态 worker 集合重建、Application reload trigger、transport listener 重绑和完整故障矩阵仍留在后续小阶段。阶段完成后全量测试为 395 passed，0 failed。

第四十个小阶段（已完成）：新增 `Application::reload_runtime_from_path`，在无 snapshot 写入的配置加载边界执行 SecretRef 校验、async `PreparedRuntime` 构造和 `RuntimeCoordinator::bind_and_activate`；配置错误或候选失败均不改变当前 `ActiveRuntime`，成功 reload 递增 revision 并 drain 旧实例。新增 Application reload 成功/失败测试；`run` 尚未接入文件监视或管理事件，transport listener 重建、资源 worker 集合重建和完整跨 Runtime 服务生命周期仍留在后续小阶段。阶段完成后全量测试为 397 passed，0 failed。

第四十一个小阶段（已完成）：让 `DnsService::wait_for_ctrl_c` 同时观察 Supervisor task completion；Degraded 组件的终止失败仅记录并继续，FatalEndpoint/Fatal、重试耗尽和 task panic 映射为 `ServiceError::TaskFailure` 并停止接收新请求。新增 service fault classification 测试，并同步 Application 的运行期错误映射；listener factory 重建、完整 endpoint/resource/storage 故障矩阵和 shutdown 分项报告仍留在后续小阶段。阶段完成后全量测试为 401 passed，0 failed。

第四十二个小阶段（已完成）：为 `Supervisor` 增加 `spawn_scoped` 和 `cancel_task`，由 Supervisor 持有每个 scoped task 的 cancellation；scoped task 同时响应自身取消和全局 shutdown，完成后从注册表清理。新增 sibling 隔离取消与全局回收测试；service listener swap 尚未接入该能力，listener factory 重建、资源 worker 集合重建和完整服务故障矩阵仍留在后续小阶段。阶段完成后全量测试为 403 passed，0 failed。

第四十三个小阶段（已完成）：将 DnsService 的 UDP/TCP/DoH transport task 改为 `Supervisor::spawn_scoped` 注册，保存每个 listener 的独立 cancellation，并在 service shutdown 时显式取消 transport task；resource refresh task 继续使用 Supervisor 全局 cancellation。新增 transport task scoped registration 测试；listener reload/rebind 尚未接入，资源 worker 集合重建、完整 endpoint 故障矩阵和 flush 生命周期仍留在后续小阶段。阶段完成后全量测试为 404 passed，0 failed。

第四十四个小阶段（已完成）：新增 `DnsService::reload_prepared`，在候选 bind 与 transport adapter 预构造成功后执行 revision CAS，使用带 revision 的 task ID 注册新 UDP/TCP/DoH listener task，再取消旧 scoped token并更新 service runtime；Application 新增 `reload_service_from_path`，复用无 snapshot 配置加载、SecretRef 校验和 async prepare 边界。新增系统 loopback listener rebind 测试，验证新 revision 接管新端口并可正常 shutdown；`run` 自动配置变更事件、资源 worker 集合重建、listener 故障自动重建和完整 flush 生命周期仍留在后续小阶段。阶段完成后全量测试为 405 passed，0 failed。

第四十五个小阶段（已完成）：将 resource refresh task 改为 `Supervisor::spawn_scoped` 注册并保存独立 cancellation；service reload 按新 Runtime 的 resource worker ID 集合重建任务，旧集合在新任务注册成功后取消，且取消清理作用于旧 task 捕获的 runtime，避免误停新 runtime scheduler。新增系统 loopback resource worker token reconciliation 测试；`run` 自动配置变更事件、完整跨 Runtime 候选发布、listener 故障自动重建和 flush 生命周期仍留在后续小阶段。阶段完成后全量测试为 406 passed，0 failed。

第四十六个小阶段（已完成）：为 Supervisor 记录 Tokio `JoinSet` task ID 到 FluxDNS `TaskId` 的映射，并改用 `join_next_with_id` 精确处理正常退出、panic 和 abort；JoinError 不再按注册表顺序猜测任务，补充 sibling 保持存活的 panic 归因回归测试。`run` 自动配置变更事件、完整跨 Runtime 候选发布、listener 故障自动重建和 flush 生命周期仍留在后续小阶段。阶段完成后全量测试为 407 passed，0 failed。

第四十七个小阶段（已完成）：新增 `Supervisor::spawn_scoped_with_factory`，让 task 同时具备 scoped cancellation 和 `TaskError::Transient` 有界重试；UDP/TCP/DoH transport task 改为三次重试上限，重试间复用同一已绑定 socket和共享请求/连接计数器，重试耗尽仍按 `FatalEndpoint` 升级。新增 scoped factory 与 service transport retry 测试；自动 listener rebind、`run` 自动配置变更事件、完整跨 Runtime 候选发布和 flush 生命周期仍留在后续小阶段。阶段完成后全量测试为 409 passed，0 failed。

第四十八个小阶段（已完成）：将 Supervisor 的 Tokio `JoinSet` task ID 映射从单独 `TaskId` 升级为完整 `TaskSpec`，panic/abort 结果在精确归因的同时保留原始 component、fault level 和 restart policy；补充 panic completion 元数据断言。`run` 自动配置变更事件、完整跨 Runtime 候选发布、listener 自动 rebind 和 flush 生命周期仍留在后续小阶段。阶段完成后全量测试为 409 passed，0 failed。

第四十九个小阶段（已完成）：为 `config::load` 测试临时目录增加进程内原子序号，避免并发测试仅依赖纳秒时间戳而发生目录复用和提前清理；生产配置快照逻辑未改变。并发快照测试及全量 409 项测试通过；`run` 自动配置变更事件、完整跨 Runtime 候选发布、listener 自动 rebind 和 flush 生命周期仍留在后续小阶段。

第五十个小阶段（已完成）：为 `ResourceRegistrySnapshot` 增加按资源过滤的 immutable registry 合并原语；候选 registry 仅接收旧 Runtime 中严格更高的 `ResourceVersion`，同版本或候选更高版本保留候选内容，并补充不同资源、版本优先级和过滤条件测试。Policy、worker schedule、Runtime metadata 的跨 Runtime 同步仍留在后续小阶段。阶段完成后资源 snapshot 定向测试 6 项通过；全量测试基线更新为 411 passed，0 failed。

验证基线补充（已完成）：修复 Windows 下测试夹具对 Unix-only `work.path` 和非法临时文件名的依赖，统一使用进程隔离的绝对临时路径；生产配置解析和资源加载逻辑未改变。使用 Rust 1.98.0 执行 `fmt --check`、`check --locked`、`clippy --all-targets -- -D warnings` 及全量 `test --locked`，结果分别通过，测试为 411 passed、0 failed。

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

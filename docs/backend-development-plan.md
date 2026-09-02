# FluxDNS 后端开发计划

> 状态：v1 模块方案已完成；当前执行至阶段 81（启动时切换 telemetry 输出与级别过滤），后续重点是 typed final subscriber/监督任务接线、SQLite 真实故障复现和 v1 验收。
>
> 更新日期：2026-09-02
>
> 总体架构：[backend-architecture.md](backend-architecture.md)
>
> 配置契约：[configuration-reference.md](configuration-reference.md)

## 1. 当前进度结论

本节只保留当前决策所需的摘要；模块实现细节见对应 `docs/backend-modules/*.md`，完整阶段证据见本文“阶段实施记录”。

- 已完成主链路：Config 严格加载与校验、Runtime 候选/激活/受管 task、UDP/TCP/DoH plain HTTP、Upstream direct/group/proxy、Policy/Resource 首次快照与 live publish、Cache memory/Moka/SQLite 首轮 adapter。
- Storage 已完成 SQLite migration、stats/detail transaction、脱敏详情 bounded worker、淘汰策略、`StatsPersistenceWorker`、统一 stats/backend/detail 生命周期 facade、可共享 `StatsRecorder`、首轮 `StorageRuntime` 生产接线、pending 内存保护/fatal 边界、首轮 degraded/recovery 状态边界及 adapter-level Busy/DiskFull fault 注入恢复分类；Observability 已完成低基数 metrics/health 基础和稳定 telemetry ports 的有界 writer/flush 边界。
- 当前未完成：完整跨 Runtime 配置候选发布、DoH 入站 TLS/PROXY/forwarded、SQLite busy/disk-full recovery、完整 upstream/group/资源详情元数据、final tracing subscriber 与 supervisor 接线、最终故障/压力/conformance 验收。

> 注：阶段 81 后的当前数值以本节汇总表和“当前验证记录”为准；历史阶段证据只在“阶段实施记录”中保留。

阶段 81 已将启动时 telemetry 输出与级别过滤接入 Application：严格配置和 SecretRef 校验通过后，`logs.enable/path/level` 切换 bootstrap subscriber 的共享输出和 reloadable filter；typed final subscriber、degraded health 发布和 Supervisor 接线仍单独追踪。最近一次大阶段全量测试为 417 passed、0 failed；本阶段沿用阶段 79 的 `observability::tests::structured_output_writes_typed_events_to_a_real_file` 1 项输出 adapter 增量证据，并通过 `cargo check`/`clippy` 验证 Application 接线；阶段 80 的输出目标接线证据保持不变。

| 口径 | 当前值 | 说明 |
| --- | ---: | --- |
| 模块方案覆盖率 | 100% | 本计划覆盖 12 个后端顶层模块，每个模块均有独立方案文档 |
| 后端代码实现进度 | **70.6%** | 主要请求、Runtime、策略、资源、缓存、Storage、观测元数据和 telemetry writer/真实输出/级别过滤首轮链路已接入；当前剩余工作集中在跨 Runtime 候选发布、DoH 入站安全边界、SQLite busy/disk-full 故障注入与恢复验收、完整 upstream/group/资源详情元数据、typed final subscriber/监督任务接线及最终故障验收。各模块的实现细节和证据见对应模块文档。 |
| v1 交付总进度 | **73.5%** | 设计阶段 10% 已完成，加上实现与验收部分的 `90% × 70.6%` |

### 当前验证记录

- 最近一次大阶段全量后端测试：`417 passed、0 failed`。
- 当前阶段（阶段 81）增量验证：复用阶段 79 的 `observability::tests::structured_output_writes_typed_events_to_a_real_file`，`1 passed、0 failed`，并执行 `cargo check --manifest-path backend/Cargo.toml --locked` 与 `cargo clippy --manifest-path backend/Cargo.toml --locked --all-targets -- -D warnings`；阶段 80 的输出目标接线、阶段 78 的 Policy/Storage focused 测试及阶段 77/76 的 adapter fault、telemetry writer 增量证据均保持通过。
- 小阶段只执行增量验证；完成大阶段时再执行全量后端测试。
- `StorageRuntime` 已纳入 `Application` prepare、`DnsService` 的 `Supervisor` flush task 和 drain 后 shutdown；统计 pending 超限会通过受监督 task 升级为 fatal，SQLite degraded 成功操作可恢复 healthy，adapter-level Busy/DiskFull fault 已有确定性注入恢复分类；Policy Core 已通过可选 observation 接口向 Stats/resolve detail 传播 strategy/source/cache；`TelemetryWriter` 已纳入稳定 telemetry ports 的有界排队、优先级、失败重排队和 deadline-aware flush 边界，`StructuredTelemetryOutput` 已能写入真实文件/stderr，`run` 已在配置校验后切换共享输出目标和级别过滤，但尚未接入 typed final subscriber、degraded health 发布和监督任务；当前进度为后端 `70.6%`、v1 `73.5%`，OS/SQLite 真实故障和最终故障压力验收仍未完成。

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
| Application | `backend/src/main.rs`、`backend/src/app.rs` | [application.md](backend-modules/application.md) | 已完成 | 实现中 | 55% | 4% |
| Ports | `backend/src/ports/*` | [ports.md](backend-modules/ports.md) | 已完成 | 实现中 | 35% | 8% |
| Config | `backend/src/config/*` | [config.md](backend-modules/config.md) | 已完成 | 已验证 | 100% | 10% |
| Runtime | `backend/src/runtime/*` | [runtime.md](backend-modules/runtime.md) | 已完成 | 实现中 | 65% | 12% |
| Transport | `backend/src/transport/*` | [transport.md](backend-modules/transport.md) | 已完成 | 实现中 | 50% | 11% |
| DNS Core | `backend/src/dns/*` | [dns-core.md](backend-modules/dns-core.md) | 已完成 | 实现中 | 60% | 10% |
| Policy | `backend/src/policy/*` | [policy.md](backend-modules/policy.md) | 已完成 | 实现中 | 70% | 8% |
| Upstream | `backend/src/upstream/*` | [upstream.md](backend-modules/upstream.md) | 已完成 | 实现中 | 99% | 10% |
| Cache | `backend/src/cache/*` | [cache.md](backend-modules/cache.md) | 已完成 | 实现中 | 66% | 9% |
| Resource | `backend/src/resource/*` | [resource.md](backend-modules/resource.md) | 已完成 | 实现中 | 90% | 7% |
| Storage | `backend/src/storage/*`、`backend/migrations/*` | [storage.md](backend-modules/storage.md) | 已完成 | 实现中 | 82% | 8% |
| Observability | `backend/src/observability.rs` | [observability.md](backend-modules/observability.md) | 已完成 | 实现中 | 65% | 3% |

后端代码实现总进度：

```text
4% × 55% + 8% × 35% + 10% × 100% + 12% × 65% + 11% × 50% + 10% × 60% + 8% × 70% + 10% × 99% + 9% × 66% + 7% × 90% + 8% × 82% + 3% × 65% ≈ 70.6%
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

当前后端代码实现进度为 70.1%，v1 交付进度为 73.1%。Config、Runtime、Transport、Upstream、Policy、Resource 和 Cache 的首轮链路已建立；Storage 已完成 SQLite adapter、detail writer、StatsPersistenceWorker、统一生命周期 facade、首轮服务生产接线、pending 内存保护边界、首轮 degraded/recovery 状态转换及 policy source/cache/strategy 元数据落库；Observability 已完成稳定 telemetry ports 的有界 writer/flush 边界。后续按“Runtime 生命周期 → Storage/Observability 完整性 → DoH 安全边界 → v1 验收”推进，不按文档完成度虚增进度。

| 顺序 | 目标 | 主要范围 | 退出条件 |
| --- | --- | --- | --- |
| 0 | 开发环境与验证门 | 直接调用项目 Rust 1.98.0；执行 `cargo fmt --manifest-path backend/Cargo.toml --all -- --check`、`cargo check --manifest-path backend/Cargo.toml --locked`、`cargo clippy --manifest-path backend/Cargo.toml --locked --all-targets -- -D warnings` 和 `cargo test --manifest-path backend/Cargo.toml --locked` | 版本来源可追溯，最近一次大阶段基线为 `417 passed、0 failed`，后续阶段不得用 `--ignore-rust-version` 替代正式验证 |
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

### 阶段实施记录

以下记录只保留每个阶段的目标、实际变更和验证证据；详细设计回链到对应模块文档。

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

阶段 3 小阶段索引（1～9，均已完成）：建立 `PreparedRuntime`/`RuntimeSnapshot`/`BoundCandidate`、`ArcSwap` revision CAS 与 drain、`Supervisor` task tree、系统 socket capability、Application CLI/run、UDP/TCP service、TCP session 和 Runtime metadata/core 同 revision 边界。代表性证据为 Runtime targeted tests、UDP/TCP activation tests 及 service smoke；具体变更见 Runtime/Application 模块文档和对应提交。

### 阶段 4：DNS Core 与 UDP/TCP

涉及：DNS Core、Transport、Policy 的最小默认策略。

阶段 4 小阶段索引（1～4，均已完成）：完成 wire codec、固定 SERVFAIL/hosts Core、UDP/TCP framing、ID/EDNS/TC、TCP 持久 session 和连接级顺序响应；验收覆盖 UDP/TCP 一致性、畸形报文、连续 frame、半帧、clean EOF 与取消。

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

小阶段索引 1～13（均已完成）：完成 hosts/DoH exchange、group selection/outcome、bootstrap answer/resolver 及 Registry 基础接线；具体边界与测试见 [upstream.md](backend-modules/upstream.md)。

小阶段索引 14～21（均已完成）：完成 outbound profile/credential、SOCKS5/SOCKS5H codec、stream/dial/hostname resolver、proxy DoH adapter 和配置驱动 Registry/Policy/Runtime prepare；具体边界与测试见 [upstream.md](backend-modules/upstream.md)。

小阶段索引 22～26（均已完成）：完成 direct/group fallback、Reqwest/Rustls direct/proxy HTTPS、parallel late-window、nested group 和对应的 deadline/取消语义；具体边界与测试见 [upstream.md](backend-modules/upstream.md)。

小阶段索引 27～35（均已完成）：完成 loopback TLS 信任链验证、parallel late sink、finalizer 生命周期、ResourceRefreshWorker、Policy live swap、生产 ResourceFetcher、async prepare 及 remote/file refresh worker；具体边界与测试见 [upstream.md](backend-modules/upstream.md)、[cache.md](backend-modules/cache.md)、[resource.md](backend-modules/resource.md)。

阶段 5/资源当前边界：完整跨 Runtime candidate 生命周期、resource-only swap、严格 encoder 完成 hook、长期压力和 service-level fault matrix 仍未实现。

### 阶段 6：缓存

涉及：Cache、DNS Core、Storage 的独立缓存 adapter。

- 实现 namespace、key、TTL、single-flight、optimistic refresh 和 CAS；
- 接入 Moka 与独立 SQLite cache store；
- 验收：缓存准入、质量替换、恢复降级和资源变化不全局失效测试通过。

小阶段索引 1～7（均已完成）：完成内存 `CacheStore`、响应准入、稳定 key/namespace、`CacheFacade`、single-flight、容量淘汰、FDCP 文件持久化、`LateCacheFinalizer` 和 Policy 请求链路；具体边界与测试见 [cache.md](backend-modules/cache.md)。

小阶段索引 8～10（均已完成）：接入 `MokaCacheStore` 并作为 Policy 默认 store，新增独立 `SqlitePersistentCacheStore`；Moka、Policy 与 SQLite adapter 增量测试均通过。WAL/SHM 观测、busy/disk-full recovery、跨 adapter contract/fault tests 和完整 late-window candidate 生命周期仍未完成。

### 阶段 7：完整策略与资源

涉及：Policy、Resource、DNS Core。

- 实现 client、strategy、hosts、rule_set 编译索引；
- 完成本地/远程资源首次快照、每资源 revision 和原子发布；
- 验收：匹配优先级、资源解析、首次失败和乱序刷新测试通过。

小阶段索引 1～8（均已完成）：完成 client/strategy/route index、`ResolutionPlan`、hosts/rule parser 与 loader、resource snapshot/CAS、`ResourceRefreshRuntime`，并接入 DNS Core/Policy；具体边界与测试见 [policy.md](backend-modules/policy.md)、[resource.md](backend-modules/resource.md) 和 [dns-core.md](backend-modules/dns-core.md)。

### 阶段 8：DoH 接入与代理安全边界

涉及：Transport、Upstream、Config。

- 实现 DoH GET/POST、TLS terminate/external、forwarded header；
- 实现 PROXY v1/v2、SOCKS5/SOCKS5H 和 SecretRef 防泄漏；
- 验收：协议边界、可信代理、Host/SNI 和大消息限制测试通过。

小阶段索引 1（已完成）：接入 DoH plain HTTP/1.x GET/POST、typed endpoint、路由、HTTP/DNS 错误分层及受监督 listener/session；codec/session 定向测试和 `127.0.0.1:8355` smoke 通过。具体边界见 [transport.md](backend-modules/transport.md)。

当前边界：HTTP/1.x 仍按读取顺序处理，未实现入站 TLS terminate/external 握手、PROXY v1/v2、forwarded header 信任链、HTTP/2 和完整资源/故障注入验收；上游 DoH HTTPS/TLS adapter 已完成 live handshake 验证。

### 阶段 9：统计、详情日志与观测

涉及：Storage、Observability、DNS Core。

- 完成 SQLite migration、daily stats、batch ledger 和独立详情 writer；
- 完成 degraded 状态、persistence gap、低基数 metrics 和脱敏日志；
- 验收：跨午夜、幂等重试、队列溢出、数据库 busy/磁盘故障测试通过。

小阶段索引 1～19（均已完成）：完成 UTC day/sharded accumulator、epoch/batch ledger、SQLite migration 与幂等 stats transaction、脱敏 detail、bounded detail writer、淘汰/硬上限、周期 flush/shutdown，以及 `StorageService` backend/detail 生命周期 facade 和 `StatsPersistenceWorker` 首轮闭环。代表性证据见 [storage.md](backend-modules/storage.md)；最近一次大阶段全量基线为 `417 passed、0 failed`，各小阶段只保留增量测试记录。

小阶段索引 20（已完成）：将 `StatsPersistenceWorker` 接入 `StorageService` 的 stats flush/shutdown 顺序，并暴露可共享的同步 `StatsRecorder`；`storage::service::tests` 增量测试 `3 passed、0 failed`。

小阶段索引 21（已完成）：按 `ResolvedConfig` 组装 `StorageRuntime`，在 `Application` prepare 阶段打开并迁移业务 SQLite，在 `DnsService` 注册受监督的周期 flush task，shutdown 时先 drain writer 再关闭 backend；请求数据面接入首轮 transport/outcome 聚合和脱敏详情事件。增量测试为 `storage::service::tests` `4 passed、0 failed`，以及 service 生命周期测试 `1 passed、0 failed`。

小阶段索引 22（已完成）：为 stats ledger 增加固定 pending batch/event 内存保护，超限时不切换活动 epoch、不丢失新请求，并由 `StorageService`/`Supervisor` 将该错误分类为 fatal；普通 backend 失败仍保留 pending 供重试。增量测试为 `storage::stats::tests` `4 passed、0 failed`、`storage::statistics::tests` `4 passed、0 failed`、`storage::service::tests` `5 passed、0 failed`，以及 service failure classification `1 passed、0 failed`。

小阶段索引 23（已完成）：修正 SQLite `Degraded` 状态的可恢复边界，允许 degraded 状态继续执行有限重试；成功 migration/execute/detail/checkpoint 会恢复 `Healthy`，不可恢复 SQL 错误进入 `Failed`，健康探针不会绕过 failed 状态。`storage::sqlite::tests` 增量测试 `12 passed、0 failed`。

小阶段索引 24（已完成）：为稳定 telemetry ports 增加 `TelemetryWriter`，统一实现有界日志/metrics/health 排队、优先级丢弃与保留、输出失败重排队、deadline-aware flush 和 shutdown 关闭边界；`observability::tests::telemetry_writer` 增量测试 `5 passed、0 failed`。最终 tracing subscriber、真实文件/stderr output、degraded health 发布和 supervisor 接线仍待后续小阶段。

小阶段索引 25（已完成）：为 SQLite execute/detail transaction 增加受 `cfg(test)` 限定的 `InjectedSqliteFault::{Busy,DiskFull}`，修正内部 `Unavailable` 错误进入 `Degraded` 的状态转换，并验证下一次成功操作恢复 `Healthy`；`storage::sqlite::tests::injected_busy_and_disk_full` 增量测试 `1 passed、0 failed`。OS/SQLite 真实故障复现仍待后续验收。

小阶段索引 26（已完成）：为 `DnsCore` 增加可选 observation 接口，Policy Core 统一返回生效 strategy、answer source 和 cache status；`ObservedDnsCore` 将其写入 stats 维度与 resolve detail，SQLite `resolve_log.source` 使用受控枚举值。增量测试为 `dns::policy::tests::` `18 passed、0 failed`、`storage::sqlite::tests::` `13 passed、0 failed` 和 `storage::resolve_log::tests::` `6 passed、0 failed`。

小阶段索引 27（已完成）：新增 `StructuredTelemetryOutput`，将已脱敏的 `LogEvent`、`MetricEvent` 和 `ComponentHealthEvent` 写入真实文件或 stderr，写入失败返回安全 `PortError`；`observability::tests::structured_output_writes_typed_events_to_a_real_file` 增量测试 `1 passed、0 failed`。动态 final subscriber 与 Supervisor flush task 仍待后续验收。

小阶段索引 28（已完成）：`init_bootstrap` 使用 reloadable level filter 和共享输出 writer，Application 在配置/SecretRef 校验后按 `logs.enable/path/level` 切换文件、stderr 或 Sink；typed final event subscriber、degraded health 发布和 Supervisor flush task 仍待后续验收。增量通过 `cargo check`、`cargo clippy` 及阶段 79 输出测试。

阶段 9 当前边界：OS/SQLite 真实 busy/disk-full 故障复现与恢复验收、完整 upstream/group/资源详情元数据、typed final tracing subscriber、degraded health 发布、supervisor 接线和最终故障注入仍未完成；adapter-level fault 注入、policy source/cache/strategy 首轮传播、真实输出 adapter 及启动时输出/级别切换已完成。

### 阶段 10：刷新、故障注入和 v1 验收

涉及：全部模块。

- 完成资源定时刷新、退避、stale 状态和并发 CAS；
- 完成 listener、数据库、缓存、资源和 telemetry/log sink 故障注入；
- 完成协议 conformance、压力测试和长期运行检查；
- 同步 README、配置示例、模块进度和最终验收证据。

小阶段索引 36～58（均已完成）：完成 RuntimeCoordinator/Resource worker 生命周期、Supervisor 有界重试与 scoped cancellation、精确 task 归因、配置 fingerprint 自动 reload、listener 复用与 rebind、跨 Runtime 资源状态合并、LateCacheFinalizer owner、最新 Runtime 路由和 resource worker 增量协调。

代表性验证：Runtime/Service/Application focused tests、loopback listener/resource reload 和并发快照测试均通过；最近一次大阶段全量基线为 `417 passed、0 failed`。当前仍未完成完整跨 Runtime candidate 生命周期、resource-only swap、listener 自动重建、SQLite 真实故障复现、final telemetry output 与最终故障矩阵；具体边界见 [runtime.md](backend-modules/runtime.md)、[application.md](backend-modules/application.md) 和 [cache.md](backend-modules/cache.md)。

## 6. 阶段提交规则

本计划中的每个编号阶段都必须形成一次独立的本地提交。若某个阶段继续拆分为多个可独立实现和验收的小阶段，则每个小阶段也必须各自提交一次，不能等到整个大阶段结束后合并成一个无法审计的提交。

代码提交以“小阶段”为最小粒度：小阶段完成约定范围并通过验收后应立即提交，不得把多个小阶段的代码、测试和文档累计到一次提交中，也不得以单次大提交替代阶段化交付。一个小阶段即使涉及多个模块，也必须围绕同一可说明、可验证、可回滚的目标组织变更。

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

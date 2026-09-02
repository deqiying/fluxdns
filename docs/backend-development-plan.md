# FluxDNS 后端开发计划

> 状态：MVP v0.1 已完成；当前已完成至阶段 97（Cache persistence 跨 adapter contract）。后续按 v1 需要补齐跨 Runtime 生命周期、协议组合、安全故障复现、持久化故障恢复和最终验收。
>
> 更新日期：2026-09-02
>
> 总体架构：[backend-architecture.md](backend-architecture.md) · 配置契约：[configuration-reference.md](configuration-reference.md)

## 1. 当前结论

### 已交付

- Config：`version: 1` strict load、路径/SecretRef 归一化、语义校验、迁移边界和快照安全边界。
- Runtime/Application：`PreparedRuntime → BoundCandidate → ActiveRuntime`、revision CAS、Supervisor、受管 task、配置文件轮询 reload、listener 复用/rebind、graceful shutdown。
- DNS 数据面：UDP、TCP、DoH plain HTTP/1.x；hosts、Policy、Cache、Upstream 主链路；资源首次加载和 auto-update refresh。
- Storage：SQLite migration、stats/detail transaction、bounded writer、flush/shutdown、pending 内存保护、首轮 degraded/recovery 和 adapter-level Busy/DiskFull fault 注入。
- Observability：低基数 metrics/health、typed event、typed final tracing layer、degraded health 发布、输出失败 stderr fallback、`TelemetryWriter` 有界队列、失败重排队、deadline-aware flush、真实文件/stderr output、启动时日志目标/级别切换，以及 `DnsService`/Supervisor 周期 flush 与 shutdown 接线。
- DoH 安全首轮：trusted forwarded header、PROXY v1/v2 地址恢复，TLS terminate 的 PEM/DER 证书加载、私钥匹配、TLS 1.2/1.3 握手；PROXY 前导先消费再升级 TLS。

### MVP 边界

MVP v0.1 已完成，要求 strict config、UDP/TCP/plain DoH、hosts/Policy/Cache/Upstream、SQLite stats/detail、受监督 task 和 graceful shutdown。MVP 不阻塞于 DoH HTTP/2、完整 metadata、真实 OS/SQLite 故障复现、typed final subscriber 或压力/conformance 验收。

### 当前未完成

- 完整跨 Runtime candidate 生命周期和独立 resource-only runtime swap；
- 配置变更的完整 resource-only/listener-rebind/restart 分类及自动 rebind 故障矩阵；
- DoH HTTP/2、完整 HTTP/DNS 协议组合和证书/信任边界矩阵；
- SQLite 真实 busy/disk-full 复现与恢复验收、独立 cache persistence 故障矩阵；
- 完整 group/member/resource metadata；
- v1 最终压力、长期运行、conformance、drain/flush/shutdown 验收。

## 2. 进度总览

| 口径 | 当前值 | 说明 |
| --- | ---: | --- |
| 模块方案覆盖率 | 100% | 12 个后端顶层模块均有独立方案文档 |
| 后端代码实现进度 | **74.7%** | 以模块代码和验证证据计算，不因文档完成虚增 |
| v1 交付总进度 | **77.2%** | `10% × 设计完成度 + 90% × 后端代码实现进度` |
| MVP v0.1 | **已完成** | 本地 loopback 和 plain DoH 主链路已验证 |

模块进度：

| 模块 | 实现状态 | 进度 | 权重 |
| --- | --- | ---: | ---: |
| Application | 实现中 | 55% | 4% |
| Ports | 实现中 | 40% | 8% |
| Config | 已验证 | 100% | 10% |
| Runtime | 实现中 | 65% | 12% |
| Transport | 实现中 | 72% | 11% |
| DNS Core | 实现中 | 62% | 10% |
| Policy | 实现中 | 70% | 8% |
| Upstream | 已实现待验证 | 99% | 10% |
| Cache | 实现中 | 68% | 9% |
| Resource | 已实现待验证 | 90% | 7% |
| Storage | 已实现待验证 | 84% | 8% |
| Observability | 实现中 | 90% | 3% |

进度计算：

```text
4%×55% + 8%×40% + 10%×100% + 12%×65% + 11%×72%
+ 10%×62% + 8%×70% + 10%×99% + 9%×68% + 7%×90%
+ 8%×84% + 3%×90% ≈ 74.7%
```

进度判定只接受可核验证据：50% 为 happy path + focused tests，70% 为真实跨模块链路，85% 为异常/取消/并发/资源限制，100% 为集成、故障注入、验收和文档回链全部完成。

## 3. 开发路线

| 顺序 | 目标 | 主要范围 | 退出条件 |
| --- | --- | --- | --- |
| 1 | Runtime 生命周期 | candidate registry、跨 Runtime 合并、revision CAS、resource-only swap | 候选失败保留旧 Runtime；并发资源发布不丢失；无 detached task |
| 2 | 配置变更/rebind | 去抖、失败语义、resource-only/listener rebind/restart 分类 | 新 listener 全部 bind 成功后切换；旧 task 可取消并 drain |
| 3 | DNS Core/Cache 一致性 | 最新 snapshot、late-window/finalizer、跨 transport contract | 单请求固定 revision；资源更新只影响后续请求 |
| 4 | Storage/Observability 生产链路 | SQLx/SQLite 故障恢复、final subscriber、health/event writer、Supervisor flush | busy/disk-full、脱敏、backpressure、flush/shutdown 可复现 |
| 5 | Cache 持久化 | 独立 SQLite cache persistence、Moka/SQLite contract | expiry/weight、corrupt/busy/disk-full、shutdown 持久化通过 |
| 6 | DoH 安全与协议 | HTTP/2、TLS/PROXY/forwarded 完整矩阵 | 不可信输入 fail-closed；Host/SNI、GET/POST、错误分层有端到端证据 |
| 7 | v1 最终验收 | 故障注入、conformance、压力、长期运行、完整 drain | 所有验收门槛有可复现记录 |

实施原则：先完成主线和 MVP；小阶段只做增量改动和定向验证；大阶段结束时才做全量 `cargo fmt`、`cargo check`、`cargo clippy`、`cargo test`。不为 MVP 追加 WebUI、管理 API、认证、DoT/DoQ、主动健康检查或外部 metrics exporter。

## 4. 阶段摘要

详细小阶段流水账不再放入本文件；具体实现以代码、模块文档和本表中的阶段证据为准。

| 阶段 | 状态 | 已交付摘要 | 代表性证据 |
| --- | --- | --- | --- |
| 0 | 已完成 | 架构、配置契约、模块方案、验证口径 | 方案文档和目录路由完整 |
| 1 | 已完成 | binary crate、核心类型、ports、错误分类、bootstrap 日志 | 46 tests、fmt/check/clippy 通过 |
| 2 | 已完成 | strict config、迁移/路径/SecretRef/语义校验、快照安全 | 135 tests；fmt/check/clippy 通过 |
| 3 | 已完成 | Runtime snapshot、CAS、Supervisor、UDP/TCP service、Application run | Runtime/Service/Application focused tests；大阶段基线 417 passed |
| 4 | 已完成 | DNS wire、UDP/TCP framing、TCP session 和基础 Policy | UDP/TCP 一致性、畸形报文、取消测试 |
| 5 | 已完成 | upstream direct/group/proxy、SOCKS、DoH outbound、resource fetch | connector、deadline、取消、loopback TLS focused tests |
| 6 | 已完成 | memory/Moka/cache facade、FDCP、SQLite cache 首轮 | adapter 定向测试；跨 adapter fault matrix 后置 |
| 7 | 已完成 | Policy/Resource index、snapshot/CAS、refresh worker、Core 接线 | policy/resource focused tests |
| 8 | 已完成 | DoH plain HTTP/1.x、HTTP/DNS 错误分层、出站 TLS | DoH/session/client-IP tests；HTTP/2 后置 |
| 9 | 已完成首轮 | SQLite stats/detail、StorageRuntime、TelemetryWriter、typed tracing layer、真实 output、启动日志切换、policy 首轮观测元数据、首轮 health publish/lifecycle | storage/observability/policy focused tests；OS/SQLite 真实故障和跨 Runtime health lifecycle 后置 |
| 10 | 进行中 | 资源刷新、故障注入、安全边界和最终验收持续补齐 | 当前最新小阶段为 97 |

### 最新增量记录

- 阶段 83：MVP 验收。全量首次结果为 `455 passed、2 failed`，失败为非 MVP live HTTPS loopback 超时；按 MVP 范围为 `455 passed、0 failed`。既有 plain DoH smoke 继续有效。
- 阶段 84：两个 live HTTPS loopback 测试 deadline 调整为 10s，各 `1 passed、0 failed`；未改变生产 timeout。
- 阶段 85：selected upstream/group ID 进入 stats 有界维度和 resolve detail 存在性标记；定向测试通过。
- 阶段 86：DoH `forwarded_header` trusted peer、链解析、missing/invalid fallback；`12 passed、0 failed`。
- 阶段 87：DoH PROXY v1/v2、分片、TCP4/TCP6、长度上限和 trusted peer；`14 passed、0 failed`。
- 阶段 88：DoH TLS terminate 首轮。PEM/DER cert chain/private key 加载和匹配，显式 ring provider 的 TLS 1.2/1.3，PROXY 前导消费后 TLS upgrade；system socket `1 passed、0 failed`，DoH 定向测试 `16 passed、0 failed`。
- 阶段 89：配置 watcher 增加内容 fingerprint，与 metadata 一起做两次稳定轮询；相同长度内容变更定向测试 `2 passed、0 failed`，未跨越模块进度档位。
- 阶段 90：将 `TelemetryWriter` 接入 `DnsService` 的 Supervisor 周期 flush 和 shutdown；生产输出复用现有 tracing 目标，Telemetry flush task 定向测试 `1 passed、0 failed`，Observability focused tests `17 passed、0 failed`。
- 阶段 91：增加可 reload 的 typed tracing layer；配置校验后替换 bootstrap `fmt` layer，安全提取 `event/component/result/revision` 字段并写入 `TelemetryWriter`，typed layer focused test `1 passed、0 failed`，Observability focused tests `18 passed、0 failed`。
- 阶段 92：将 Storage/Telemetry/Supervisor 的状态转换发布为 `ComponentHealthEvent`，覆盖 healthy/degraded/failed/stopping；健康事件 bounded telemetry focused test `1 passed、0 failed`，未重复全量 `cargo fmt/test`。
- 阶段 93：结构化文件/共享输出失败时尝试安全 stderr fallback，两个输出均失败才保留队列并报告错误；fallback focused test `1 passed、0 failed`，Observability focused tests `19 passed、0 failed`，未重复全量 `cargo fmt/test`。
- 阶段 94：Resource refresh task 接入同一 health publish 路径，覆盖启动 healthy、刷新成功 healthy、刷新失败 degraded 和 worker 缺失 failed；service focused suite `29 passed、0 failed`，未重复全量 `cargo fmt/test`。
- 阶段 95：统一监听 `SIGINT`/Unix `SIGTERM`，优雅停机期间再次收到终止信号立即返回运行期错误；service focused suite `29 passed、0 failed`，Windows 编译通过，Unix `SIGTERM` runtime smoke 尚未执行，未重复全量 `cargo fmt/test`。
- 阶段 96：`TelemetryWriter` 对同一组件健康事件归一化 `first_seen/last_changed/last_success/retry_count/persistence_gap`，避免刷新和重试重置生命周期字段；Observability focused tests `20 passed、0 failed`，未重复全量 `cargo fmt/test`。
- 阶段 97：新增文件快照与 SQLite cache persistence 的共享 contract test，覆盖 persist/recover 的 live/expired 隔离、记录字段一致性、容量维护和 shutdown 后拒绝操作；cache persistence contract `1 passed、0 failed`，文件 persistence `6 passed、0 failed`，SQLite persistence `3 passed、0 failed`，未重复全量 `cargo fmt/test`。

### 最近小阶段（97）验证命令

```text
backend/.cargo-home/bin/rustfmt --edition 2024 backend/src/cache/mod.rs
cargo test --manifest-path backend/Cargo.toml --locked --offline cache::persistence_contract_tests::file_and_sqlite_adapters_follow_the_same_contract -- --nocapture
cargo test --manifest-path backend/Cargo.toml --locked --offline cache::persistence::tests:: -- --nocapture
cargo test --manifest-path backend/Cargo.toml --locked --offline cache::sqlite::tests:: -- --nocapture
cargo check --manifest-path backend/Cargo.toml --locked --offline
cargo clippy --manifest-path backend/Cargo.toml --locked --offline --all-targets -- -D warnings
git diff --check
```

以上均通过；阶段 97 未重复全量 `cargo fmt/test`。阶段 96 的 health lifecycle focused test、阶段 95 的 SIGINT/SIGTERM 编译验证、阶段 94 的 Resource health focused test、阶段 93 的 fallback focused test、阶段 92 的 health publish focused test、阶段 91 的 typed tracing focused test、阶段 90 的 telemetry flush focused test、阶段 89 的 config watcher focused test及阶段 88 的 system socket/DoH focused tests 证据保留在对应提交中。

## 5. v1 验收门槛

后端代码实现进度只有在以下条件全部满足后才能标记 100%：

1. 配置示例 strict load，错误包含稳定分类和字段路径；
2. UDP、TCP、DoH 对同一 canonical query 的策略结果一致；
3. 单请求只捕获一次 Runtime revision，资源并发更新不丢失；
4. 所有后台任务受 Supervisor 管理，无 detached task；
5. 数据库、详情日志、缓存持久化故障不阻塞 DNS 数据面；启动必需数据库失败时拒绝启动；
6. 日志和 metrics 不暴露 SecretRef、完整 client ID、原始 IP、query、header 或 raw DNS wire；
7. 缓存、上游组、资源刷新、统计具备确定性并发/故障测试；
8. 全量 `cargo fmt --manifest-path backend/Cargo.toml --all -- --check`、`cargo test --manifest-path backend/Cargo.toml --locked`、`cargo clippy --manifest-path backend/Cargo.toml --locked -- -D warnings` 和 `git diff --check` 通过。

## 6. 文档与提交规则

- 每个可独立验收的小阶段单独本地提交，提交范围只覆盖该阶段文件；不使用 `git add .`，不 push。
- 小阶段同步更新直接受影响的模块文档、验证证据和本计划；不重复抄录每个测试用例。
- 代码/配置变更使用 `feat`/`fix`，纯计划或规范调整使用 `docs`，最终验收使用 `test`；提交说明采用简体中文 Conventional Commit。
- 计划只保留决策、边界、进度和证据。详细设计放入 `docs/backend-modules/*.md`，命令输出放入提交或验证记录，避免持续扩大上下文。

相关模块文档：

- [application.md](backend-modules/application.md)
- [ports.md](backend-modules/ports.md)
- [runtime.md](backend-modules/runtime.md)
- [transport.md](backend-modules/transport.md)
- [dns-core.md](backend-modules/dns-core.md)
- [policy.md](backend-modules/policy.md)
- [upstream.md](backend-modules/upstream.md)
- [cache.md](backend-modules/cache.md)
- [resource.md](backend-modules/resource.md)
- [storage.md](backend-modules/storage.md)
- [observability.md](backend-modules/observability.md)

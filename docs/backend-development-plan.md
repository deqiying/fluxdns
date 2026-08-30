# FluxDNS 后端开发计划

> 状态：v1 模块方案已完成，阶段 1 已完成
>
> 更新日期：2026-08-30
>
> 总体架构：[backend-architecture.md](backend-architecture.md)
>
> 配置契约：[configuration-reference.md](configuration-reference.md)

## 1. 当前进度结论

仓库已固定 `backend/` 与 `frontend/` 两个独立代码主目录；根目录不作为任一端的工程目录。`backend/` 已具备单 binary crate、核心契约和 46 个单元测试，尚无配置运行时、真实 adapter、listener、数据库 migration 或可提供 DNS 服务的运行链路。

| 口径 | 当前值 | 说明 |
| --- | ---: | --- |
| 模块方案覆盖率 | 100% | 本计划覆盖 12 个后端顶层模块，每个模块均有独立方案文档 |
| 后端代码实现进度 | **5%** | Application、Ports、DNS Core、Observability 均达到 20% 骨架与公共契约里程碑 |
| v1 交付总进度 | **14.5%** | 设计阶段 10% 已完成，加上实现与验收部分的 `90% × 5%` |

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
| Application | `backend/src/main.rs`、`backend/src/app.rs` | [application.md](backend-modules/application.md) | 已完成 | 实现中 | 20% | 4% |
| Ports | `backend/src/ports/*` | [ports.md](backend-modules/ports.md) | 已完成 | 实现中 | 20% | 8% |
| Config | `backend/src/config/*` | [config.md](backend-modules/config.md) | 已完成 | 未开始 | 0% | 10% |
| Runtime | `backend/src/runtime/*` | [runtime.md](backend-modules/runtime.md) | 已完成 | 未开始 | 0% | 12% |
| Transport | `backend/src/transport/*` | [transport.md](backend-modules/transport.md) | 已完成 | 未开始 | 0% | 11% |
| DNS Core | `backend/src/dns/*` | [dns-core.md](backend-modules/dns-core.md) | 已完成 | 实现中 | 20% | 10% |
| Policy | `backend/src/policy/*` | [policy.md](backend-modules/policy.md) | 已完成 | 未开始 | 0% | 8% |
| Upstream | `backend/src/upstream/*` | [upstream.md](backend-modules/upstream.md) | 已完成 | 未开始 | 0% | 10% |
| Cache | `backend/src/cache/*` | [cache.md](backend-modules/cache.md) | 已完成 | 未开始 | 0% | 9% |
| Resource | `backend/src/resource/*` | [resource.md](backend-modules/resource.md) | 已完成 | 未开始 | 0% | 7% |
| Storage | `backend/src/storage/*`、`backend/migrations/*` | [storage.md](backend-modules/storage.md) | 已完成 | 未开始 | 0% | 8% |
| Observability | `backend/src/observability.rs` | [observability.md](backend-modules/observability.md) | 已完成 | 实现中 | 20% | 3% |

后端代码实现总进度：

```text
4% × 20% + 8% × 20% + 10% × 20% + 3% × 20% = 5%
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

- 完成 versioned DTO、严格解析、迁移注册表、路径/SecretRef 归一化；
- 完成引用图、循环、继承、bind 冲突和 feature gate 校验；
- 生成不可变 `ResolvedConfig` 与结构化错误报告；
- 验收：配置 golden test 和示例配置全量校验通过。

### 阶段 3：Runtime 与启动闭环

涉及：Runtime、Application、Ports。

- 实现 `PreparedRuntime`、`RuntimeSnapshot`、`ActiveRuntime`、`BindPlan`；
- 实现 task supervisor、故障等级和优雅停机；
- 候选 prepare/bind 失败不得发布半成品；
- 验收：原子切换、失败保留旧 runtime、shutdown deadline 测试通过。

### 阶段 4：DNS Core 与 UDP/TCP

涉及：DNS Core、Transport、Policy 的最小默认策略。

- 打通 UDP/TCP framing → canonical request → core → response encoder；
- 完成 DNS ID、EDNS、截断、deadline 和错误响应语义；
- 验收：UDP/TCP 一致性、并发、畸形报文和取消测试通过。

### 阶段 5：上游解析

涉及：Upstream、Ports。

- 实现内联 hosts、单 DoH connector、bootstrap、connect_ip；
- 实现 `parallel`、`round-robin`、`load-balance`、`failover` 和 fallback；
- 验收：Host/SNI、HTTP/DNS 错误分层、超时与确定性选择测试通过。

### 阶段 6：缓存

涉及：Cache、DNS Core、Storage 的独立缓存 adapter。

- 实现 namespace、key、TTL、single-flight、optimistic refresh 和 CAS；
- 接入 Moka 与独立 SQLite cache store；
- 验收：缓存准入、质量替换、恢复降级和资源变化不全局失效测试通过。

### 阶段 7：完整策略与资源

涉及：Policy、Resource、DNS Core。

- 实现 client、strategy、hosts、rule_set 编译索引；
- 完成本地/远程资源首次快照、每资源 revision 和原子发布；
- 验收：匹配优先级、资源解析、首次失败和乱序刷新测试通过。

### 阶段 8：DoH 接入与代理安全边界

涉及：Transport、Upstream、Config。

- 实现 DoH GET/POST、TLS terminate/external、forwarded header；
- 实现 PROXY v1/v2、SOCKS5/SOCKS5H 和 SecretRef 防泄漏；
- 验收：协议边界、可信代理、Host/SNI 和大消息限制测试通过。

### 阶段 9：统计、详情日志与观测

涉及：Storage、Observability、DNS Core。

- 完成 SQLite migration、daily stats、batch ledger 和独立详情 writer；
- 完成 degraded 状态、persistence gap、低基数 metrics 和脱敏日志；
- 验收：跨午夜、幂等重试、队列溢出、数据库 busy/磁盘故障测试通过。

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

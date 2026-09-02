# Application 模块设计

> 状态：v1 方案已完成，已实现配置校验、Runtime bind、UDP/TCP/DoH plain HTTP service 启动和基础 graceful shutdown；正式 `run` prepare 已在 bind 前完成 remote rule-set restore-or-fetch，`Application` 创建的 `RuntimeCoordinator` 由 `DnsService` 持有，service Supervisor 负责长期 remote refresh task；Application 已提供无 snapshot 副作用的配置文件 reload 触发 API、service-aware reload 入口重建 listener task，以及基于配置文件 fingerprint 的去抖自动 reload；已接入配置驱动的正式日志输出目标和 reloadable level filter；`DnsService` 会观察 Supervisor 的终止 task 并升级不可恢复故障
>
> 更新日期：2026-09-02
>
> 目标代码：`backend/src/main.rs`、`backend/src/app.rs`
>
> 上位设计：[后端架构](../backend-architecture.md) · [开发计划](../backend-development-plan.md)

## 1. 职责与边界

Application 模块是进程边界和依赖装配入口，负责把 Config、Runtime 和 Observability 组合成可运行服务。

它负责：

- 解析进程级启动参数并定位配置文件；
- 建立最小 bootstrap 日志，再初始化正式观测组件；
- 调用配置加载和 runtime prepare；
- 把进程信号转换为统一 shutdown 请求；
- 在运行期间轮询配置文件 fingerprint，并把稳定变更交给统一 reload 入口；
- 将结构化错误映射为面向操作者的消息和稳定退出码；
- 保证所有服务任务都交由 Runtime supervisor 管理。

它不负责：

- listener accept loop、任务重启和 drain；这些属于 Runtime；
- DNS 解析、策略、缓存或上游选择；
- 自行读写 SQLite、资源文件或远程 URL；
- 在 CLI 层重新解释配置默认值或继承规则。

`app.rs` 只编排用例，`runtime/*` 持有长期运行状态。这样避免 application 与 supervisor 同时拥有 task。

## 2. 进程入口

`main.rs` 保持薄层：

1. 读取命令行和进程环境；
2. 初始化只写 stderr 的 bootstrap subscriber；
3. 创建 Tokio multi-thread runtime；
4. 调用 `app::run`；
5. 输出一条脱敏后的最终错误；
6. 返回对应退出码。

panic hook 只记录 panic 分类、位置和 backtrace 是否可用，不记录配置内容、DNS wire 或 secret。panic 仍由进程边界终止，不能被伪装为普通成功退出。

## 3. 启动输入与命令边界

首个可运行切片只要求“启动服务”和“只读校验配置”两类用例。具体 CLI 拼写在实现 Application 时固定，但必须满足：

- 配置路径可以显式指定；
- 未指定时只使用文档约定的默认 `config.yaml`，不遍历目录猜测；
- 校验用例停在 bind 之前，不创建对外 listener；
- 任何会覆盖、迁移或回滚配置文件的命令在拥有独立交互契约前不启用；
- stdout 用于机器可读或明确请求的结果，诊断默认写 stderr。

Application 将原始路径交给 Config 模块；路径归一化、工作目录和配置快照复制不在 CLI 层实现。

## 4. 装配流程

启动顺序固定为：

```text
bootstrap telemetry
  → load/migrate/resolve/validate config
  → initialize final telemetry
  → prepare runtime candidate (restore/fetch remote resources)
  → bind all endpoints
  → activate runtime
  → wait for supervisor, config change or shutdown signal
  → graceful shutdown
```

当前实现以进程级 bootstrap subscriber 作为启动底座，并在严格配置/SecretRef 校验后切换正式日志文件、stderr 或关闭目标及 reloadable level filter；stats/detail flush 已接入 `DnsService` drain，cache persistence 与 telemetry flush 仍待统一生命周期接线。配置加载早期错误和服务生命周期事件均保持结构化、脱敏输出。DoH 首轮装配 plain HTTP 或 TLS terminate，支持 forwarded header/PROXY 首轮客户端地址恢复；PROXY 前导会在连接升级 TLS 前消费。HTTP/2、nginx 集成和特权端口仍未做端到端 smoke。

依赖装配使用显式 constructor/build step，不使用全局 mutable singleton。正式 `run` 通过 async `PreparedRuntime` 在 bind 前完成 remote rule-set restore-or-fetch，创建 `Arc<RuntimeCoordinator>` 并交给 `DnsService`；service Supervisor 持有自动刷新 task，资源 task 通过 coordinator 查询当前活动 runtime，并以 scoped token 管理 reload 生命周期，transport task 在启动时绑定一份 runtime，显式 service reload 时切换到新 revision。测试通过 fake ports 注入 clock、socket、fetcher、storage 和 telemetry。

`reload_runtime_from_path` 复用同一 Config → async prepare → bind → revision CAS 边界，读取配置时关闭 snapshot 写入，SecretRef 校验和候选失败均不会改变当前 `ActiveRuntime`。`reload_service_from_path` 则在同一 prepare 边界后调用 `DnsService::reload_prepared`：BindPlan 变化时为新 revision 绑定 listener，BindPlan 未变化时复用已激活 listener，再重建 UDP/TCP/DoH adapter 和 resource refresh task，并取消旧 scoped token。`run` 以配置文件的 metadata fingerprint 做两次稳定轮询后触发该 service-aware 入口；成功才提交 fingerprint，坏配置或 bind 失败会记录并保留旧 Runtime，下一轮继续重试。

## 5. 信号与退出

当前实现处理 `SIGINT` 或运行期 Supervisor task 终止：

1. 通过 `Supervisor` cancellation 停止 accept/receive；
2. 先把 `ActiveRuntime` 标记为 draining，拒绝新请求 admission；
3. 在固定 5 秒 grace deadline 内回收 UDP loop、TCP listener、DoH listener 和连接 session；
4. 在同一 grace deadline 内关闭 `RuntimeCoordinator` 持有的历史/当前 `LateCacheFinalizer` owner；
5. 返回成功或 shutdown timeout 错误。

运行期 task 完成时，Degraded 组件的终止失败只记录并继续服务；FatalEndpoint/Fatal、重试耗尽和 panic 映射为 `RuntimeFatal`，先标记当前 runtime draining，再交由进程边界返回非零错误。显式 service reload 会为新 revision 注册新的 listener task，并通过 scoped cancellation 取消旧 task；运行期故障本身仍不会自动重建 listener。

第二个终止信号快速退出、cache persistence/telemetry flush 和 `SIGTERM` 专用处理仍未实现。配置文件轮询只作为内部事件源，不提供外部管理 API。

建议退出码分类：

| 退出码 | 分类 |
| ---: | --- |
| 0 | 正常结束或只读校验成功 |
| 2 | CLI 参数或配置语法/语义错误 |
| 3 | prepare 失败，包括资源、SecretRef、数据库或 migration |
| 4 | bind 或启动期 runtime 失败 |
| 5 | 运行期 fatal、不可恢复 task panic 或 shutdown timeout |

退出码只表达大类；详细原因由结构化错误链提供。

## 6. 错误呈现

Application 将内部错误转换为：

- 稳定错误分类；
- 发生阶段；
- 配置字段路径或组件名称；
- 可安全展示的上下文；
- 原因链的单行摘要。

不得展示 SecretRef 实际值、完整代理 URL、证书私钥内容、raw DNS message、完整 client ID 或原始请求 URL。

## 7. 测试与验收

- 配置错误、prepare 错误、bind 错误和运行期 fatal 映射到正确退出码；
- bootstrap 日志和正式日志切换时不丢失 fatal 诊断；
- 首次信号进入优雅停机，第二次信号触发快速终止；
- 校验用例不绑定端口、不启动后台刷新；
- dependency fake 能证明 Application 没有绕过 Config/Runtime port；
- panic 和 shutdown timeout 返回非零状态。

## 8. 实现检查清单

- [x] 创建 `main.rs` 与 `app.rs`；
- [x] 建立进程级错误与退出码映射；
- [x] 接入 bootstrap telemetry 初始化；
- [x] 接入读取配置后的 Config load/resolve/preflight 边界；
- [x] 接入 Runtime bind、activate、wait、shutdown；
- [x] 提供配置文件 reload 的 prepare/bind/activate 触发 API，并验证失败保留旧 runtime；
- [x] 提供 service-aware reload 的 listener task 重建 API，并验证新 revision 接管新端口；
- [x] 观察 Supervisor task 完成并按 fault level 映射运行期服务错误；
- [ ] 完成信号与退出测试；
- [x] 记录阶段 1 验证证据并更新实现进度。

阶段证据：`app::tests::exit_codes_are_stable`、CLI 参数、`validate` 只读和 `reload_runtime_from_path` 成功/失败测试通过；`service::tests::reload_prepared_rebinds_listener_tasks_to_the_new_runtime` 以及 `service::tests::reload_prepared_reconciles_resource_worker_tokens` 通过系统 loopback socket 验证新 revision 接管新端口、旧 listener/resource token 取消且 service runtime 更新；正式 `run` 已切换到 async remote prepare，`runtime::prepared::tests` 验证首次 fetch、第二次 fallback restore 以及 refresh worker 的 Policy live publish。真实 smoke 使用临时配置在 UDP `8353`、TCP `8354`、DoH `8355` 启动，hosts 查询返回 `127.0.0.1`，同连接双 TCP frame 维持 ID 顺序，DoH GET/POST 保留 DNS ID/RCODE，`SIGINT` 后输出 `service_shutdown` 并以 0 退出；正式日志目标/级别切换、forwarded header 和 TLS loopback 定向测试已通过。未测试 nginx、HTTP/2 或特权端口。

当前实现进度：**55%**。

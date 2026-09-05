# Application 模块设计

> 文档状态：有效
>
> 适用范围：进程入口、依赖装配、信号、退出和服务生命周期
>
> 最后评审：2026-09-05（安全 panic hook 与已接受的快速取消停机契约；基线见[模块索引](README.md)，证据见[生命周期](../../../implementation/backend/lifecycle.md)）
>
> 关联实现：[main.rs](../../../../backend/src/main.rs)、[app.rs](../../../../backend/src/app.rs)、[service.rs](../../../../backend/src/service.rs)
>
> 关联文档：[后端架构](../overview.md)

## 1. 职责与边界

Application 模块是进程边界和依赖装配入口，负责把 Config、Runtime 和 Observability 组合成可运行服务。

它负责：

- 解析进程级启动参数并定位配置文件；
- 建立最小 bootstrap 日志，再初始化正式观测组件；
- 调用配置加载和 runtime prepare；
- 把进程信号转换为统一 shutdown 请求；
- 在运行期间轮询配置文件 fingerprint，并把稳定变更交给统一 reload 入口；
- 将结构化错误映射为面向操作者的消息和稳定退出码；
- 通过 `DnsService` 装配 Supervisor 与进程级服务 owner，保证运行任务有明确的关闭边界。

它不负责：

- listener accept loop、任务重启和 drain；这些属于 Runtime；
- DNS 解析、策略、缓存或上游选择；
- 自行读写 SQLite、资源文件或远程 URL；
- 在 CLI 层重新解释配置默认值或继承规则。

`app.rs` 编排用例，`runtime/*` 管理候选、活动实例与监督原语，`service.rs` 的 `DnsService` 持有实际 Supervisor、transport/resource task 注册信息及 Storage/Resolution/Telemetry/Management。这些服务的内部 worker 可以由各自 owner 持有，不要求每个 task 都直接登记到 Supervisor。

## 2. 进程入口

`main.rs` 保持薄层：

1. 由 `#[tokio::main(flavor = "multi_thread")]` 建立 Tokio runtime；
2. 在异步 `main` 中先安装安全 panic hook，再初始化只写 stderr 的 bootstrap subscriber；
3. 调用 `app::run`，由 Application 读取和解析命令行；
4. 执行 `run`、`validate` 或 help/version 输出；
5. 输出一条脱敏后的最终错误；
6. 返回对应退出码。

[panic_safety.rs](../../../../backend/src/panic_safety.rs) 的进程 hook 只向 stderr 输出固定 panic 分类、经限制的源码文件名/行列和 backtrace 可用状态，不输出 panic payload、线程名、路径或栈内容。它不依赖可能正在失败的 telemetry writer，也不改变 unwind、owner 回收或既有 fatal 升级策略。安装点位于 Tokio runtime 构建后的异步 `main`，不覆盖此前的 runtime 构建失败；进程致命 panic 不得被伪装为成功。

## 3. 启动输入与命令边界

Application 提供“启动服务”和“只读校验配置”两类用例，必须满足：

- 配置路径可以显式指定；
- 未指定时只使用文档约定的默认 `config.yaml`，不遍历目录猜测；
- `validate` 只走严格配置加载，关闭 snapshot 写入，不恢复 ConfigStore journal，也不解析 SecretRef 实际值、不准备资源、不打开数据库或绑定 listener；
- 任何会覆盖、迁移或回滚配置文件的命令在拥有独立交互契约前不启用；
- stdout 用于机器可读或明确请求的结果，诊断默认写 stderr。

Application 将原始路径交给 Config 模块；路径归一化、工作目录和配置快照复制不在 CLI 层实现。

## 4. 装配流程

装配依赖按以下边界组织；实际函数调用顺序见[生命周期实现](../../../implementation/backend/lifecycle.md)，本图不代表每一步都由同一个类型负责：

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

依赖装配使用显式 constructor/build step，不使用全局 mutable singleton。资源 task 通过 coordinator 查询当前活动 runtime，以 scoped token 管理 reload；transport task 固定持有对应 runtime，显式 reload 才切换。Storage/Telemetry 按进程持有，候选 core 复用其 sink，不因 revision 重建。

文件 reload 关闭 snapshot 写入，完成严格加载、SecretRef 检查和 prepare 后再 bind/CAS；失败保留旧 ActiveRuntime。按 BindPlan 复用或重绑 listener，成功后才更新 watcher fingerprint、注册新 task 并取消旧 token。进程配置的 restart-required 边界由[配置参考](../../../implementation/configuration.md)定义；用户变更与内部写入识别遵循[Management 设计](../../management.md)。

## 5. 信号与退出

`SIGINT`/Unix `SIGTERM` 或运行期 Supervisor 致命任务终止应进入同一有界停机流程：

1. 撤销 WebUI session，并先取消 Management accept；
2. `RuntimeCoordinator::begin_drain` 标记当前及历史 runtime，再取消 transport/resource task 和 Supervisor；
3. 在固定 5 秒 grace deadline 内回收 UDP loop、TCP listener、DoH listener 和连接 session；
4. 由 `RuntimeCoordinator` 在同一 grace deadline 内等待当前及旧 Runtime 的 request guard drain；
5. 停止并排空进程级 Resolution 管线，再关闭历史/当前 `LateCacheFinalizer` 及缓存持久化 owner；
6. 关闭 Storage，最后 flush/shutdown Telemetry，返回阶段报告或失败。

运行期 task 完成时，Degraded 组件可以记录失败后继续服务；endpoint 耗尽先按逻辑 listener 的可用 sibling 聚合，致命升级和 task panic 进入统一 drain 并返回非零错误。显式 reload 与瞬时 task 重试不能形成两套竞争的 listener ownership。

第二个终止信号结束剩余等待并返回非零错误。各阶段共用同一个 5 秒总 deadline，不是每阶段重新获得 5 秒。已确认停机请求尽快取消，TCP/DoH/UDP dispatch 可以被 cancellation 中止，不保证已读完整请求一定写回，不新增响应优先 drain。Storage 按统计优先、详情使用剩余时间关闭，见 [Storage](storage.md)。配置文件轮询只是内部事件源，不构成外部管理写 API。实际接线和 Unix smoke 边界见[生命周期实现](../../../implementation/backend/lifecycle.md)。

退出码分类契约：

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

## 7. 契约验证要求

- 配置错误、prepare 错误、bind 错误和运行期 fatal 映射到正确退出码；
- bootstrap 日志和正式日志切换时不丢失 fatal 诊断；
- 首次信号进入优雅停机，第二次信号触发快速终止；
- 校验用例不绑定端口、不启动后台刷新；
- dependency fake 能证明 Application 没有绕过 Config/Runtime port；
- panic 和 shutdown timeout 返回非零状态。

# Application 模块设计

> 状态：v1 方案已完成，代码未实现
>
> 更新日期：2026-08-30
>
> 目标代码：`src/main.rs`、`src/app.rs`
>
> 上位设计：[后端架构](../backend-architecture.md) · [开发计划](../backend-development-plan.md)

## 1. 职责与边界

Application 模块是进程边界和依赖装配入口，负责把 Config、Runtime 和 Observability 组合成可运行服务。

它负责：

- 解析进程级启动参数并定位配置文件；
- 建立最小 bootstrap 日志，再初始化正式观测组件；
- 调用配置加载和 runtime prepare；
- 把进程信号转换为统一 shutdown 请求；
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
  → prepare runtime candidate
  → bind all endpoints
  → activate runtime
  → wait for supervisor or shutdown signal
  → graceful shutdown
```

正式日志初始化需要 `ResolvedConfig.logs`，因此配置加载早期错误使用 bootstrap stderr；正式 subscriber 就绪后，后续事件进入配置的日志目的地。

依赖装配使用显式 constructor/build step，不使用全局 mutable singleton。测试通过 fake ports 注入 clock、socket、fetcher、storage 和 telemetry。

## 5. 信号与退出

Unix 至少处理 `SIGINT` 和 `SIGTERM`，其他平台映射为等价的控制台终止事件。收到第一个信号时：

1. 触发全局 `CancellationToken`；
2. 请求 Runtime 停止接收新连接；
3. 等待 grace deadline；
4. flush stats、resolve log、cache persistence 和日志；
5. 返回成功或 shutdown timeout 错误。

第二个终止信号可缩短等待并进入强制退出，但仍应尝试输出一次结构化原因。

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

- [ ] 创建 `main.rs` 与 `app.rs`；
- [ ] 建立进程级错误与退出码映射；
- [ ] 接入 bootstrap/final telemetry 两阶段初始化；
- [ ] 接入 Runtime prepare、activate、wait、shutdown；
- [ ] 完成信号与退出测试；
- [ ] 更新本模块实现进度。

当前实现进度：**0%**。

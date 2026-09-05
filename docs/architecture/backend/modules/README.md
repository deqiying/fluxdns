# 后端模块设计

> 文档状态：有效
>
> 适用范围：12 个后端模块的稳定设计入口

| 模块 | 设计职责 | 当前接线入口 |
| --- | --- | --- |
| [Application](application.md) | CLI、装配、信号与退出 | [生命周期](../../../implementation/backend/lifecycle.md) |
| [Config](config.md) | 严格解析、归一化、迁移和安全写入 | [配置](../../../implementation/configuration.md) |
| [Ports](ports.md) | 可替换边界、deadline 与失败类型 | [DNS 管线](../../../implementation/backend/dns-pipeline.md) |
| [DNS Core](dns-core.md) | canonical message、决策和响应语义 | [DNS 管线](../../../implementation/backend/dns-pipeline.md) |
| [Policy](policy.md) | client、strategy、route 与继承 | [DNS 管线](../../../implementation/backend/dns-pipeline.md) |
| [Cache](cache.md) | key、准入、CAS、refresh 和恢复 | [DNS 管线](../../../implementation/backend/dns-pipeline.md) |
| [Upstream](upstream.md) | connector、选择器和 fallback | [DNS 管线](../../../implementation/backend/dns-pipeline.md) |
| [Transport](transport.md) | UDP/TCP/DoH 接入与编码 | [DNS 管线](../../../implementation/backend/dns-pipeline.md) |
| [Resource](resource.md) | 解析、snapshot、刷新和发布 | [后台服务](../../../implementation/backend/background-services.md) |
| [Runtime](runtime.md) | prepare、bind/CAS、supervisor 与 drain | [生命周期](../../../implementation/backend/lifecycle.md) |
| [Storage](storage.md) | 聚合、详情、ledger 和数据库边界 | [后台服务](../../../implementation/backend/background-services.md) |
| [Observability](observability.md) | 日志、指标、health 和背压 | [后台服务](../../../implementation/backend/background-services.md) |

模块设计保留独立价值，不机械派生 12 份实现文档。内部结构示例表达职责分解，不保证与源码目录逐项一致；真实路径以实现文档为入口。迁移未等同整篇契约重新验收，具体待核对项见[活动计划](../../../plans/backend-contract-gaps.md)。

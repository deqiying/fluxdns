# 后端模块设计

> 文档状态：有效
>
> 适用范围：12 个后端模块的稳定设计入口
>
> 最后核对：2026-09-05（全部模块的职责、关键类型、正式接线与主要行为）
>
> 代码基线：`8223d819efb83fed642900e6b121825083e8c1dd`（核对开始时工作树干净；本轮仅改文档）

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

本轮以当前源码为事实基准，直接修订已有模块正文，而非仅刷新日期。各模块“关联实现”提供核对入口；其中类型、算法、owner 和支持范围描述当前代码，不将未接线的原语或旧设计草图当成正式能力。

模块设计保留职责、不变量与取舍，完整字段和运行链路继续由 configuration/implementation 维护，不机械派生 12 份实现文档。尚未落实的旧要求与真实运行验收分开登记在[差距计划](../../../plans/backend-contract-gaps.md)；文档对齐不等于批准代码变更，也不等于接受全部历史实现偏差。

本轮只进行源码、schema、构造调用和测试定义的静态核对，未运行 Cargo、DNS smoke、故障注入或压力测试。各文档末尾的“契约验证要求”是后续检查范围，不是本轮通过记录。

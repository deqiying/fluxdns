# 后端模块设计

> 文档状态：有效
>
> 适用范围：12 个后端模块的稳定设计入口
>
> 最后核对：2026-09-05（全部模块的职责、关键类型、正式接线与主要行为）
>
> 初始全模块审计基线：`8223d819efb83fed642900e6b121825083e8c1dd`
>
> 本次契约实施基线：`19c3c81e4fdbea9424d522620ad81462c6d22eb1` 加工作树，更新 Application、Cache、Upstream、Resource、Storage、Observability 的获批行为，并同步 Ports/Runtime 相关摘要；其他模块不宣称重新完整审计

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

模块设计保留职责、不变量与取舍，完整字段和运行链路继续由 configuration/implementation 维护，不机械派生 12 份实现文档。已验收 D1-D9 保留配置语义与异步主链；后续环境、组合及长期负载证据由[契约验证开发计划](../../../plans/backend-contract-validation.md)独立跟踪，不以文档对齐替代运行证据。

各文档末尾的“契约验证要求”仍是完整检查范围，不是全部通过记录。本次 Cargo、SQLite、loopback 与手动 profile 的实际结果统一见[后台服务验证](../../../implementation/backend/background-services.md#本次验证)；未将本机测试外推为远程、Unix 或长期压力验收。

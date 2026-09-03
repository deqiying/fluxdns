# 后端模块文档索引

> 文档状态：有效
>
> 适用范围：`backend/src/` 各模块的设计、实现边界和模块级验证导航

本目录保存后端模块级文档。跨模块架构由[后端架构设计](../architecture.md)维护，总体阶段和进度由[后端开发计划](../development-plan.md)维护，配置公共契约由[配置字段参考](../configuration-reference.md)维护。

## 模块路由

| 模块文档 | 主要职责 |
| --- | --- |
| [Application](application.md) | 进程入口、装配、信号、退出和服务生命周期 |
| [Config](config.md) | 配置加载、迁移、归一化、校验和安全快照 |
| [Transport](transport.md) | UDP、TCP、DoH、TLS、client IP 和响应编码 |
| [Ports](ports.md) | 核心与 adapter 之间的稳定接口和 contract test |
| [DNS Core](dns-core.md) | canonical message、请求管线、缓存和上游结果处理 |
| [Policy](policy.md) | client、strategy、rule 和 `ResolutionPlan` |
| [Upstream](upstream.md) | connector、bootstrap、outbound、group 和故障回退 |
| [Cache](cache.md) | key、TTL、single-flight、memory 和 persistence |
| [Resource](resource.md) | hosts/rule 解析、加载、snapshot 和刷新发布 |
| [Runtime](runtime.md) | prepared/active runtime、CAS、supervisor 和 shutdown |
| [Storage](storage.md) | SQLite、统计、解析记录、migration 和淘汰 |
| [Observability](observability.md) | tracing、metrics、health、脱敏和 backpressure |

## 维护要求

- 模块文档完整描述本模块职责、边界、失败语义和模块级验证，不重复总体开发进度或跨模块架构。
- 新增、重命名或删除后端模块文档时，同步更新本索引和后端架构中的模块映射；只有一级路由或目录职责变化时才更新[文档总索引](../../README.md)。
- 模块行为变化时，在同一变更中更新对应文档；影响跨模块契约、配置契约或总体进度时，再同步更新相应权威文档。
- 具体格式、状态和废弃流程遵循[文档维护规范](../../standards/documentation-maintenance.md)。

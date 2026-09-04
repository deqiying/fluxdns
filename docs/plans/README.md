# FluxDNS 方案文档索引

> 文档状态：有效
>
> 适用范围：跨领域、尚未完成实施的技术方案导航

本目录只保存需要在仓库内评审、且尚未完成实施的跨领域方案。方案完成后，应将稳定事实迁移到对应权威文档；没有继续保留价值的实施方案按[文档维护规范](../standards/documentation-maintenance.md)删除，不在本目录建立归档区。

## 当前方案

| 方案 | 文档状态 | 实现状态 | 范围 |
| --- | --- | --- | --- |
| [DNS 查询主链异步观测重构方案](dns-query-pipeline-async-observation.md) | 草案 | 未实现 | `2ms` cache-hit SLO、fast cache key v2、异步 cache commit、统一解析事件、聚合统计与 `resolve_log` 异步消费 |
| [FluxDNS v2 前后端整合与 WebUI Management Server 实施方案](webui-v2-management-integration.md) | 有效 | 部分实现，浏览器与 Linux 环境验收待执行 | management server、`/api/v1/*`、WebUI 静态资源内嵌、首次用户初始化与配置持久化 |

## 维护要求

- 本目录不是架构和配置事实的长期权威来源；已实现行为以对应的 `docs/backend/`、`docs/frontend/` 和 `docs/standards/` 文档为准。
- 方案状态、实现状态或适用范围变化时，应同步更新本索引。
- 新增、迁移、废弃或删除方案时，遵循[文档维护规范](../standards/documentation-maintenance.md)。

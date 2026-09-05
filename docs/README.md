# FluxDNS 文档

> 文档状态：有效
>
> 适用范围：文档分类、权威来源与阅读导航

文档先按用途分类，再按前后端领域细分。这里的“当前实现”是与代码同批维护的事实说明，不代表自动生成或已通过所有环境验收。

| 要回答的问题 | 入口 | 负责内容 |
| --- | --- | --- |
| 接下来改变什么、怎样验收？ | [plans](plans/README.md) | 活动方案、剩余任务、验收和退出条件 |
| 为什么这样设计、哪些约束不能破坏？ | [architecture](architecture/README.md) | 已接受的职责、依赖、不变量与取舍 |
| 代码现在怎样运行、证据到哪里？ | [implementation](implementation/README.md) | 实际入口、接线、数据流、能力和验证边界 |
| 修改和验证必须遵守什么？ | [rules](rules/README.md) | 文档、工具环境、本地测试等项目规则 |

## 权威来源

| 主题 | 唯一完整维护位置 |
| --- | --- |
| 系统边界 | [系统设计](architecture/system.md) |
| 后端跨模块设计 / 模块契约 | [后端总览](architecture/backend/overview.md) / [模块索引](architecture/backend/modules/README.md) |
| 前端分层 / Management 安全设计 | [前端设计](architecture/frontend.md) / [Management 设计](architecture/management.md) |
| 配置字段、默认值、路径和运行支持 | [配置参考](implementation/configuration.md)，与 [model](../backend/src/config/model.rs)、[resolve](../backend/src/config/resolve.rs)、[validate](../backend/src/config/validate.rs) 同步 |
| Management API 字段与状态码 | [OpenAPI](../frontend/openapi/management-api-v1.yaml)；[生成类型](../frontend/src/shared/api/generated.ts) 不人工维护 |
| 后端与前端实际接线 | [后端实现](implementation/backend/README.md) / [前端实现](implementation/frontend/README.md) |
| 构建、启动与发布脚本行为 | [交付实现](implementation/delivery.md)；版本与命令以 manifest 和脚本为准 |
| 文档分类、状态与维护流程 | [文档维护规则](rules/documentation-maintenance.md) |

源码、schema、配置和实际输出证明现状；架构记录接受的设计。两者冲突时记录差距并进入活动计划，不能靠改文档把缺陷变成契约。其他入口只给摘要和链接，不重复维护整套事实。

## 最短入口

- 使用与开发：[项目 README](../README.md)、[前端工程 README](../frontend/README.md)。
- 开始变更：[AGENTS.md](../AGENTS.md) 与对应领域文档。
- 临时日志、个人配置和运行记录放入 `_fluxdns/`，不进入长期文档；历史版本交由 Git 追溯。

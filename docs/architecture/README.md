# 架构设计

> 文档状态：有效
>
> 适用范围：已接受的系统与领域设计导航

本层回答职责、约束和取舍，不记录完成比例、历史测试数量或逐阶段工作清单。

| 文档 | 范围 |
| --- | --- |
| [系统设计](system.md) | DNS 数据面、Management 控制面、前端与交付边界 |
| [后端设计](backend/README.md) | 运行时所有权、核心管线和 12 个模块契约 |
| [Management 设计](management.md) | 认证、同源保护、只读查询和内嵌 SPA |
| [前端设计](frontend.md) | 分层、查询状态、路由与呈现约束 |

代码入口和能力限制见[当前实现](../implementation/README.md)。已验收 D1-D9 的后续组合、环境与长期负载验证见[后端契约验证开发计划](../plans/backend-contract-validation.md)；这些证据缺口不等同功能未接线，文档移动也不等于完成设计重审。

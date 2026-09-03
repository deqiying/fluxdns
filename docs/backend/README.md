# 后端文档索引

> 文档状态：有效
>
> 适用范围：`docs/backend/` 后端架构、配置、开发计划和模块文档导航

本目录集中维护 FluxDNS 后端长期技术文档。跨领域规范由[规范文档索引](../standards/README.md)统一路由，仓库级文档入口见[文档总索引](../README.md)。

## 文档路由

| 需要了解或修改的内容 | 权威文档 |
| --- | --- |
| 后端总体架构、跨模块边界和运行时契约 | [后端架构设计](architecture.md) |
| 后端阶段、总体进度、下一步和验收门槛 | [后端开发计划](development-plan.md) |
| 配置字段、路径、校验、迁移和示例语义 | [配置字段参考](configuration-reference.md) |
| 单模块职责、内部契约和模块级验证 | [后端模块文档索引](modules/README.md) |

## 目录职责

```text
backend/
├── README.md                    # 后端文档入口与职责路由
├── architecture.md              # 总体架构与跨模块契约
├── development-plan.md          # 阶段、进度与验收门槛
├── configuration-reference.md   # 配置公共契约
└── modules/                     # 模块级设计、实现边界与验证
```

后端文档只维护本表所列职责。同一事实由一个权威文档完整描述；其他文档保留必要摘要和相对链接。

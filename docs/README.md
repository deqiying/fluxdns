# FluxDNS 文档索引

> 文档状态：有效
>
> 适用范围：仓库级文档导航与权威边界说明

本目录保存 FluxDNS 的长期技术文档。新增、迁移、重命名或删除文档前，先阅读[文档维护规范](standards/documentation-maintenance.md)。项目协作入口仍为仓库根目录的 [`AGENTS.md`](../AGENTS.md)。

## 快速路由

| 需要了解或修改的内容 | 权威文档 |
| --- | --- |
| 后端总体架构、跨模块边界和运行时契约 | [后端架构设计](backend-architecture.md) |
| 后端阶段、总体进度、下一步和验收门槛 | [后端开发计划](backend-development-plan.md) |
| 后端单模块职责、内部契约和模块级验证 | [后端模块文档索引](backend-modules/README.md) |
| 配置字段、路径、校验、迁移和示例语义 | [配置字段参考](configuration-reference.md) |
| 前端总体架构、技术栈和实施边界 | [前端架构设计](frontend-architecture.md) |
| 跨模块开发与协作规范 | [规范文档索引](standards/README.md) |

## 目录职责

```text
docs/
├── README.md                    # 文档总入口和权威路由
├── backend-architecture.md      # 后端总体架构与跨模块契约
├── backend-development-plan.md  # 后端阶段、进度与验收
├── configuration-reference.md   # 配置公共契约
├── frontend-architecture.md     # 前端总体架构与实施边界
├── backend-modules/             # 后端模块级设计与实现边界
└── standards/                   # 跨模块、可执行的项目规范
```

各文档只维护本表所列职责范围。同一事实应由一个权威文档完整描述，其他位置只保留必要摘要和相对链接，避免复制后分别演进。

## 维护入口

- [文档维护规范](standards/documentation-maintenance.md)：文档选址、命名、状态、同步、验证和废弃规则。
- [本地测试规范](standards/local-testing.md)：本地文件、工具、构建物和测试结果记录规则。

新增、重命名或删除一级文档、子目录或索引项时，必须在同一变更中更新本索引；规范文档和模块文档还要分别更新各自目录的 `README.md`。

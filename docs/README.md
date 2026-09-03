# FluxDNS 文档索引

> 文档状态：有效
>
> 适用范围：仓库级文档导航与权威边界说明

本目录保存 FluxDNS 的长期技术文档。新增、迁移、重命名或删除文档前，先阅读[文档维护规范](standards/documentation-maintenance.md)。项目协作入口仍为仓库根目录的 [`AGENTS.md`](../AGENTS.md)。

## 快速路由

| 需要了解或修改的内容 | 权威文档 |
| --- | --- |
| 后端架构、配置、开发计划和模块文档 | [后端文档索引](backend/README.md) |
| 前端总体架构、技术栈和实施边界 | [前端文档索引](frontend/README.md) |
| 跨模块开发与协作规范 | [规范文档索引](standards/README.md) |

## 目录职责

```text
docs/
├── README.md                    # 文档总入口和一级领域路由
├── backend/                     # 后端架构、配置、计划和模块文档
├── frontend/                    # 前端架构和前端文档入口
├── standards/                   # 跨领域、可执行的项目规范
└── plans/                       # 按需创建；跨领域未实施方案
```

各文档只维护本表所列职责范围。同一事实应由一个权威文档完整描述，其他位置只保留必要摘要和相对链接，避免复制后分别演进。

本目录树与[文档维护规范](standards/documentation-maintenance.md)中的基准结构保持同步。新增一级目录必须先确认其职责不与现有目录重叠；`plans/` 仅在出现需要仓库内评审的未实施方案时按需创建，不保留空目录。

## 维护入口

- [文档维护规范](standards/documentation-maintenance.md)：文档目录结构、选址、命名、状态、同步、验证和废弃规则。
- [本地测试规范](standards/local-testing.md)：本地文件、工具、构建物和测试结果记录规则。

新增、重命名或删除一级目录或索引项时，必须在同一变更中更新本索引；领域、规范和模块文档还要更新最近一级目录的 `README.md`。

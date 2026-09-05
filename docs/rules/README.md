# 项目规则

> 文档状态：有效
>
> 适用范围：`docs/rules/` 规范文档导航与职责说明

本目录是协作与工程规则维护层，不存放 DNS `rule_set` 数据、产品架构或脚本实现流水账。项目级入口为 [`AGENTS.md`](../../AGENTS.md)，实际脚本行为见[交付实现](../implementation/delivery.md)。

## 文档路由

- [文档维护规则](documentation-maintenance.md)：四类文档权威、选址、命名、索引、分类状态、代码同步、方案退出和检查范围。
- [项目环境使用规范](environment-usage.md)：项目工具链、Node.js 与 pnpm 命令调用、构建物、依赖目录、缓存和工具安装边界。
- [本地测试规范](local-testing.md)：本地测试配置目录、运行时文件、DoH smoke test 和结果记录。

新增规范文档时，应使用清晰的英文文件名，补充本索引和根目录 `AGENTS.md` 的路由，并按[文档维护规范](documentation-maintenance.md)更新文档总索引或对应模块入口。

---
name: project-doc-maintenance
description: Maintain FluxDNS Markdown documents and indexes when adding, editing, moving, renaming, deprecating, or validating documentation; route plans, accepted design, current implementation, and project rules to their canonical sources.
metadata:
  short-description: FluxDNS 文档维护与目录治理
---

# FluxDNS 文档维护

本技能用于项目文档维护，不授权产品重构、工具安装、提交、推送或发布。

## 读取入口

1. 读取根 [AGENTS.md](../../../AGENTS.md) 与[文档总索引](../../../docs/README.md)。
2. 读取[文档维护规则](../../../docs/rules/documentation-maintenance.md)，由该规则决定分类、状态、证据与退出条件；不在本技能另建一套规范。
3. 通过索引进入受影响的计划、架构、实现或规则；涉及工具/运行时文件时同时读取环境和本地测试规则。

## 执行路径

- 用 `rg` 查找已有权威、入站引用和重复内容，再决定更新、拆分或新增。混合文档按段落职责拆分，不只换目录。
- 使用源码、schema、配置与真实命令核查现状；区分代码存在、正式接线和环境验收。发现设计/实现冲突时保留证据并明确剩余决策，不用文档修改掩盖缺陷。
- 修改正文后同步最近索引、必要的 AGENTS 路由和所有相对链接。只有核验对应范围才刷新日期/基线。
- 方案执行完成后，按维护规则将新逻辑沉淀到对应 implementation；若改变原有设计，同步更新对应 architecture，然后删除方案文档、索引项并修正入站引用。保留未完成的实施/验收任务，不自动归档或关闭其他计划。

## 验证与交付

本技能统一维护 [scripts/check-docs.ps1](scripts/check-docs.ps1)，不维护独立测试版本。从仓库根运行：

```powershell
pwsh -File .agents/skills/project-doc-maintenance/scripts/check-docs.ps1
git diff --check
```

检查器默认定位到所属仓库根目录，也可用 `-RootPath` 显式指定待检查仓库。修改检查器后重新执行检查，并核对受影响的语法边界。自动检查的支持范围和限制以维护规则为准，人工复核职责、源码依据与实际 diff。

交付说明新入口、保留的验收/设计差距、实际执行的验证与未执行项。保留无关工作树修改，不因文档任务自动安装、提交或执行产品/发布命令。

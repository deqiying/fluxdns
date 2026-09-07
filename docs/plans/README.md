# 活动计划

> 文档状态：有效
>
> 适用范围：尚需实施、决策或验收的独立变更

WebUI 重构从[开发总计划](webui-management-development-plan.md)进入：先确认共同契约和 P0-P5 顺序，再按前后端子计划开发，并按各自提交检查点分批验证和提交，不将整套重构合并成一次提交。

| 计划 | 文档状态 | 计划状态 | 剩余范围 |
| --- | --- | --- | --- |
| [WebUI 重构开发总计划](webui-management-development-plan.md) | 草案 | 待评审 | 共同决策、P0-P5 依赖顺序、跨端交接、联合验收及阶段性 Git 提交规则 |
| [WebUI 后端开发计划](webui-management-backend-development-plan.md) | 草案 | 待评审 | BE-01 至 BE-12 详细任务及 BC-01 至 BC-28 提交检查点；配置、身份、快照、分片、保留、管理接口、实时与迁移 |
| [WebUI 前端开发计划](webui-management-frontend-development-plan.md) | 草案 | 待评审 | FE-01 至 FE-12 详细任务及 FC-01 至 FC-15 提交检查点；12 模块、29 图稿、表单、实时交互与浏览器验收 |
| [WebUI 管理后台重构需求](webui-management-requirements.md) | 草案 | 待评审 | 确认 12 个一级模块及 29 张视觉草案；范围及图稿解释由本文维护，执行拆解见开发总计划 |
| [WebUI 配套后端重构方案](webui-management-backend-refactor.md) | 草案 | 待评审 | 原始 ID/IP＋最小匹配结果、保留 IP 策略匹配、独立缓存快照、统计/详情统一保留与日分片、管理接口及迁移验收 |

计划以问题、相对当前基线的变化、步骤、风险和退出条件为中心。长期设计和实现分别放入 [architecture](../architecture/README.md) 与 [implementation](../implementation/README.md)，不保留已完成的阶段清单或总体进度文档。

代码完成但必要验收未完成时保留“待验收”。方案执行完成后，新逻辑必须沉淀到对应 implementation；若改变原有设计，同步更新对应 architecture，然后删除方案文档与索引项，不建立 archive/history。取消方案只保留已实际发生的变更与有效决策，不把未实施内容写成现状。具体流程见[文档维护规则](../rules/documentation-maintenance.md)。

# 活动计划

> 文档状态：有效
>
> 适用范围：尚需实施、决策或验收的独立变更

WebUI 重构的 D-01 至 D-12 已在[决策清单](webui-management-decisions.md)记录确认结果。按[总计划](webui-management-development-plan.md)的 P0-P5 顺序推进，[配置专项](webui-management-config-runtime-plan.md)统一热更新与文件处理语义。共 51 个建议提交检查点，实施时分批验证、本地 commit，不 push；本轮仅重订方案。

| 计划 | 文档状态 | 计划状态 | 剩余范围 |
| --- | --- | --- | --- |
| [WebUI 重构决策清单](webui-management-decisions.md) | 有效 | 待实施 | 12 项决定已确认；跟踪 T-01 至 T-08 技术落实和阶段提交授权 |
| [WebUI 重构开发总计划](webui-management-development-plan.md) | 草案 | 待评审 | 重订 P0-P5、51 个检查点、跨端依赖和 Windows/2ms 验收 |
| [WebUI 配置热更新专项](webui-management-config-runtime-plan.md) | 草案 | 待评审 | 活动源、先应用后持久化、外部差异/还原/采用、热 owner、失败恢复；不另建提交编号 |
| [WebUI 后端开发计划](webui-management-backend-development-plan.md) | 草案 | 待评审 | BE-01 至 BE-12、BC-01 至 BC-32；热配置、身份、快照、分片、保留、实时、新基线与 Windows 验收 |
| [WebUI 前端开发计划](webui-management-frontend-development-plan.md) | 草案 | 待评审 | FE-01 至 FE-12、FC-01 至 FC-16；12 模块、29 图稿、外部差异工作区及真实交互 |
| [WebUI 管理后台重构需求](webui-management-requirements.md) | 草案 | 待评审 | 保留已审阅模块范围；已按决定校正 name、热配置和旧图标注解释 |
| [WebUI 配套后端重构方案](webui-management-backend-refactor.md) | 草案 | 待评审 | 身份、独立快照、统一保留与日分片；取消旧版迁移/兼容，接入热配置专项 |

计划以问题、相对当前基线的变化、步骤、风险和退出条件为中心。长期设计和实现分别放入 [architecture](../architecture/README.md) 与 [implementation](../implementation/README.md)，不保留已完成的阶段清单或总体进度文档。

代码完成但必要验收未完成时保留“待验收”。方案执行完成后，新逻辑必须沉淀到对应 implementation；若改变原有设计，同步更新对应 architecture，然后删除方案文档与索引项，不建立 archive/history。取消方案只保留已实际发生的变更与有效决策，不把未实施内容写成现状。具体流程见[文档维护规则](../rules/documentation-maintenance.md)。

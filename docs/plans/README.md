# 活动计划

> 文档状态：有效
>
> 适用范围：尚需实施、决策或验收的独立变更

WebUI 重构的 D-01 至 D-12 已在[决策清单](webui-management-decisions.md)记录确认结果。当前仅获准实施 P0，GC-01 已提交为 `007f943`；BC-01 分批验证、本地 commit，不 push、不自动进入 P1。后续按[总计划](webui-management-development-plan.md)顺序推进，[配置专项](webui-management-config-runtime-plan.md)统一热更新与文件处理语义。

| 计划 | 文档状态 | 计划状态 | 剩余范围 |
| --- | --- | --- | --- |
| [WebUI 重构决策清单](webui-management-decisions.md) | 有效 | 实施中 | 12 项决定已确认；P0 技术核定与剩余 T 项 |
| [WebUI 重构开发总计划](webui-management-development-plan.md) | 有效 | 实施中 | P0 契约实施；P1-P5 未授权启动 |
| [WebUI 配置热更新专项](webui-management-config-runtime-plan.md) | 草案 | 待评审 | 活动源、先应用后持久化、外部差异/还原/采用、热 owner、失败恢复；不另建提交编号 |
| [WebUI 后端开发计划](webui-management-backend-development-plan.md) | 有效 | 实施中 | BC-01 分单元实施；BC-26 待新 owner；其余检查点未执行 |
| [WebUI 前端开发计划](webui-management-frontend-development-plan.md) | 草案 | 待评审 | FE-01 至 FE-12、FC-01 至 FC-16；12 模块、29 图稿、外部差异工作区及真实交互 |
| [WebUI 管理后台重构需求](webui-management-requirements.md) | 草案 | 待评审 | 保留已审阅模块范围；已按决定校正 name、热配置和旧图标注解释 |
| [WebUI 配套后端重构方案](webui-management-backend-refactor.md) | 草案 | 待评审 | 身份、独立快照、统一保留与日分片；取消旧版迁移/兼容，接入热配置专项 |

计划以问题、相对当前基线的变化、步骤、风险和退出条件为中心。长期设计和实现分别放入 [architecture](../architecture/README.md) 与 [implementation](../implementation/README.md)，不保留已完成的阶段清单或总体进度文档。

代码完成但必要验收未完成时保留“待验收”。方案执行完成后，新逻辑必须沉淀到对应 implementation；若改变原有设计，同步更新对应 architecture，然后删除方案文档与索引项，不建立 archive/history。取消方案只保留已实际发生的变更与有效决策，不把未实施内容写成现状。具体流程见[文档维护规则](../rules/documentation-maintenance.md)。

# 活动计划

> 文档状态：有效
>
> 适用范围：尚需实施、决策或验收的独立变更

| 计划 | 文档状态 | 计划状态 | 剩余范围 |
| --- | --- | --- | --- |
| [后端契约差距核对](backend-contract-gaps.md) | 有效 | 待验收 | D1-D9 已按批准选择实施并同步正式文档；保留组合、真实故障、Unix 信号及长期压力验收 |
| [WebUI 管理后台重构需求](webui-management-requirements.md) | 草案 | 待评审 | 确认 12 个一级模块、配置编辑、实时推送需求与 3 张视觉草案；后续开展详细设计和前后端实施 |

计划以问题、相对当前基线的变化、步骤、风险和退出条件为中心。长期设计和实现分别放入 [architecture](../architecture/README.md) 与 [implementation](../implementation/README.md)，不保留已完成的阶段清单或总体进度文档。

代码完成但必要验收未完成时保留“待验收”。方案执行完成后，新逻辑必须沉淀到对应 implementation；若改变原有设计，同步更新对应 architecture，然后删除方案文档与索引项，不建立 archive/history。取消方案只保留已实际发生的变更与有效决策，不把未实施内容写成现状。具体流程见[文档维护规则](../rules/documentation-maintenance.md)。

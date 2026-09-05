# 后端契约差距核对计划

> 文档状态：草案
>
> 计划状态：待评审
>
> 适用范围：文档拆分时保留的既有设计差距和非 v2 后端验收缺口
>
> 代码基线：`0f18d5b2ddf67625121fd7e0662e21723362565f`

## 问题与依据

旧模块文档把设计、实现和阶段测试混在一起。移除完成清单时，仍须保留未实现约束和未验收范围。此计划是差距登记与后续评审入口，不是本次文档任务中的产品改动授权。

| 项目 | 既有要求 / 当前证据 | 下一步 |
| --- | --- | --- |
| 安全 panic hook | [Application](../architecture/backend/modules/application.md) 要求安全输出；本轮搜索源树未找到 `set_hook`，见[生命周期](../implementation/backend/lifecycle.md) | 确认需要的进程输出契约和测试，再实施 hook 或明确评审修订；不能宣称默认 hook 已满足 |
| 持久化缓存访问热度 | [Cache](../architecture/backend/modules/cache.md) 保留 last-access bucket/近似 LRU 设计；[SQLite/codec](../implementation/backend/background-services.md) 未写访问热度 | 评审写放大/队列与淘汰约束，明确是否实施原设计；不把现有写入顺序淘汰改称 LRU |
| late-window 完整生命周期 | 旧 Cache 清单明确仍有跨 revision/owner 场景缺口；已有 finalizer 不等于全矩阵通过 | 针对旧/新 runtime、取消、首响应/晚响应、producer lease 和 shutdown 建立可重现矩阵，区分代码缺陷与缺证据 |
| 真实数据库故障 | cache/storage 的 Busy、DiskFull hook 与 fake tests 不能证明 OS 真实磁盘耗尽/恢复 | 在批准的隔离介质执行 full/busy/权限/恢复，验证旧记录、重试、gap 和 DNS 不阻塞 |
| Adapter conformance | Ports/Upstream/Transport 的共享测试覆盖不等于完整 adapter 矩阵 | 明确 direct/bootstrap/connect_ip/SOCKS5/SOCKS5H、Host/SNI、TLS/PROXY、deadline/cancellation 组合与实际环境 |
| Runtime 并发与时间控制 | 旧 Runtime 清单保留并发、故障和时间控制测试要求 | 核对资源合并/候选 CAS、listener 复用、旧 task 迟到、fake clock、deadline 与取消矩阵，不仅验证单次 reload |
| Storage migration 与压力 | 旧 Storage 清单未关闭完整 migration、压力和故障测试 | 核对空库/旧库升级、幂等 ledger、跨午夜/late event、积压保护、软硬容量和恢复；区分已有测试与未验收场景 |
| DoH/TLS 安全与限额 | 旧 Transport 清单保留完整资源限制、安全和协议测试 | 覆盖 wire/header/GET/POST/session 上限、TLS/PROXY/forwarded 信任、错误分层与连接恢复，不用 plain HTTP 单项代替 |
| Unix 信号与压力 | Application 保留 SIGTERM/第二终止信号 smoke；旧 profile 只是本机 release loopback | 在 Unix 进程环境验信号；冻结目标硬件/QPS/并发/资源预算后执行长期压力，不复用旧百分比 |

## 目标与非目标

先对每一项形成“已实现但缺证据 / 明确未实现 / 需澄清设计”的判断，再批准最小实施或验收方案。不新增 DoT/DoQ、主动健康检查等未来能力，不顺手重构 DNS 模块。

旧模块中的固定 timeout、端口 contract 和测试要求仍保留在架构中；本表不是所有函数的审计清单。新发现的冲突需要补充源码证据，不能用“待核对”无限掩盖。

## 步骤、风险与退出

1. 对照表中设计、源码及现有测试，确认每项实际缺口和影响面。
2. 对改变代码或接受契约的选项进行评审；测试先使用项目现有工具/fake，真实环境不足时保留限制，不擅自安装。
3. 按批准范围实施与定向验证，同批更新 implementation；契约确实改变才更新 architecture。
4. 所有登记项有实现/验收证据或明确的范围决策后收口；确认新逻辑已沉淀到对应 implementation、改变的设计已同步到 architecture，再删除本计划与索引项。

风险是把缺验收写成未实现，或把已有缺陷通过文档改成合法行为。本轮仅完成静态登记，没有执行产品修复、故障注入或性能测试；[v2 专属验收](webui-v2-management-integration.md)独立管理。

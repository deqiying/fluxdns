# 后端契约差距核对计划

> 文档状态：有效
>
> 计划状态：待验收
>
> 适用范围：获批 D1-D9 的实施收口与剩余组合、环境验收
>
> 代码基线：`19c3c81e4fdbea9424d522620ad81462c6d22eb1` 加本次后端契约实施工作树
>
> 最后核对：2026-09-05，实际命令及结果见[后台服务验证](../implementation/backend/background-services.md#本次验证)

## 已确认边界

用户已批准 D1、D2、D3、D6、D8、D9 的建议方案，并明确 D4 的响应/收集/计数口径、D5 请求取消和 D7 统计优先顺序。本计划不再保留待选决策，正式行为已经沉淀到 architecture/implementation；仅保留未完成验收，不能因此标为全部完成。

1. 保留现有配置语义，`dns.cache.persistence.max_size_bytes` 仍是编码内容加 framing 的预算，不改变为 SQLite 主库/WAL/SHM 的物理硬配额。
2. 保留异步主链：请求完成后单次无等待移交，缓存提交、统计、请求指标和详情投影由后台处理；不把 SQLite I/O 或同步指标聚合移回响应路径。既有 bootstrap 地址缓存与后台采样不回退。
3. 不新增配置项、DoT/DoQ、主动健康检查、物理缓存配额、访问热度淘汰或完整 span/attempt 事件流；不为旧类型草图新增抽象。
4. 本轮不迁移业务 Storage 的旧时间字段类型。metadata 现为 Unix 毫秒字符串，缓存新增时间索引为纳秒整数；统一业务时间类型须另行明确迁移范围。

## 决策与交付入口

| 编号 | 已批准并落实的选择 | 权威说明 / 本地证据 |
| --- | --- | --- |
| D1 | 保留编码预算，不新增物理配额 | [配置参考](../implementation/configuration.md)、[Cache](../architecture/backend/modules/cache.md)；既有预算契约测试 |
| D2 | 按完整 key 增量 upsert；保留插入时间淘汰，不做 LRU | [SQLite cache](../../backend/src/cache/sqlite.rs)：v1 事务升级保留 payload、变更行 trigger、namespace 隔离、绝对插入年龄、失败回滚/坏行维护 |
| D3 | 支持条件请求；304 只复用已校验且身份/代际匹配的本地内容 | [Resource](../architecture/backend/modules/resource.md)；loopback 条件头、重复 304、pair 损坏/缺失、旧 manifest、URL/代际变化、同预算与取消重试 |
| D4-R | 等待已启动同阶段成员的更好答案；正常响应含 NXDOMAIN 阻止 fallback | [Upstream](../architecture/backend/modules/upstream.md)、[executor tests](../../backend/src/upstream/executor.rs)；sink 有无都择优，慢 primary 超时不触发 fallback，失败成员不遮蔽有效答案 |
| D4-P | Positive 提前响应后继续收集，保留 late-result 能力 | 正式 sink 仍将 JoinSet 移交受管 drain；nested group、真实 connector 与 late candidate 用例回归 |
| D4-L | 保留 primary lease 口径 | 不改为逐 attempt least-in-flight；原选择/重试契约回归 |
| D5 | 停机在途请求尽快取消 | [Application](../architecture/backend/modules/application.md)、[生命周期](../implementation/backend/lifecycle.md)；保留现有 cancellation 与总 grace，不保证请求写回 |
| D6 | 启动共享 deadline，加实际写入/回滚探针 | [Storage](../architecture/backend/modules/storage.md)；过期预算不建库、基础建表事务回滚、真实 SQLite 锁、写入拒绝/探针回滚 |
| D7 | 统计优先，详情只用剩余时间 | [StorageRuntime](../../backend/src/storage/service.rs)；停止接收后回收当前 batch，先 stats，再排空剩余详情；trigger 验证多批次顺序 |
| D8 | 复用完成事件，在后台聚合请求 histogram/outcome/cache status | [Observability](../architecture/backend/modules/observability.md)；固定 14 项加原 2 项采样、桶边界/溢出/拥塞/关闭详情/关闭 writer、配对 profile |
| D9 | 安全 panic hook，不改变既有失败升级策略 | [panic_safety.rs](../../backend/src/panic_safety.rs)；子进程验证 payload/线程名/栈不输出，保留 unwind |

## 剩余验收

以下不是“代码还没接线”。本地单测、真实 loopback、SQLite 锁与 trigger 只覆盖列明场景，不等价真实故障介质、全部组合或长期压力。

| 范围 | 已有基础 | 尚缺的证据 |
| --- | --- | --- |
| late-window / owner | [dns/policy.rs](../../backend/src/dns/policy.rs)、[executor.rs](../../backend/src/upstream/executor.rs)、[service.rs](../../backend/src/service.rs) 的当前/历史 finalizer、择优、late sink 用例 | 跨 revision、producer lease、首/晚响应、取消、sink 满和 shutdown 的完整组合矩阵 |
| 真实数据库故障 | Cache/Storage adapter 契约、真实 SQLite 写锁、trigger 写入拒绝及事务回滚 | 隔离介质下的真实 disk-full、权限/I/O 故障与恢复；核验旧数据、pending/ledger、gap、DNS 时延和资源占用，不以 hook 替代 |
| Adapter conformance | [ports/testing.rs](../../backend/src/ports/testing.rs)、UDP/TCP/DoH 与 HTTP/HTTPS/SOCKS5 loopback | direct/bootstrap/connect_ip/SOCKS5/SOCKS5H、Host/SNI、TLS/PROXY、deadline/cancellation 的完整组合与真实远程资源条件请求 |
| Runtime 并发与时间控制 | [coordinator.rs](../../backend/src/runtime/coordinator.rs)、[prepared.rs](../../backend/src/runtime/prepared.rs)、[supervisor.rs](../../backend/src/runtime/supervisor.rs) 的 CAS/rebind/retry/drain | Policy/metadata 分步发布、旧 task 迟到、资源刷新与 reload 竞争、内部 owner panic 的组合；FakeClock 不控制全部 Tokio timer |
| Storage migration 与压力 | [sqlite.rs](../../backend/src/storage/sqlite.rs) 的升级/重开/幂等、启动探针、统计先于多批详情 | 各旧版本、跨午夜/late event、积压保护、详情软硬容量与恢复的长时组合；SQL 已在执行且接近停机截止时的预算耗尽 |
| DoH/TLS 安全与限额 | [doh.rs](../../backend/src/transport/doh.rs)、[system_socket.rs](../../backend/src/runtime/system_socket.rs) 的 GET/POST/TLS/PROXY 用例 | 真实 TLS/forwarded 信任链、坏连接隔离、1,024 session 上限及长期连接恢复；不将 UDP 顺序执行等同通用 admission limiter |
| Unix 信号与长期负载 | SIGTERM/第二信号分支与本机短时配对 profile | Unix 真实进程双信号；冻结硬件、QPS、并发和预算后的长期 RSS/CPU/时延测试，不沿用旧性能百分比 |

## 验收边界与退出

- deadline 约束等待与后续步骤，不承诺强制中断已进入 OS/SQLite 的操作；启动失败不会创建可服务 owner。
- Storage 的统计优先发生在当前详情 SQL 回收之后，不承诺抢占事务、零丢失或每阶段重获完整预算。
- 缓存常规批写不再全量解码/重写 payload，但容量合计仍扫描索引；不以实现方式推断未经测量的性能收益。
- 条件资源刷新仍读取/解析本地内容；304 减少网络 body 与落盘写入，不等于零 I/O 或零编译成本。
- profile 为本机 debug 短时样本，不是生产 SLO、远程成功率或长期压力证明。

后续按上表补齐组合测试，并在具备已授权隔离环境时执行真实故障、Unix 与长期压力验收。必要环境不足时记录条件，不安装额外代理或操作共享/生产介质。每项取得证据或用户明确调整验收范围后，将最终证据沉淀到 implementation；确认接受的设计已经同步，再删除本计划与索引项，不建立 archive/history。

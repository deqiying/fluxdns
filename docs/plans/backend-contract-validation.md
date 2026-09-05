# 后端契约验证开发计划

> 文档状态：有效
>
> 计划状态：实施中
>
> 适用范围：已验收后端契约的组合测试、隔离环境验收、容量恢复与长期负载验证
>
> 代码基线：`749e1ec2f8e260ff7f43a598e27683d1a377af16`
>
> 最后核对：2026-09-05（UTC），用户已确认评审通过；从 `f65fb3f8bd68e1a40ca041d9a380859b44a3da0c` 的干净工作树开始实施

## 执行状态

本计划**尚未整体完成**。本轮已交付本机验证增量、显式连接驱动和可重复证据入口；三段状态按下表保留，不能用默认全量回归替代每包剩余矩阵。

| 工作包 | 测试/脚本实现 | 实际执行与断言 | 剩余范围/门槛 |
| --- | --- | --- | --- |
| V0 | 已建立测试编号、共享同步点及记录命令/指纹/失败的 runner | Windows 工具链、loopback、SQLite 可用；无 Git 既有脏改动 | 以下环境未提供，不假定存在获授权目标 |
| V1 | 分步发布、双资源 CAS、两类时钟、真实 service refresh/reload/rebind、当前/历史 finalizer 及三类后台 owner panic | 本机编号矩阵通过；复现并最小修复 reload activation 等锁超预算；失败候选不替换活动 runtime | 与真实远程刷新、Unix 信号的组合仍依赖 V7/V8；内部回收不等同于统一自动恢复 |
| V2 | nested group 中 primary 提交/丢弃 × 同代/换代 × late 前停机；独立 follower 取消、负答案后取消、publisher 拒收与 finalizer 终态 | 本机编号矩阵通过；复现并最小修复未 poll abort 的 finalizer active 泄漏 | 不宣称穷举所有因素排列；真实满载/介质故障导致的持续积压与联合恢复仍由 V5/V9 验收 |
| V3 | 真 TCP stream、HTTP/HTTPS × SOCKS 目标/Host/SNI/TLS 顺序、坏证书/body、TLS/头/body 与三段 SOCKS 共享预算；复用 bootstrap/拒绝 suite | 新增本地矩阵通过；不含远程服务 | 真实挂起拨号的在途取消及跨 OS 网络终态尚缺实测；截断 body 当前 Internal/不可重试仅记录现状，分类或策略调整须确认 |
| V4 | v1–v5 升级重开/失败回滚、UTC late event、事件保护、真实 SQL 三阶段 × 正常/超时停机、并发 flush 等锁 | 本机 SQLite/内存矩阵通过；复现并最小修复 stats flush 等锁超预算，显式恢复不重复统计 | 介质故障、持续容量/恢复和长时间联合负载仍由 V5/V9/V10 验收；不承诺强制抢占 SQL |
| V5 | 复用既有生产 adapter，未新增真实介质故障驱动 | 未执行，未验收 | 需确认可丢弃介质绝对路径、空间上限、故障方式、watchdog、恢复与授权；不得故障化当前个人目录 |
| V6 | V6-C01 显式容量驱动已实现；复用本地 TLS/PROXY/forwarded suite | TCP/plain DoH 1,023/1,024/超额等待、单槽释放、停机端口回收已实测 | 仍缺 external 代理环境、重复重连/reload 与慢握手/body 混合、OS 句柄趋势；不关闭 V6 |
| V7 | 复用已有 loopback 条件资源夹具；未新增真实远程专用驱动 | 既有 loopback suite 本机回归；远程未执行 | 需获准且可控制版本/响应的远程端点及请求证据，不用任意公网 URL 替代 |
| V8 | 生产 Unix 信号分支未改；真实子进程信号驱动待目标环境确定 | 当前仅 Windows；未执行 Unix 验收 | 需获授权 Unix 环境与进程工作目录，保证第二信号真实命中 drain |
| V9 | 新增三轮 SQLite trigger 故障/恢复与 pending 事件数保护的本地子集 | 局部正确性通过，不是持续负载验收 | V5/V6 前置、冻结周期以及 cache/连接/owner/详情联合恢复驱动待补 |
| V10 | 本计划保留设计参数表；未新增长期负载/性能对照驱动 | 未执行，未验收 | 先冻结构建、负载、时长、资源/性能阈值和安全停止条件；旧 profile 不能替代 |

具体用例、三处生产修复和证据分别沉淀在[生命周期](../implementation/backend/lifecycle.md#契约验证补充)、[DNS 管线](../implementation/backend/dns-pipeline.md#契约验证补充)及[后台服务](../implementation/backend/background-services.md#契约验证运行入口)。本轮 Local runner 的 27 项新增默认测试重复 3 次均通过，完整回归 669 通过、0 失败、3 忽略；V6-C01 另行显式重复 3 次通过，两组源码指纹一致。未达到第 9 节退出条件，因此保留本计划与索引，不进入“已完成”或删除。

## 1. 目标与现状

D1-D9 已由用户确认验收，正式契约保存在[后端设计](../architecture/backend/overview.md)及其模块文档中；业务时间整数迁移也已提交。本计划独立承接尚缺的验证证据，不重新实施已完成的功能，不因后续环境测试尚未运行而重新打开 D1-D9 的设计选择。

现状依据：

- [DNS 管线](../implementation/backend/dns-pipeline.md)记录生产 Policy、Upstream、Cache 与 late finalizer 的接线；已有单场景测试不能证明全部跨 revision、取消和 shutdown 组合。
- [生命周期](../implementation/backend/lifecycle.md)与[后台服务](../implementation/backend/background-services.md)记录 owner、共享 deadline、统计优先和既有本地验证；本地 SQLite 锁、trigger 与错误注入不能替代真实故障介质。
- [SQLite Storage](../../backend/src/storage/sqlite.rs)已有 v1 升级、v5→v6 数据/ledger/自增序列保留、异常时间回滚与重开用例；剩余任务是补齐升级起点和运行组合，不是新增一次时间迁移。
- [FakeClock](../../backend/src/ports/testing.rs)只推进自身 Clock 状态；[Supervisor](../../backend/src/runtime/supervisor.rs)仍有 Tokio retry timer，不能用一类时钟测试替代全部时间行为。
- [Service](../../backend/src/service.rs)已有真实信号分支及 TCP/DoH session 数量限制；边界函数测试不能证明真实连接满载、释放与恢复。

交付目标是建立可重复的测试、运行入口和证据。默认只补测试及必要夹具；仅在复现既定契约缺陷后做最小生产修复。测试实现、实际运行通过、目标环境验收是三个不同状态，必须分别记录。

## 2. 保留契约与非目标

1. 配置含义、默认值和路径解析不变；缓存持久化仍按编码内容与 framing 计费、按插入时间淘汰，不改为物理硬配额或 LRU。依据：[配置参考](../implementation/configuration.md)、[Cache](../architecture/backend/modules/cache.md)。
2. 保留单次无等待完成事件移交和后台缓存/统计/指标/详情处理，不把 SQLite I/O 或同步聚合移回响应主链；保留 bootstrap 地址缓存与后台采样。依据：[后台服务](../implementation/backend/background-services.md)。
3. 正常 terminal response 包括 NXDOMAIN，阻止启动 fallback；允许等待已启动同阶段成员的更好答案，Positive 首响应后保留生产 late-result 收集；负答案不额外续期，in-flight 保持 primary lease 口径。依据：[Upstream](../architecture/backend/modules/upstream.md)。
4. 停机尽快取消在途请求，不保证所有已读请求完成写回；启动/停机共享总 deadline。已进入 OS/SQLite 的操作不承诺被强制中断，统计优先也不意味着抢占当前详情事务。依据：[Runtime](../architecture/backend/modules/runtime.md)、[Storage](../architecture/backend/modules/storage.md)。
5. 业务绝对时间保留 schema v6 的非负 `INTEGER` UTC Unix 毫秒；缓存独立纳秒索引、耗时精度和 UTC 日桶各自不变，不补造历史字段，不新增 down migration。依据：[业务时间存储](../implementation/backend/background-services.md#业务时间存储)。
6. 条件请求只复用已校验且身份/代际匹配的本地内容；304 重试共享原预算。保留现有 panic 脱敏、错误分级和升级策略，不把 owner panic 自动恢复当作本计划目标。

不新增配置项、DoT/DoQ、主动健康检查、通用 admission limiter、完整逐 attempt span、持久审计 spool 或新的生产测试注册层。不顺手修改前端/API，不预先引入测试框架、系统代理、CI 服务或工具依赖。改变兼容性、资源预算或失败策略的修复须另行确认。

## 3. 拆分与推荐顺序

推荐按表顺序推进；“前置”是对应证据依赖，不要求无关工作包全部完成。每包可独立交付、评审和验收，共享夹具由 V0/V1 维护，不为每包复制一套环境或并发控制。

| 阶段 | 工作包 | 交付主题 | 前置 |
| --- | --- | --- | --- |
| 准备 | V0 | 用例清单、状态与证据约定、环境盘点 | 本计划获批 |
| A：本地确定性 | V1 | Runtime 并发与时间控制 | V0 |
| A：本地确定性 | V2 | late-window / owner 组合 | V1 的时序与 owner 夹具 |
| A：本地确定性 | V3 | Adapter 本地 conformance | V0；涉及 reload 的场景复用 V1 |
| A：本地确定性 | V4 | Storage 升级矩阵、统计与停机边界 | V0；并发场景复用 V1 |
| B：隔离环境 | V5 | Cache/Storage 真实数据库故障与恢复 | V4、获授权隔离介质 |
| B：隔离环境 | V6 | DoH/TLS 信任链、连接限额与恢复 | V3、可控 TLS/代理环境 |
| B：隔离环境 | V7 | 真实远程资源条件请求 | V3、可控远程端点 |
| B：隔离环境 | V8 | Unix 真实进程信号与退出 | V1/V2/V4 的生命周期边界、Unix 环境 |
| C：持续运行 | V9 | 积压保护、容量与连接的持续恢复 | V4/V5；连接部分依赖 V6 |
| C：持续运行 | V10 | 稳态、故障周期与长期性能对照 | V9、所测链路相关 A/B 项、冻结的负载与预算 |

原七类剩余范围完整映射如下，拆分不等于减少验收：

| 原范围 | 新工作包 |
| --- | --- |
| late-window / owner | V2 |
| 真实数据库故障 | V5 |
| Adapter conformance | V3，本地 TLS 信任链延伸至 V6，真实资源请求延伸至 V7 |
| Runtime 并发与时间控制 | V1 |
| Storage migration 与压力 | V4、V9 |
| DoH/TLS 安全与限额 | V6、V9 |
| Unix 信号与长期负载 | V8、V10 |

环境不足仅阻塞相应运行验收及其依赖项。例如，V5 缺隔离介质不阻塞 V3/V4；V8 缺 Unix 不阻塞本机确定性测试。V10 不能将缺少前置证据的场景算作已验收；缩小目标环境或场景范围须经明确确认并保留限制。

## 4. V0：验收基线与执行约定

先复用各模块现有测试和源码内嵌夹具，建立编号用例清单，不先建设通用测试框架。用例定义应随实际测试代码维护；本计划只管理剩余范围，正式结果沉淀到对应 implementation，原始输出留在本地专用目录。

每个用例至少明确：

- 既定契约、对应工作包、生产入口、前置状态、故障/竞争发生点与预期终态。
- 覆盖因素、支持/不支持的组合及其依据。每个列明因素和失败阶段都要覆盖；高风险交互必须有定向三因素或以上组合，不能仅用两两组合宣称“完整矩阵”。
- 触发和同步方式、操作 deadline、测试 watchdog、必要的重复次数与清理检查；不以调大 sleep 掩盖竞争失败。
- 三段进度：测试/脚本是否实现、适用环境是否执行、断言是否通过；失败和未执行分别记录原因，不能通过忽略测试消除缺口。
- 源码 commit、工具链、命令、配置与脱敏输入、时序/种子、结果、遗留问题，以及数据库/进程/task/句柄的清理结果。

Clock 与调度控制按实际等待点选择：已有 Clock 注入继续复用；消息、barrier 或通知控制发布/取消顺序；Tokio timer 的暂停/推进须先核对当前 feature 和真实 I/O 限制。必要测试开关仅位于测试边界，不增加生产配置或第二套 Runtime。读取真实数据库、跨进程信号和真实 I/O 故障必须保留对应运行证据。

完成条件：原七类范围均有可执行用例分解和归属；列清当前本机、Unix、远程、故障介质与负载环境是否可用。性能阈值尚未确定只阻塞阶段 C，不阻塞阶段 A。

## 5. 阶段 A：本地确定性测试

### V1：Runtime 并发与时间控制

源码入口：[coordinator.rs](../../backend/src/runtime/coordinator.rs)、[prepared.rs](../../backend/src/runtime/prepared.rs)、[supervisor.rs](../../backend/src/runtime/supervisor.rs)、[snapshot.rs](../../backend/src/runtime/snapshot.rs)、[ports/testing.rs](../../backend/src/ports/testing.rs)。

1. 枚举资源 Policy 发布、metadata 发布、revision CAS、rebind、drain、retry 和内部 owner 回收的线性化/等待点，补最少的测试同步能力。
2. 显式覆盖 Policy 成功后 metadata 尚未发布、旧刷新迟到、新 candidate 发布、两个不同资源竞争、失败候选不替换活动实例；验证分步发布的现有语义，不擅自改成跨对象原子事务。
3. 覆盖 reload 与刷新竞争、旧 task 结束/失败/取消及当前/历史 owner panic；分别验证 Supervisor 直接任务与内部 worker 的归属、升级及回收。
4. 将 retry、请求 deadline 和 shutdown budget 分别映射到实际时钟；同一 deadline 不因重试或阶段切换重置。

完成条件：无需碰运气的 sleep 即可触发目标竞争；旧结果不能覆盖不匹配的新代际，无永久 guard/lease/task 遗留；panic 仍脱敏、正确归因并按现有策略处理。缺陷先有复现用例，再做最小修复。

### V2：late-window / owner 组合

源码入口：[dns/policy.rs](../../backend/src/dns/policy.rs)、[upstream/executor.rs](../../backend/src/upstream/executor.rs)、[service.rs](../../backend/src/service.rs)及其现有 finalizer 用例。

1. 以当前/历史 revision、producer leader/follower、首响应/晚响应、Positive/正常负答案/失败、取消时点、sink 可用/满/关闭、运行/shutdown 为因素建立矩阵；加入 nested group。
2. 必测交互包括：负答案先到且同阶段 Positive 晚到；已有 NXDOMAIN 后另一 primary 超时而不启动 fallback；Positive 首响应后 reload 再收到 late result；sink 满时 producer 结束并唤醒 waiter；跨 revision 与 shutdown 同时发生。
3. 分别断言客户端响应只确定一次、late candidate 不改写已响应内容、缓存写回匹配 key/quality/producer 约束、primary lease 口径不变。
4. 覆盖成功、拒收、取消、失败和 drop 后的 lease/waiter 终态；当前与历史 finalizer 均受 owner 管理，在既有 deadline 下结束或报告未完成。

完成条件：没有悬挂 waiter、重复响应、跨代错误覆盖或脱管 drain；队列满/丢弃有对应计数。不能为通过测试关闭 late-result、延长预算或恢复“响应完成后才允许停机”。

### V3：Adapter 本地 conformance

源码入口：[ports/testing.rs](../../backend/src/ports/testing.rs)、[tokio_outbound.rs](../../backend/src/upstream/tokio_outbound.rs)、[bootstrap.rs](../../backend/src/upstream/bootstrap.rs)、[socks5.rs](../../backend/src/upstream/socks5.rs)、[reqwest_http.rs](../../backend/src/upstream/reqwest_http.rs)、[doh.rs](../../backend/src/upstream/doh.rs)、[system_socket.rs](../../backend/src/runtime/system_socket.rs)。

1. 先核对现有支持关系，分别列出入站 UDP/TCP/DoH 与上游 HTTP/HTTPS、direct、bootstrap、`connect_ip`、SOCKS5/SOCKS5H 的适用组合；不支持的组合验证既有拒绝行为，不实现新协议。
2. 复用真实 loopback server/dialer，按每类 adapter 可表达的契约复用断言；记录实际拨号地址、代理 CONNECT 地址类型、HTTP Host、TLS SNI 与校验证书身份，不能只检查配置值。
3. 覆盖本地/代理侧解析、显式 IP 与域名身份分离、TLS/PROXY 顺序、bootstrap 超时、拨号/握手/读取取消、EOF 和坏响应；这些阶段共享剩余预算。
4. 在测试中调用适用 conformance 断言，不增加运行期注册门禁。既有测试足够的场景只复用和记录，不重复维护两套 suite。

完成条件：支持矩阵每项有真实 adapter 证据或明确的拒绝断言；域名/IP/Host/SNI 不混用，错误分类与取消满足 port 契约。此包不宣称远程服务、外部 TLS 信任链或全部生产网络已验收。

### V4：Storage 升级、统计与停机边界

源码入口：[migrations](../../backend/migrations)、[sqlite.rs](../../backend/src/storage/sqlite.rs)、[backend_contract_tests.rs](../../backend/src/storage/backend_contract_tests.rs)、[statistics.rs](../../backend/src/storage/statistics.rs)、[stats.rs](../../backend/src/storage/stats.rs)、[service.rs](../../backend/src/storage/service.rs)。

1. 用历史 migration 构造新库及 v1-v5 各起点的可识别数据，升级至 v6 后重开；覆盖空库、含数据、历史 nullable 字段、删除后自增高水位、规范时间、异常时间与不支持的较新版本。旧 migration 不重写。
2. 对照全部目标字段、统计总数/维度、ledger hash/序号、详情 ID 和实际 INTEGER 时间类型；分别断言每步事务失败保留该步之前状态，不能误称整条升级链一次性回滚。
3. 用事件时间覆盖 UTC 午夜前后、乱序和 late event、epoch swap、同批重试；保证按事件所属日聚合且已提交批次不重复计数，不补造历史事实。
4. 覆盖详情批写、软/硬容量边界、清理顺序、pending/gap；在 SQL 未开始、已开始、完成待回收与停机截止接近时验证统计优先和总预算。

完成条件：各升级起点与异常路径有独立断言；统计幂等、时间单位、详情投影和主链耗时精度保持既定契约。SQL 已执行时的 deadline 耗尽必须真实报告，不以“可抢占事务”作为通过条件。长时容量与故障恢复另由 V5/V9 证明。

## 6. 阶段 B：隔离环境验收

### V5：真实数据库故障与恢复

在 V4 的断言基础上分别运行 Cache 与业务 Storage 的生产 adapter。执行前确认独立可丢弃介质、绝对目标路径、空间上限、恢复操作、watchdog 和授权，禁止填满系统盘、操作共享/生产数据库或故障化个人工作目录。

1. 真实 disk-full、权限失败、I/O 失败分别执行；覆盖启动建库/迁移/写探针和运行批写/提交/checkpoint 等适用阶段，并区分主库与 WAL 的失败位置。
2. 先写可识别基线，再制造故障，保留实际 OS/SQLite 错误、介质状态及失败阶段；只触发 hook、trigger 或 busy lock 不算相应介质故障通过。权限变更必须确认实际 I/O 已失败，不能只以权限命令成功为证据。
3. 故障期间检查旧数据、事务原子性、pending/ledger/gap、DNS 响应和时延、RSS/CPU/句柄；分别断言 Cache best-effort 降级与 Storage pending 保护/失败升级。
4. 恢复介质后验证重试或重启、数据一致性、已提交批次去重与资源释放。未达到保护阈值的恢复和达到阈值后的有限退出分开验证，不要求无限 pending 或零丢失。

完成条件：每种故障都有实际介质证据、恢复或按契约退出结果，数据库逻辑内容与完整性检查通过；受限环境不能复现的故障保持未验收。不得安装额外驱动/代理或扩大介质权限来绕过环境门槛。

### V6：DoH/TLS 信任链与真实连接限额

源码入口：[transport/doh.rs](../../backend/src/transport/doh.rs)、[system_socket.rs](../../backend/src/runtime/system_socket.rs)、[service.rs](../../backend/src/service.rs)。与 V3 共享证书、HTTP 和 PROXY 夹具，新增验证生产 listener 的持续连接行为。

1. 分别验证当前支持的 TLS 模式、受信/不受信 peer、forwarded 多跳链、伪造头、证书身份不匹配和 PROXY/TLS 读取顺序；GET/POST 同时检查 HTTP 与 DNS 层结果。
2. 无效 TLS、截断/畸形 HTTP、慢握手、慢 body 和单连接异常不得破坏其他有效连接；断言关闭范围、错误计数和仍可接收请求的能力。
3. 对单 TCP/DoH listener 实际占用 1,023、1,024 及更多连接，区分内核 backlog、已 accept session 与正在处理请求。验证现有 session 上限处暂停 accept，释放槽位后恢复，而不是把超过上限的 TCP connect 一律要求立即失败。
4. 在满载、释放、重复重连、reload 和 shutdown 期间检查 session task/句柄；不将该限制解释为跨 listener 总配额或 UDP 通用 admission limiter。

完成条件：错误信任链不能伪造有效来源，坏连接隔离有效，满载与释放恢复有真实连接证据。`external` 模式需要现有且获授权的代理环境；没有环境时记录缺口，不临时安装反向代理，不将该模式标为通过。

### V7：真实远程资源条件请求

源码入口：[fetcher.rs](../../backend/src/resource/fetcher.rs)、[remote.rs](../../backend/src/resource/remote.rs)、[prepared.rs](../../backend/src/runtime/prepared.rs)。使用已获准、可控制响应和版本的远程端点，不把任意公共 URL 的偶然 200/304 当作完整验收。

1. 首次 200 从响应取得 validator，后续条件请求分别取得内容不变的 304、内容变化的新 200；对照请求头、响应状态、内容 hash/身份、matcher 与本地内容/manifest pair。
2. 覆盖 pair 损坏/缺失、URL/adapter generation 变化、重复 304、无效新内容与取消后的重试；验证不误复用、不替换有效 snapshot、不重获 deadline。
3. 叠加资源刷新与 reload，复用 V1 的代际断言；故障后验证上次有效内容的保留或按既定启动契约失败。

完成条件：远程记录与本机发布结果能够对照，敏感头和地址不进入共享报告。只有 loopback 或环境无法控制所需响应时，仅记录已验证子集，不关闭远程矩阵。

### V8：Unix 真实进程信号与退出

复用 [Service](../../backend/src/service.rs) 的 `wait_for_termination_signal` 与 shutdown 顺序，在获授权 Unix 环境运行真实 binary，而不是只对 Cancellation 调用做单测。

1. 覆盖 SIGTERM、SIGINT，以及首信号触发 drain 后、grace 尚未结束时收到第二信号；受控延迟保证第二信号确实命中退出分支。
2. 用空闲、DNS 在途、late drain、详情写入和 pending stats 场景核验停止接收、尽快取消、统计/详情顺序及退出结果；区分正常退出、deadline 耗尽和第二信号错误退出。
3. 同时记录退出码、实际 elapsed、阶段报告和资源终态；核验监听端口释放、无遗留子进程、数据库可重开，未完成项不伪装为成功。

完成条件：证据包含 Unix 版本、真实进程 PID、发送信号顺序与时间、退出结果和清理检查；Windows 上同名辅助测试或人工终止按钮不能替代。测试 watchdog 的兜底 kill 单独记为失败/清理动作，不算正常 shutdown。

## 7. 阶段 C：持续恢复与长期负载

### V9：积压、容量与连接恢复

复用 V4/V5 的数据断言和 V6 的连接驱动，持续执行“稳态 → 增压/故障 → 解除 → 恢复/有限退出”。阈值从当前源码/配置读取并记录，不新增测试专用生产预算。

- Storage：组合统计 epoch、跨午夜/late event、批次去重、详情软/硬容量及年龄清理；分别验证 pending 批数/事件数的阈值前恢复和阈值触发后的升级、有限 flush/退出。
- Cache：在插入/淘汰、持久化写入失败、namespace 变化和 reload 中检查编码预算与恢复数据；物理主库/WAL/SHM 只作为观测值，不套用编码预算断言。
- 连接与 owner：重复满载/释放、reload/刷新、late candidate 与 shutdown，检查 session、guard、lease、队列、task 和句柄是否回到预期终态。
- 停机：压力下分别覆盖空闲 SQL、当前 SQL 接近截止、stats pending 和多批详情；报告谁消耗预算以及剩余详情，不要求事务抢占或重置 grace。

完成条件：每种适用场景至少经历评审冻结的多轮周期；无重复入账、无未解释的数据变化，无超出既有保护的持续队列增长。允许的 best-effort 丢弃必须对应可解释计数；达到 fatal 阈值后的有限退出是单独通过路径，不要求仍继续接收 DNS。

### V10：长期负载与性能对照

测试设计先完成，运行门槛后确认。不能从旧 debug 短时 profile 推导发布性能、SLO 或长期内存稳定性。

开始运行前冻结以下参数；任何缺项仅允许称为探索性测量，不能称为性能验收：

| 参数 | 必须记录的内容 |
| --- | --- |
| 版本与构建 | 源码 commit、Rust/依赖基线、release feature、配置指纹；对照版本与候选版本只改变本次目标差异 |
| 环境与隔离 | OS/内核、CPU/内存、文件系统/介质、进程与连接限制、网络位置、其他负载；确认负载发生器不是瓶颈 |
| 输入负载 | 各协议比例、QPS、并发、连接复用、查询集合、命中/未命中/负答案比例、详情开关、上游延迟/失败模式 |
| 时间安排 | 预热、稳态、故障/恢复周期、总时长、采样间隔和重复次数；建议从 10 分钟预热、30 分钟对照、4 小时持续运行及至少 3 次恢复周期起评审，不自动执行该时长 |
| 通过与停止阈值 | DNS 成功率及 p50/p95/p99、CPU、RSS 与增长趋势、句柄/task/队列、磁盘余量、允许退化幅度、恢复窗口和最大停机观察时间 |

1. 同一环境分别跑 Cache 命中、真实上游未命中、负答案、详情开/关以及受控混合流量；每类记录实际完成 QPS，成功、失败与超时分开统计数量和耗时，不将超时伪装为成功延迟，也不能只用成功样本概括整体服务表现。
2. 在持续负载中按固定计划叠加 reload、资源刷新、上游故障和数据库恢复，复用 V9 的正确性断言；不同故障源先单独再组合，确保可以归因。
3. 记录主链/端到端耗时、RSS/CPU、队列/owner/连接数、gap/drop、数据库/缓存物理占用及恢复轨迹；主链耗时不混入异步落库等待，端到端测量说明客户端排队边界。
4. 对照测试必须固定构建模式和输入，多轮结果报告分布与波动，不从单轮百分比给结论。没有业务 SLO 时只给同机对照结论，不声称已达到生产容量。

完成条件：冻结的正确性、资源、性能和恢复阈值全部满足，且没有未解释的持续资源增长；触发安全停止条件则本轮失败或未完成，保留证据。若修复了会影响时序/性能的代码，应重新冻结版本并重跑受影响的对照，不能拼接不同版本的曲线。

## 8. 开发、运行与证据交付

### 最小变更策略

优先在现有模块 `#[cfg(test)]` 和已有 contract suite 中补用例；跨进程/环境驱动复用项目现有能力。只有可复现缺陷才修改其所属生产模块，保留用户配置语义和异步主链。新增 dependency、系统工具、CI 或部署入口不由本计划自动授权。

每包先交付可重复入口及失败断言，再执行适用环境验证；修复同批更新测试与直接受影响文档。测试代码完成但缺环境的包可独立交付，但运行状态仍为未验收，不阻塞其他无依赖包。

### 命令与产物

执行前遵守[环境规范](../rules/environment-usage.md)与[本地测试规范](../rules/local-testing.md)，核对实际工具链。以下是计划执行入口，不是本次已通过记录：

```powershell
cargo fmt --manifest-path backend/Cargo.toml -- --check
cargo check --manifest-path backend/Cargo.toml --locked
cargo test --manifest-path backend/Cargo.toml --locked --quiet -- --test-threads=4
pwsh -File .agents/skills/project-doc-maintenance/scripts/check-docs.ps1
git diff --check
```

先跑受影响模块定向测试，再跑全量回归；含真实故障、外网、长时间负载和 Unix 子进程条件的测试必须显式选择并检查环境，不能进入无条件默认执行路径。实际筛选名称由新增测试确定，不把上面的全量命令当成所有环境验收入口。

运行配置、证书、数据库、日志和原始测量统一放在 `_fluxdns/`；本机测试 TEMP/TMP 指向 `_fluxdns/test-temp`，Unix 临时路径同样按本地规则设置。隔离介质使用独立测试环境中的专用工作目录，并记录实际绝对路径与授权范围。合成夹具与可复现驱动可跟随测试源码提交，不提交个人数据库、凭据或实际远程地址。

### 证据落点

| 工作包 | 正式结果与实现说明 |
| --- | --- |
| V1、V8 | [生命周期](../implementation/backend/lifecycle.md) |
| V2、V3、V6 | [DNS 管线](../implementation/backend/dns-pipeline.md)；生命周期/后台 owner 证据按实际职责链接 |
| V4、V5、V7、V9、V10 | [后台服务](../implementation/backend/background-services.md)；协议负载证据引用 DNS 管线，不重复整份报告 |

结果注明静态、fake/loopback、真实 SQLite、远程、Unix 或负载证据类别。复用旧结果时注明旧基线和未重跑范围。每包保留场景与实际结果、失败原因、环境缺口和复现入口；不要把新增测试数量当作覆盖率。

## 9. 风险、门槛与退出

- 并发测试的假稳定：用同步点控制目标竞争，保留 watchdog；重复测试与不同合法交错补充确定性用例，不用长 sleep 或无限 retry 消除失败。
- 环境/破坏性动作：故障仅限明确授权的可丢弃介质和进程；磁盘满、权限变更、信号和长时负载执行前核验目标、恢复步骤及资源上限，禁止共享/生产目标。
- 验收无限扩张：V0 列清支持组合与风险因素，评审后冻结；新发现的跨契约设计问题单独确认，不把“不穷举所有排列”写成全部组合已通过。
- 时钟和期限误判：测试 watchdog 与业务 deadline 分离，记录真实 SQL/OS 无法抢占的限制；不得为了通过延长业务 budget。
- 观测反向影响主链：测量开销单独核对，不新增同步请求日志或高基数生产指标来获取测试证据；本地原始记录使用合成数据并脱敏。

本计划已获批，按 V0 和 A/B/C 推荐顺序实施。阶段 C 的具体负载/预算、真实故障介质、远程端点与 Unix 环境在对应阶段开始前确认；未确认时记录门槛，不自动安装或扩权。

退出条件：

1. 七类原范围均取得其要求的证据，或用户明确调整某项验收范围；未执行项不能仅因脚本已提交而关闭。
2. 发现的既定契约缺陷修复并通过相关回归；设计范围变化已另行确认，配置和异步主链约束未被隐式改变。
3. 最终事实、运行证据及接受的残余限制沉淀到上表 implementation；改变设计时同步对应 architecture。
4. 删除本计划、索引项及仅为活动验收保留的入站引用，不建立 archive/history 或旧路径跳转副本；历史由 Git 追溯。

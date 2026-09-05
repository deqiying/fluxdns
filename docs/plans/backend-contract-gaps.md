# 后端契约差距核对计划

> 文档状态：草案
>
> 计划状态：待评审
>
> 适用范围：保留现有配置语义与异步主链的前提下，细化后端契约差距、用户决策与运行验收
>
> 代码基线：`56874aab35ff4bc94b3ad84e8f9fc012bb682cb8`
>
> 最后核对：2026-09-05（重新静态核对决策相关分支；本轮仅补文档，前次运行证据见[后台服务](../implementation/backend/background-services.md#本次验证)）

## 问题与依据

已直接更新[模块设计](../architecture/backend/modules/README.md)及直接受影响的 implementation/configuration 文档，以当前代码为事实基准。以下不再登记为“尚待实现”：正式 Policy/Cache/Upstream 链路、现有资源刷新、SQLite migration、受管 late finalizer 和解析完成事件，这些已经有生产接线。

文档中的旧类型/结构草图也已校正：Config 实际输出、CIDR/域名索引、RuntimeSnapshot 对象图、分层 task owner、确定性退避等，以现有源码描述，不因缺少原设想的类名或 trie 自动新增开发任务。

本计划只保留行为承诺仍未落实、原语未接线或需要运行证据的事项。本轮先详细说明取舍供用户评审，不修改产品代码；下文“建议”均不是已经确认的实施选择。

## 已确认的执行基线

用户已确认以下两条约束，不再作为开放选项：

1. **保留现有配置语义。** 已有字段不在原名称下悄悄改变含义。例如 `dns.cache.persistence.max_size_bytes` 继续表示编码快照预算，不自动变成 SQLite 文件或整个工作目录的物理配额。确需新增产品能力时，先独立确认边界，不先增加配置开关。
2. **保留异步主链优化。** 保持请求完成后的单次无等待移交、后台统计、缓存提交和详情投影；不将 SQLite I/O、逐请求同步指标聚合或日志格式化移回响应路径。已有 bootstrap 地址缓存和后台指标采样不回退。

这两条约束不等于“现有所有行为都不能修正”：启动预算、后台存储算法和有界停机可以改进，但必须证明没有偷换配置承诺或破坏请求路径。是否调整 group 响应选择等可见行为，仍需下面的具体决策。

“互斥决策”需要区分三类：

- **同一行为必须选定语义**：一个已完成查询不能同时“取消其余尝试”和“继续收集其余结果”；同一笔有限停机预算也不能无条件保证所有阶段先完成。
- **可以组合但应分阶段**：增量持久化与近似 LRU、启动 deadline 与写探针、请求 histogram 与逐 attempt 诊断不是同一个功能，不必捆绑批准。
- **需要环境，不是产品二选一**：真实磁盘故障、Unix 信号和长期压力缺少运行证据，不能通过选择“保持现状”自动标为通过。

## 决策总览

| 编号 | 主题 | 决策性质 | 建议，尚待确认 |
| --- | --- | --- | --- |
| D1 | 缓存容量承诺 | 原字段含义已锁定；是否另做物理空间治理 | 保留编码预算，本轮不新增物理硬配额 |
| D2 | 持久化写放大与淘汰 | 两个可以独立实施的优化 | 优先评估增量写；暂保留插入时间淘汰 |
| D3 | 资源 ETag/304 | 后台能力是否纳入本轮 | 支持条件请求，失败不替换有效快照 |
| D4 | group 响应、late result 与负载计数 | 三个独立的行为选择 | 保留生产 late 收集；先不改响应优先级与 primary 计数口径 |
| D5 | 停机时的在途 DNS 请求 | 快速取消与有限等待的取舍 | 本轮保留取消语义；有无损停机需求时另做有界 drain |
| D6 | Storage 启动预算与写探针 | 两项可叠加的可靠性补齐 | 整体预算加独立事务写入/回滚探针 |
| D7 | Storage 停机先保什么 | 共享预算不足时的优先级 | stats 优先，详情尽力排空，明确报告未完成项 |
| D8 | span、attempt 与 histogram | 观测粒度和常驻成本 | 先扩展后台请求指标，完整逐 attempt 诊断另案 |
| D9 | panic 脱敏与退出 | 输出内容与故障升级分开决定 | 安全 hook，不改变现有 owner 的失败传播策略 |

## 决策详解

以下容量、时延与请求数量例子用于解释选择，不是本次实测结果。没有给出未测量的性能收益百分比或固定开发工期。

### D1 缓存容量：限制有效内容，还是限制实际磁盘占用

**当前行为。** [prepare_snapshot](../../backend/src/cache/persistence.rs) 按编码记录和 framing 计算预算；[SQLite adapter](../../backend/src/cache/sqlite.rs) 的 `disk_usage()` 能读取主库、WAL、SHM 大小，但不据此设置页预算或执行物理收缩。配置为 100 MiB，不代表这些文件合计一定小于 100 MiB。

**已排除的选择。** 将原 `max_size_bytes` 直接解释为物理硬上限，会让同一配置下可保存的缓存量发生变化，并引入磁盘边界行为，违反已确认的配置语义。

| 可选范围 | 收益 | 代价与不能保证的事情 |
| --- | --- | --- |
| A. 仅保留编码预算，本轮不加物理治理 | 无兼容性变化，继续使用当前 best-effort 持久化 | 部署时仍需给数据库及辅助文件留空间；程序不承诺物理硬配额 |
| B. 在 A 上增加后台空间观测或软治理 | 更早暴露实际占用，可研究有界维护和超额降级 | 软目标不是“任意时刻均不超过”；维护 I/O、临时空间及失败路径要单独验证 |
| C. 在 A 上另行设计独立物理配额 | 适合确有严格存储预算的部署要求 | 必须定义仅 cache DB 还是含 WAL/SHM、是否含维护临时文件、既有超额库如何处理、超额时停写还是禁用缓存持久化；不能借用原字段 |

**建议。** 本轮选 A。若实际部署已有空间告警，再按 B 的可测目标推进；只有明确需要硬配额才进入 C。物理缓存空间治理不得影响业务统计数据库，也不得把缓存持久化失败升级成普通 DNS 请求失败。

**需要决定。** 本轮是否新增独立的物理空间治理范围，而不是再次决定旧字段的含义。验收应同时报告编码预算与真实文件占用，不能只检查缓存条目数。

### D2 缓存持久化：写得更少与保留热点不是同一项优化

**当前行为。** [SQLite adapter](../../backend/src/cache/sqlite.rs) 每批读取全部 payload、解码合并、裁剪，然后事务内删除并重写保留集；[容量裁剪](../../backend/src/cache/persistence.rs) 按 `inserted_at` 淘汰，时间相同再按 version/key 稳定排序。这不是近似 LRU。

例如，库中已有十万条记录，本批只有十条变化，目前仍走全量读取与重写；另一条很早插入、持续命中的记录，在超额裁剪时仍可能早于新插入的冷记录被淘汰。前者是写放大问题，后者是淘汰质量问题。

| 子决策 | 保持现状 | 可独立实施的改进 | 主要影响 |
| --- | --- | --- | --- |
| D2-W 写入方式 | 批量快照重写，逻辑简单 | 按稳定编码 key 增量 upsert，并只删除失效/被淘汰记录 | 需要内部 schema/key 索引、旧库升级或重建边界、事务原子性、重试幂等与正确的容量核算 |
| D2-E 淘汰方式 | 按插入时间淘汰，确定性强 | 引入有界、粗粒度的访问热度，再由后台做近似 LRU | 需要取得真实命中信息，处理更新丢失、换代和额外内存；只是近似，不能承诺严格 LRU |

**与异步主链的关系。** 增量写可以留在现有后台 owner，不要求改变请求移交。近似 LRU 则不能声称“只加一个后台 SQL 排序即可”：当前 [ResolutionEvent](../../backend/src/ports/observation.rs) 只有 cache lookup 状态，命中事件没有稳定 cache key；不能靠请求详情开关或 qname 还原缓存身份。若要记录热度，必须设计有界的身份和采样传递，不能每次命中同步写 SQLite，也不能把访问时间冒充响应 TTL、CAS version 或插入时间。

**建议。** 先评估并实施 D2-W，暂保留 D2-E 的插入时间语义；量化写放大、后台排队和重启恢复后，再决定是否值得引入热度。D2-W 不自动授权清空既有缓存文件，旧格式处理需在具体实施方案中明确。

**需要决定。** 本轮只处理写放大，还是同时承担淘汰策略与命中采样的改造。验收分别检查写入/扫描量、后台延迟、编码容量，以及热点保留率，不能拿其中一项证明另一项已经优化。

### D3 远程资源：重复下载，还是在内容未变时复用已验证快照

**当前行为。** [fetcher.rs](../../backend/src/resource/fetcher.rs) 无条件 GET，仅收 2xx，`modified_at=None`；[remote.rs](../../backend/src/resource/remote.rs) 校验、解析正文后保存 content/manifest。现有 [fetch port](../../backend/src/ports/effects.rs) 没有条件请求字段或独立 NotModified 结果，因此“放行 HTTP 304”本身不能完成接入。

假设一份很大的规则集一小时内未变化，当前每次定时刷新仍下载正文。条件请求的目标是让服务器确认未变时复用本地有效版本；它不改变刷新周期，也不把联网校验省略掉。

| 可选范围 | 收益 | 代价与失败语义 |
| --- | --- | --- |
| A. 保持全量 GET | 不扩展 port/manifest，错误路径少 | 内容未变也有下载、解析和持久化成本 |
| B. 支持 ETag/Last-Modified 与 304 | 可减少未变化资源的正文传输，保留现有刷新调度 | 必须绑定正确资源身份，区分“内容更新”和“确认未变”，维护 validator 与有效正文的一致性 |

**B 的必要边界。** 只有本地正文、manifest 和资源身份匹配且已验证，才能携带 validator 并接受 304；本地正文丢失或损坏时，不能把 304 当空规则集发布。需要在原 deadline 内进行一次无条件获取，预算不足则保留旧活动快照并报失败。URL、代理解析上下文、format/parser 或配置代际改变时，不盲目复用旧 validator。200 新正文解析失败不得覆盖当前可用状态；304 只刷新成功校验状态，不伪造新的内容 hash。

**建议。** 选 B，作为独立后台优化，保留现有配置字段、失败保留旧快照、禁重定向、大小限制和代理边界。

**需要决定。** 是否将这项节省资源下载成本的能力纳入本轮。验收包含 200/304、无 validator 的服务、本地损坏、配置换代和预算耗尽，不以单个 HTTP 状态测试代替完整资源发布链。

### D4 Group：首响应、后台收集和负载计数应分别决策

**当前行为。** [executor.rs](../../backend/src/upstream/executor.rs) 的并行执行在收到 Positive 时可提前结束前台等待；有 late sink 时把其余任务移交后台 drain，无 sink 时取消剩余任务。有 late sink 时，NXDOMAIN、NODATA、REFUSED 等 terminal DNS response 也可提前返回；无 sink 时，这些非 Positive 结果本身不触发同样的提前返回。具体分类来自 [assess](../../backend/src/upstream/outcome.rs)，SERVFAIL/Truncated 不是这里的 terminal 成功分支。

例如，同一组 A 在 10ms 返回 NXDOMAIN，B 在 80ms 返回 Positive。有 late sink 的当前路径可以先返回 NXDOMAIN，B 的结果随后只进入 late-result 处理，不改写已发送响应，也不保证一定获准写缓存；无 sink 的路径可继续等待并最终选择 Positive。因而这里不只是“是否多发一次请求”，也涉及客户端看到的结果和等待时长。

**D4-P：前台已经返回以后，是否继续收集？**

| 选择 | 收益 | 代价 |
| --- | --- | --- |
| A. 保留生产 late sink 的有界后台收集 | 保留当前 late-window 缓存能力，前台不等待剩余请求 | 已发出的剩余请求仍可能占用网络与 task 资源，需要 owner、取消和容量验证 |
| B. 首响应后取消其余尝试 | 尽快释放本地等待与任务 | 无法再收集后续结果；不是等价清理，会撤掉现有 late-result 能力，也不能保证撤回已经发到远端的请求 |

建议 D4-P 选 A，不为了让文档“统一取消”而回退现有能力。

**D4-R：非 Positive 的首次 terminal response 要不要等潜在 Positive？**

| 选择 | 客户端行为 | 风险 |
| --- | --- | --- |
| A. 本轮保持当前有/无 sink 分支 | 不改变既有路径的响应与等待语义 | 分支差异继续存在，应补测试并明确边界，不能宣称两者完全一致 |
| B. 统一首个 terminal DNS response 提前返回 | 更偏向低延迟，无 sink 的非 Positive 也立即结束 | 可能更早返回 NXDOMAIN/REFUSED，错过后到的 Positive；是可见行为变化 |
| C. 等待 Positive 或到期后再聚合 | 更偏向收集完整候选 | 快速负响应可能需要等慢上游；必须明确等待上限，不能把“异步实现”误称为“没有额外客户端等待” |

建议本轮 D4-R 选 A；只有明确更看重哪种响应策略时再改变。不能仅因 B/C 使用 async 就认为不影响主链性能和答案。

**D4-L：load-balance 衡量“分配给谁”，还是“谁此刻真的在执行”？** [GroupSelector](../../backend/src/upstream/group.rs) 按 primary in-flight/weight 选首成员；executor 持有该 primary lease 直到整轮顺序尝试结束，后续重试按配置顺序。假设 primary A 失败后正在重试 B，当前负载仍计在 A，B 不因此增加这份 lease 计数。

保留这一口径，表示衡量按 primary 分配的进行中查询，兼容性最好；改成逐 attempt 计数，才更接近各成员实际占用，但需要每次尝试、取消、嵌套 group 和失败都准确获取/释放 lease。是否连重试顺序也改为动态最小负载，是另一项更大的选择，不能随计数修正顺手改变。建议本轮保留 primary 口径，不将其标为逐 attempt least-in-flight。

**需要决定。** D4-P、D4-R、D4-L 分别确认；一个“优化 group”不能代替三项语义。验收必须观测首响应时间、实际 RCODE、剩余任务、late 准入和 lease 归零，而不只是最终函数返回成功。

### D5 停机请求：尽快取消，还是给已完整接收的请求完成机会

**当前行为。** [DnsService::shutdown](../../backend/src/service.rs) 标记 draining 后取消 transport task；UDP、TCP、DoH 的 dispatch 与取消竞争，因此正在处理的请求可能没有完整响应。随后等待 request guard、Resolution、Cache、Storage 和 Telemetry 回收。5 秒是整体默认 grace，不是每个请求或每个阶段额外获得 5 秒。

例如，上游预计还需 300ms，进程此时收到停止信号：当前路径可以立即取消该查询；有界 drain 则尝试让它在剩余预算内返回。但如果客户端不再读取 TCP/DoH 响应，drain 不能无限等写回。

| 选择 | 收益 | 代价与适用情景 |
| --- | --- | --- |
| A. 保留立即取消与有界回收 | 停止动作直接，不改 session/token 生命周期 | 滚动更新时客户端可能需要重试，不能称为无损停机 |
| B. 停止接收新请求，已完整接收者有界完成 | 减少正常停机时正在处理的查询被中断 | 必须区分 accept cancellation 与在途请求 cancellation，覆盖部分帧、流水请求、慢客户端、旧 runtime；会消费后台 flush 的同一总预算 |

**B 不是无限保证。** 只等待已经完成协议读取并获得准入的请求，不把已建立但空闲的连接等同于已接收请求；请求仍受原 deadline、客户端断连、第二信号和剩余 grace 约束。若全部 grace 都用于前台响应，后台统计和缓存就可能来不及排空；还要定义是否给后台保留时间，不能承诺“请求全部答复且所有后台数据必定落盘”。

**建议。** 在没有明确无损滚动重启要求前，本轮选 A 并补时序证据。若部署确实依赖该能力，再单独批准 B 及阶段预算。B 可以保留异步请求实现，但不是无行为变化的修复。

**需要决定。** 是否接受正常停机时客户端重试，还是要把有界在途完成作为新的验收要求。它与 D7 的存储优先级相关，但修改一个不能自动解决另一个。

### D6 Storage 启动：整体时间上限与可写性验证

**当前行为。** [StorageRuntime::open](../../backend/src/storage/service.rs) 先 await `SqliteStorageBackend::connect`，再调用带 deadline 的 migrate 检查；[connect](../../backend/src/storage/sqlite.rs) 本身执行建目录、连接、建表/元数据和 schema 升级，没有接收 open 的 deadline。现有 `health_probe` 执行 `SELECT 1`，不是独立写入/回滚探针。

这不是“启动完全没有写操作”：connect 已有建库和迁移写入。真正缺口是整个启动阶段的预算覆盖，以及独立、可验收的目标数据库事务可写性检查。应先证明额外探针的覆盖价值，不能仅为增加检查层次重复迁移逻辑。

| 选择 | 补齐范围 | 代价 |
| --- | --- | --- |
| A. 只覆盖整体启动 deadline | 预算过期拒绝继续启动，失败不创建可服务 owner | 不新增独立可写性保证 |
| B. A 加最小事务写入并显式回滚 | 在启动时验证目标库可开始写事务、执行受控写入并回滚 | 多一次启动 I/O；探针/回滚失败要保留安全错误，不能写虚假业务统计或详情 |

**必要边界。** deadline 应贯穿连接、升级、探针和失败清理，不为每一步重置计时；超时不能只返回一个错误而遗留继续工作的 owner。同步文件操作和底层数据库工作不能仅凭外层 timeout 就宣称可强制中断；应核对其取消和回收行为。探针只证明检查时刻可用，不保证运行期间永远有磁盘空间，也不是断电耐久性测试。

**建议。** 选 B，沿用当前业务数据库启动失败即拒绝服务的契约，不扩大为 DNS 请求中的同步写检查，也不改变缓存恢复可降级的独立行为。

**需要决定。** 本轮只补时间预算，还是同时把独立可写性验证纳入启动契约。验收包括首次建库、已有库升级、锁等待、探针/回滚失败及超时清理，且探针后业务数据不变。

### D7 Storage 停机：预算不足时先保存统计还是查询详情

**当前行为。** [StorageRuntime::shutdown](../../backend/src/storage/service.rs) 先取消并等待独立详情 task，再调用内部 `StorageService::shutdown`；只有后者才是 stats、detail、backend 的顺序。因此“内部 facade 先 stats”不等于正式运行 owner 的统计优先。

例如，剩余停机时间只有 1 秒，详情排空预计需要 2 秒，统计 flush 只需 100ms。当前顺序可能先把 1 秒用在详情上，统计尚未开始就没有预算；统计优先则争取先保存汇总，再处理剩余详情。这是有限时间内保护哪类数据的真实取舍。

| 选择 | 优先保护 | 代价 |
| --- | --- | --- |
| A. 保持详情 task 先关闭 | 更早回收已有详情消费者，生命周期改动小 | 慢详情可能耗尽统计预算 |
| B. 统计优先，详情用剩余时间 | 对总请求数、日统计等核心汇总更有利 | 时间不足时允许详情明确不完整；需要正式 owner 的协调，不只是移动一行调用 |
| C. 两者并行尽力 | 不预设逻辑上的先后 | 共享 SQLite/operation lock 仍会竞争，不能因此保证统计优先或两者都及时完成 |

**B 的必要边界。** 前置 Resolution owner 已停止产生新统计；停止详情的新输入，并处理已经在途的详情写事务，再建立 stats 优先的操作顺序。不能简单 abort 一个任务就假定底层 SQL 已经退出。统计失败后仍尝试预算内清理并报告错误，不让失败丢失；只有消费者全部回收后才关闭 backend。

**建议。** 选 B，与“统计默认开启、详情可关闭”的现有产品定位一致。承诺的是优先级和可观测结果，不是磁盘故障时统计零丢失；若上游 shutdown 阶段已耗尽总预算，B 也不能补回时间。

**需要决定。** 是否接受停机预算不足时优先保护聚合统计、允许详情缺口。验收需证明慢详情、锁竞争、stats 失败和 shared deadline 下的实际顺序与错误报告。

### D8 观测：知道请求结果，还是重建每个上游尝试的完整过程

**当前行为。** 正式 [TelemetryWriter](../../backend/src/observability.rs) 只聚合两个已接线指标；[ResolutionEvent](../../backend/src/ports/observation.rs) 已有请求总耗时、core 耗时、终态和 cache lookup 状态，但没有每个候选的开始/结束时间。请求完成事件无法事后准确重建各 attempt 的完整时间线。

例如，一次请求最终成功，不足以回答“是否 A 超时后才重试 B、B 用了多久、parallel 中哪个候选被取消”。请求 histogram 回答整体延迟分布；逐 attempt 指标或 span 才能回答这些内部过程，两者不能相互冒充。

| 粒度 | 能回答的问题 | 成本与能力边界 |
| --- | --- | --- |
| A. 保持当前两个后台指标 | ingress 接收数量与 writer 排队深度 | 无请求延迟分布、无完整逐 attempt 流 |
| B. 后台消费既有完成事件，增加有界请求 histogram/结果指标 | 已接收完成事件的耗时分布、缓存和终态分布 | 无需新增逐请求同步聚合；必须定义固定桶、标签、输出与 ingress gap，不包含丢失事件 |
| C. 在 B 上增加采样的逐 attempt 诊断 | 一部分请求的候选时序、失败和取消原因 | executor 必须新增有界采样数据，后台不能凭空恢复；要确认采样比例、队列、标签和性能预算 |
| D. 默认完整 span 树和所有 attempt 事件 | 更全面的关联过程 | 常驻事件量随候选数增长，需端到端 owner、容量及采样/导出设计；本轮不默认批准 |

**与已确认基线的关系。** “异步输出”不等于“采集零成本”。C/D 即使不写磁盘，也会增加时钟读取、上下文传递、事件构造和有界入队；不能未经测量就保证不影响此前优化。B 可以复用现有冻结字段，但仍要把聚合放在后台，不改变 `logs.enable=false` 的既有边界。

**建议。** 若确实需要新增可操作的延迟指标，先选 B；C 仅在明确排障需求和开销预算后单独设计，不默认实现 D。不能为了通过旧文档的“完整观测”检查新增第二套请求完成事件或默认常驻全量 tracing。HTTP exporter、新 API 和新存储表也不随 B 自动进入范围。

**需要决定。** 当前需要的是整体延迟分布，还是逐候选问题定位。验收必须分别验证数据口径、桶/series 容量、开关、丢弃诊断、reload/关闭及主链开销。

### D9 Panic：安全输出与是否立即终止进程分开处理

**当前行为。** [main.rs](../../backend/src/main.rs) 没有自定义 panic hook；[Application 设计](../architecture/backend/modules/application.md) 要求不输出配置、DNS wire 或 secret。Service 对其观察到的 task panic 有失败传播路径，但 hook 本身不等于所有内部 owner 的 panic 监督。

这里不把“保留原始秘密以便诊断”列为合法生产选项。真正需要明确的是允许保留哪些定位信息，以及是否改变既有故障升级规则。

| 范围 | 行为 | 影响 |
| --- | --- | --- |
| A. 只补安全 hook | 直接输出固定事件分类、受控源码位置和回溯启用/可用状态，不打印 payload、任意线程名或完整 backtrace | 降低未受控内容泄露风险；原始 panic 文本不再可见，定位依赖源码位置与安全事件 |
| B. 在 A 上让任意 task panic 都立即结束整个进程 | 对所有 panic 采用统一 fail-fast | 会绕过部分 owner 的正常清理与后台 flush，改变可用性和数据保全行为，不是“加 hook”的必然要求 |

**建议。** 本轮选 A。hook 不递归调用可能已损坏的 telemetry writer，不吞掉 panic，也不把致命错误改成成功；沿用现有传播/退出策略。主线程、受监督 task 与内部 worker 是否全部按预期升级，需要按 owner 分别验证，不能仅凭 hook 安装成功宣称解决。

**需要决定。** 确认 A 的安全定位字段与排查方式；B 若无明确 fail-fast 需求不实施。验收用含敏感标记的 panic payload 检查实际 stderr，并检查相关退出/清理路径，不将测试中的 catch 当成生产可继续服务的证明。

## 建议的实施组合

以下是供评审的组合，不表示本轮已经获得逐项批准：

1. 优先补可靠性：D9-A 安全 hook、D6-B 整体启动预算与写探针、D7-B 统计优先关闭。三者都应留在启动/后台/停机边界，不增加正常 DNS 请求的同步 I/O。
2. 后台优化分开交付：D3-B 条件请求；D2-W 增量持久化；有实际指标需求再做 D8-B。不能因合并实施而丢掉各自的格式、容量与失败验收。
3. 本轮暂不改变：D1 的编码预算、D2-E 的插入时间淘汰、D4 的既有响应/late/primary 口径、D5 的停机取消语义；不启用 D8-D 全量 tracing。

暂不改变的条目要区分“本轮延期”和“正式接受现状并撤销旧要求”。只有用户明确选择后才能相应关闭或延期，不能将本建议自动写成已接受架构、删除缺口或将未执行验收标成通过。

## 现有原语的处置

已获批的两类接入不再作为本计划缺口：bootstrap 地址状态已接入配置绑定 resolver，详见 [DNS 管线](../implementation/backend/dns-pipeline.md)；有限 counter/gauge 聚合已接入正式 writer 和后台采样，旧事件/health 模型已收口，详见[后台服务](../implementation/backend/background-services.md)。对应已完成方案按文档规则删除，不保留第二份长期设计。

无调用方的 secret port 和旧 hosts-only Core 装配已清理；有效 Ports、测试参考实现及文件 cache codec 仍保留。职责分别见 [Ports](../architecture/backend/modules/ports.md)、[DNS 管线](../implementation/backend/dns-pipeline.md)和[后台服务](../implementation/backend/background-services.md)，不能把测试 fake 的存在当成主链冗余或性能优化。

资源 ETag/304、完整 tracing span/attempt 流、安全 panic hook 不属于“已有完整实现、只缺调用点”：需要补充契约或实现，再接入并验证。

## 已实现但未完成运行验收

下表保留环境和组合验收缺口。相关单测已随前次完整后端测试执行，记录见[后台服务](../implementation/backend/background-services.md#本次验证)；本轮未运行产品测试，已有单测也不能替代最后一列要求的真实环境、长期压力或更完整组合。

| 范围 | 已有实现 / 测试入口 | 尚缺的证据 |
| --- | --- | --- |
| late-window / owner | [dns/policy.rs](../../backend/src/dns/policy.rs) 有 latest-runtime refresh/late sink 测试；[service.rs](../../backend/src/service.rs) 有 `shutdown_closes_finalizers_from_previous_and_current_runtime` | 跨 revision、producer lease、首/晚响应、取消、sink 满和 shutdown 的组合矩阵；先区分上表行为差距 |
| 真实数据库故障 | [cache contract tests](../../backend/src/cache/backend_contract_tests.rs)、[storage contract tests](../../backend/src/storage/backend_contract_tests.rs)、SQLite Busy/DiskFull hooks | 隔离介质下的真实 busy/full/权限/恢复；旧数据、pending/ledger、gap、DNS 时延和资源占用，不能用注入 hook 替代 |
| Adapter conformance | [ports/testing.rs](../../backend/src/ports/testing.rs)、service 的跨 UDP/TCP/plain DoH 测试及各出站 adapter 用例 | direct/bootstrap/connect_ip/SOCKS5/SOCKS5H、Host/SNI、TLS/PROXY、deadline/cancellation 的实际组合；不存在统一运行时 conformance 门禁 |
| Runtime 并发与时间控制 | [coordinator.rs](../../backend/src/runtime/coordinator.rs)、[prepared.rs](../../backend/src/runtime/prepared.rs)、[supervisor.rs](../../backend/src/runtime/supervisor.rs) 有合并、CAS、rebind、retry/drain 测试 | Policy/metadata 分步发布、旧 task 迟到、资源与 reload 竞争、内部 owner panic；FakeClock 不代表全部 Tokio timer 可统一推进 |
| Storage migration 与压力 | [sqlite.rs](../../backend/src/storage/sqlite.rs) 有升级/重开/幂等用例，`ledger.rs`/`stats.rs` 有批次重试原语 | 各旧版本升级、跨午夜/late event、积压保护、软硬详情容量、故障恢复及停机优先级 |
| DoH/TLS 安全与限额 | [doh.rs](../../backend/src/transport/doh.rs)、[system_socket.rs](../../backend/src/runtime/system_socket.rs) 有 wire/header/GET/POST/TLS/PROXY 用例 | 真实 TLS/forwarded 信任链、坏连接隔离、1,024 session 上限、长期连接恢复；UDP 顺序执行/分配策略不等同通用 admission limiter |
| Unix 信号与长期负载 | [service.rs](../../backend/src/service.rs) 有 SIGTERM 分支和第二信号等待，源码保留本机 profile 测试 | Unix 真实进程双信号；冻结硬件、QPS、并发与资源预算后长期压力，不沿用旧百分比 |

## 目标与非目标

已锁定配置语义和异步主链基线，下一步对 D1-D9 选择本轮实施、保持现状或延期的具体范围。不新增 DoT/DoQ、主动健康检查，不为了对齐旧类型草图新增 SecretProvider adapter、TransportProfile 或 trie，不顺手重构 DNS 模块。

这是关键契约核对，不是逐函数正确性审计、完整性能评估或安全认证。用户要求以代码为准，已据此更正当前能力；对原来更强的安全、容量、取消和观测承诺，仍显式保留差距，不能通过改文档静默关闭。

## 步骤、风险与退出

1. 记录 D1-D9 的用户选择，尤其分别记录 D2 的写入/淘汰和 D4 的收集/响应/计数决定；当前仅两条执行基线已确认。
2. 为获批行为和现有验收缺口建立定向矩阵；优先使用项目现有工具/fake，真实环境不足时保留限制，不擅自安装或操作真实故障介质。
3. 按批准范围实施与定向验证，同批更新 implementation；契约确实改变才更新 architecture。
4. 所有登记项有实现/验收证据或明确的范围决策后收口；确认新逻辑已沉淀到对应 implementation、改变的设计已同步到 architecture，再删除本计划与索引项。

风险是把缺验收写成未实现，或把已有缺陷通过文档改成合法行为。具体执行检查以交付记录为准；过时的 v2 计划已按用户要求移除，既有环境证据边界见[交付实现](../implementation/delivery.md)。本计划仍为待评审，不因个别原语接入或文档清理而关闭。

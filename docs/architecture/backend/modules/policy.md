# Policy 模块设计

> 文档状态：有效
>
> 适用范围：client、strategy、rule、resource matcher、`PolicyContext` 与 `RouteDecision`
>
> 最后评审：2026-09-08（客户端管理键、请求身份索引及 mapped IPv4 匹配边界；其余基线见[模块索引](README.md)）
>
> 2026-09-20 局部评审：仅核对 group ECS、cache semantics fingerprint 与跨 runtime 重新决策约束；不代表整篇重审或运行验收
>
> 局部评审基线：`fcbb12831c66e81e02108ccd030ac3c2a1e08f56` 加本次工作树；仅用于上述范围
>
> 2026-11-08 局部评审：仅同步 dat selector 按需编译的 Resource 依赖描述；其余范围沿原评审记录
>
> 2026-11-08 局部评审：仅同步 DoH 裸路由可提取隐式 ID、显式模板必填和 canonical route ID 约束；客户端精确 ID 优先及 CIDR 回退不变，其余范围沿原评审记录。
>
> 2026-11-09 局部评审：基线 `08a774724e2304f72dd7b8d0a9d0299983f9b0e5` 加工作树，仅核对完整决策后 v3 AnswerKey、成员 ECS 与 refresh/late-result；未新增运行验收。
>
> 2026-11-10 局部评审：基线 `f73514297f7df3925ff0f912840210e4acdd1046` 加工作树，仅补充 ECS 请求优先/自定义兜底模式，既有层级与成员覆盖规则不变。
>
> 关联实现：[client.rs](../../../../backend/src/policy/client.rs)、[plan.rs](../../../../backend/src/policy/plan.rs)、[dns/policy.rs](../../../../backend/src/dns/policy.rs)
>
> 关联文档：[后端架构](../overview.md) · [配置字段参考](../../../implementation/configuration.md) · [DNS Core](dns-core.md) · [Resource](resource.md) · [Upstream](upstream.md)

## 1. 职责

Policy 模块把已解析配置和资源 snapshot 编译成纯内存决策索引，并将每个请求的决策拆为可安全提前计算的 `PolicyContext` 与按需执行的 `RouteDecision`。

它负责：

- client ID/IP 匹配；
- listener/route 默认策略与 client override；
- strategy 有序规则；
- hosts/rule_set 匹配；
- cache、TTL、ECS 的生效值计算；
- 本地回答或 upstream target 的确定。

它不执行网络 I/O、读文件、写缓存或构建 DNS transport response。

## 2. 内部结构

| 文件 | 职责 |
| --- | --- |
| `client.rs` | 管理 name map、exact client ID map、按前缀长度排序的 CIDR 列表和冲突检测 |
| `strategy.rs` | strategy、覆盖值和默认 upstream |
| `route.rs` | listener/DoH route 到基础策略的映射 |
| `plan.rs` | client override、cache/TTL/ECS 生效值，以及请求级 `PolicyContext`/`RouteDecision` 组合 |

规则数据结构由 Resource 模块编译，Policy 只持有不可变 matcher handle。

## 3. 编译产物

`PolicyIndex` 编译 client、strategy、route 与 hosts/rule 资源索引，语义上提供：

- 配置管理 `name` map 与大小写敏感的 exact `client_id` map，两者不互相派生；
- IPv4/IPv6 CIDR 列表，按 prefix length 降序扫描实现最长前缀匹配，并非 trie；
- strategy ID → compiled strategy；
- listener/route → base strategy；
- hosts/rule resource handle；
- typed upstream handle；
- 每层 cache/TTL/ECS override；
- 会改变路由或最终查询的 typed 决策输入；target 子树摘要由外层 `UpstreamRuntime` 预计算；
- 用于观测的稳定、低基数 ID。

编译发生在 prepare/resource update，不在请求时解析字符串引用。

索引只持有已解析 typed 值。`ResolvedClient`/`ClientRule` 以 `name` 表示配置管理键，生产 v2 的单个 `client_id` 直接进入内部 `client_ids` 统一容器。client、strategy、route 分别编译，重复 name、重复 client ID、空 matcher 和引用错误在构造时拒绝。resource prepare 交付已编译 snapshot；同步测试构造器不等同完整资源准备入口。

PolicyContext 在逐规则 matcher 前产生 cache/TTL/namespace 等上下文；每个请求随后执行 RouteDecision 的 listener hosts 与有序 strategy rule，再准备最终成员 ECS 和答案 key。Fast 路径已删除。资源 CAS 仍同步发布 matcher/version/hash，但资源 hash 不进入答案 key。具体 core/registry 构造器见[DNS 管线实现](../../../implementation/backend/dns-pipeline.md)。

## 4. 域名规范化

所有 matcher 使用统一 canonical domain：

- DNS label 比较 ASCII 大小写不敏感；
- 统一移除表现层末尾根点，但保留根域特殊值；
- 拒绝空 label、超长 label 和超长 name；
- 不在查询热路径执行任意 Unicode IDNA 猜测；
- 配置 parser 负责把文本域名转换为与 DNS wire 相同的 canonical representation。

regex 在资源加载时编译，运行时只执行已限制语法和大小的 matcher。

## 5. Client 匹配

顺序固定：

1. 有 `client_id` 时先做 exact ID lookup；
2. 未命中 ID 时先把 IPv4-mapped IPv6 `client_addr` 规范化为 IPv4，再做最长 CIDR 前缀；
3. 都未命中时使用 `unknown` bucket；
4. 同优先级冲突必须在 Config prepare 阶段失败，运行时不依赖数组顺序。

匹配结果包含 client rule name 与实际 identity。cache namespace 使用实际命中的 client ID 或规范化 IP 生成域分隔 SHA-256 摘要；IPv4 与其 mapped IPv6 表示得到同一摘要，不只使用 client rule name，也不把原始身份写入缓存键。

## 6. Strategy 选择

基础策略来自普通 listener 或 DoH route。client rule 可以覆盖 strategy：

```text
base strategy from listener/route
  → apply matched client.strategy when present
  → load compiled strategy
```

引用不存在应在 prepare 阶段失败。运行时出现缺失表示 snapshot 不变量被破坏，返回 internal error，不能静默回退到任意默认策略。

## 7. Rule 执行

执行顺序：

1. 检查 listener 级 hosts；命中则本地回答；
2. 从 strategy 第一条 rule 开始顺序匹配；
3. `hosts` rule 命中则本地回答；
4. `rule_set` 命中则选择该 rule 的 upstream；
5. 没有 rule 命中则使用 `default_upstream`。

first-match 指 strategy rule 顺序。HostsIndex 内部为 exact → 最长 wildcard suffix；RuleIndex 内部为 exact → 最长 domain suffix（含 apex）→ keyword → regex。不能混成一个包含 wildcard 的通用规则 matcher，也不能省略 keyword 的优先级。

`rule_set` 引用先尝试完整资源名；完整名不存在且包含 `:` 时，才按第一个 `:` 解释为 `resource:selector`。资源名大小写敏感；selector 使用 Config/Resource 共用规则归一化为小写，允许 `!` 等不产生分隔歧义的可打印 ASCII。selector 只对支持子集的格式有效，不存在或格式不支持时在 prepare 阶段失败。Resource 在加载或刷新时只编译并缓存被 strategy 引用的 selector matcher（见 [Resource dat](resource.md#datgeositedat)）；查询热路径只对当前 strategy rule 执行一次 map lookup，不逐个重新校验 selector。

## 8. 覆盖与继承

Policy 不从原始配置对象动态 fallback，而是读取 Config 已归一化的 override。

缓存：

```text
client cache → strategy cache → global cache
```

- 当前层显式 `enabled: false` 立即停止选择；
- 当前层 `enabled: true` 选择对应 namespace；
- 整块缺失才继续到下一层；
- 一个请求最多选择一个 namespace。

TTL：

```text
client ttl_override → strategy ttl_override → global ttl_override
```

ECS：

```text
rule → strategy → client → upstream → global
```

`disabled` 是明确结果，不继续继承。`custom` 与 `client_or_custom` 必须已有本级合法 CIDR；后者优先合法请求 ECS，仅缺失或非法时使用自定义值，不按客户端地址推导。ECS 优先级保持 `rule > strategy > client > upstream > global`：group 有 rule/strategy/client 显式结果时统一覆盖成员；否则遍历 primary、fallback 与 nested group 的全部可达 direct leaves，让显式 upstream ECS 覆盖 global，未显式成员继承请求级 ECS。最终 ECS 一致时使用 uniform scope 缓存；真正异构时保留 per-member query，按排序成员 ECS 组合摘要精确缓存，lookup、single-flight 和 commit 均不绕过。请求 ECS 清除 host bits、source 上限 IPv4 /24、IPv6 /56，较短前缀保留，查询 scope 置 0；自定义 CIDR 不按 client 上限截断。完整字段契约见[配置参考](../../../implementation/configuration.md#ecs-优先级)。

## 9. `PolicyContext` 与 `RouteDecision`

`PolicyContext` 的实际字段是 listener_id、可选 route、ClientMatch、`Arc<ResolvedStrategy>`、CacheDecision 和 TTL override。Core 结合上下文计算请求级 ECS，不将衍生值误列为 context 字段。

`RouteDecision` 在每次请求查缓存前计算，包含 upstream `ConfigId`、生效 ECS、可选 hosts `ConfigId` 和 MatchedRule；并没有独立 `LocalAnswer` / `UpstreamTarget` 返回类型。资源版本与缓存 provenance 由 Core 组合为 completion observation。`ResolutionPlan` 是两阶段结果的组合视图。

两类结果不持有 SecretRef、HTTP client 或具体 connector；其中的配置 handle 和 matcher 引用来自 prepare，不能把“typed”解释为所有字段都不含字符串。

### 9.1 target fingerprint 与答案身份

`dns/policy.rs` 的 `UpstreamRuntime` 为各 target 预计算子树 fingerprint：包括 group 成员/fallback、模式、权重、超时与嵌套结构，以及 direct 的完整 DoH URL、connect_ip、bootstrap、所用 outbound 和上游 hosts 内容。无关 target 或 policy 资源更新不改变该摘要。

完整决策后，Core 用 namespace、question、RD/CD/DO、保留 EDNS options、transport compatibility、target fingerprint 和最终 ECS 构造 v3 `AnswerKey`。listener、route、客户端、strategy、规则/hosts hash 不再是答案维度；需要身份隔离时使用显式 client/strategy namespace。EDNS 存在本身不入 key，规范化细节与 scope/exact 边界见 [Cache](cache.md)。

资源刷新继续原子发布 Policy matcher/index 与 content hash，新请求按新规则决策；相同 target/最终查询仍复用答案，不清库。listener/strategy hosts 本地命中始终绕过 response cache。

## 10. 一致性

一次 `prepare_context`/`evaluate_route` 使用请求捕获的同一个 `RuntimeSnapshot` 和同一次加载的 Policy 资源状态。资源刷新后，新请求使用新 matcher 与 content hash；已开始请求继续使用旧 `Arc`，不加全局读锁。

请求决策使用同一次加载的 `Arc<PolicyState>`，其中 matcher、版本和 content hash 一起 CAS 发布。Runtime 的 `ResourceRegistrySnapshot<()>` 是另一份观测 metadata：`PreparedRuntime::refresh_resource` 先发布 Policy，再更新 metadata，并非两个 `ArcSwap` 的跨对象原子事务。请求决策不通过这份 metadata 重建 matcher；两者一致性不能被描述为“所有读者同时看到同一份原子组合”。

optimistic refresh 与 late-result 切换 latest core 时重新执行 `prepare_cache_query`，按最新 Policy/资源得到完整决策。写回核对 `AnswerKey` 相等，不再保存或比较 policy semantics；policy 变更但答案身份不变可以继续接纳。刷新 scope 迁移的版本水位与锁内覆盖清理契约见 [Cache](cache.md#5-optimistic-与-late-result)。

## 11. 错误语义

- prepare 阶段：重复 client name/client ID、冲突 CIDR、缺失 strategy/upstream/resource、非法 selector 直接失败；
- 请求阶段：正常不匹配使用 default upstream；
- snapshot 不变量破坏返回 internal，DNS Core 映射 SERVFAIL；
- regex 或 matcher 不得在热路径产生 panic；
- client 信息缺失不是错误，进入 `unknown`。

## 12. 契约验证要求

- name 与 client ID 独立索引、ID 优先于 CIDR、IPv4/IPv6 最长前缀、mapped IPv4 和 unknown；
- 冲突在 prepare 阶段拒绝；
- base strategy 与 client override；
- DoH 使用 Transport 将裸路由隐式后缀纳入逐段固定段优先、同形显式模板优先后选出的 canonical route ID，该 ID 始终为配置 `path` 原文；裸路由精确命中时不携带 ID、追加单个非空段时携带 ID，显式模板必须携带非空 ID。Policy 不重新匹配路径，客户端仍按精确 ID 优先、来源 IP/CIDR 回退匹配，路径 ID 不作为认证（2026-11-08 局部评审，完整路由边界见[配置参考](../../../implementation/configuration.md)）；
- listener hosts、strategy first-match、default upstream；
- exact/suffix/wildcard/regex 优先级；
- `resource:selector` 解析和不存在错误；
- cache tri-state、TTL、ECS 全覆盖矩阵，以及 primary/fallback/nested direct leaves 的统一/异构最终 query；
- `prepare_context` 不执行逐规则 matcher，但所有请求都在 cache lookup 前完成 `evaluate_route` 与成员 ECS 准备；
- v3 AnswerKey 的 target/最终查询稳定性、uniform/异构编码隔离、跨入口共享与 namespace 身份隔离；
- 同一 snapshot 下决策确定性；
- 资源 swap 后 matcher 与 hash 同步切换，新旧请求各自保持一致。
- refresh/late-result 切换 latest core 后重新准备，按 AnswerKey 一致性接纳；scope 变化清理不得删除刷新期间更新的条目。

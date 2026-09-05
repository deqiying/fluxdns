# Policy 模块设计

> 文档状态：有效
>
> 适用范围：client、strategy、rule、resource matcher、`PolicyContext` 与 `RouteDecision`
>
> 最后评审：待核对（本次仅分类与边界复核，不等同完整契约重审）
>
> 关联实现：`backend/src/policy/*`
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
| `client.rs` | client ID map、CIDR trie 和冲突检测结果 |
| `strategy.rs` | strategy、覆盖值和默认 upstream |
| `route.rs` | listener/DoH route 到基础策略的映射 |
| `plan.rs` | client override、cache/TTL/ECS 生效值，以及请求级 `PolicyContext`/`RouteDecision` 组合 |

规则数据结构由 Resource 模块编译，Policy 只持有不可变 matcher handle。

## 3. 编译产物

`PolicyIndex` 至少包含：

- exact client ID map；
- IPv4/IPv6 longest-prefix trie；
- strategy ID → compiled strategy；
- listener/route → base strategy；
- hosts/rule resource handle；
- typed upstream handle；
- 每层 cache/TTL/ECS override；
- 会改变答案的策略语义摘要输入与 fast-path safety；
- 用于观测的稳定、低基数 ID。

编译发生在 prepare/resource update，不在请求时解析字符串引用。

索引只持有已解析 typed 值。client、strategy、route 分别编译，重复名称、空 matcher 和引用错误在构造时拒绝。resource prepare 交付已编译 snapshot；同步测试构造器不等同完整资源准备入口。

PolicyContext 在逐规则 matcher 前产生 cache/TTL/ECS/namespace；fast miss 后 RouteDecision 才执行 listener hosts 与有序 strategy rule。规则结果只输出 typed target、resource/version 和安全摘要。PolicyState 预计算不含观测/管理配置的语义基底，资源 CAS 同步更新 matcher 与 content hash，请求 fingerprint 只编码稳定字段。具体 core/registry 构造器见[DNS 管线实现](../../../implementation/backend/dns-pipeline.md)。

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
2. 未命中 ID 时按 `client_addr` 做最长 CIDR 前缀；
3. 都未命中时使用 `unknown` bucket；
4. 同优先级冲突必须在 Config prepare 阶段失败，运行时不依赖数组顺序。

匹配结果包含 client rule ID 与实际 identity。cache namespace 使用实际命中的 client ID 或规范化 IP 生成域分隔 SHA-256 摘要，不只使用 client rule name，也不把原始身份写入缓存键。

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

first-match 只指 rule 顺序；单个 matcher 内部使用 exact → most-specific suffix/wildcard → regex 的固定优先级。

`rule_set` 引用先尝试完整资源名；完整名不存在且包含 `:` 时，才按第一个 `:` 解释为 `resource:selector`。资源名大小写敏感；selector 使用 Config/Resource 共用规则归一化为小写，允许 `!` 等不产生分隔歧义的可打印 ASCII。selector 只对支持子集的格式有效，不存在或格式不支持时在 prepare 阶段失败。Resource 已在加载或刷新时编译并缓存所有 selector matcher；查询热路径只对当前 strategy rule 执行一次 map lookup，不逐个重新校验 selector。

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

`disabled` 是明确结果，不继续继承。`custom` 必须已有合法 CIDR。group 在 rule/strategy/client 未显式覆盖时，把 direct member 的 upstream ECS 应用到该成员 query；成员 ECS 优先于 global。由于成员选择发生在 fast cache lookup 之后，此类 group 不使用 fast key，并继续绕过 response cache，避免不同成员 ECS 共用错误响应。

## 9. `PolicyContext` 与 `RouteDecision`

`PolicyContext` 是 fast cache lookup 的前置结果，包含：

- client bucket 和 identity digest；
- strategy 与 route identity；
- `CacheDecision`：disabled 或唯一 namespace；
- effective TTL override；
- 可在 matcher 前确定的 effective ECS；
- fast-path eligibility 与 policy/request fingerprint 输入。

`RouteDecision` 仅在 fast miss 或不安全时计算，包含 matched rule/resource、`LocalAnswer` 或 typed `UpstreamTarget`、最终 ECS、resource revision 摘要和安全 decision trace IDs。兼容的 `ResolutionPlan` 只是两阶段结果的组合视图，不是请求热路径必须先完整构造的单体。

两类结果都不含原始配置字符串、regex 文本、SecretRef、HTTP URL 或具体 connector。

### 9.1 fast cache 语义 fingerprint

policy fingerprint 覆盖会改变答案的已解析 typed 配置、strategy/upstream/hosts/rule 语义、资源 content hash 和选择安全性；`logs`、`webui`、`database` 等纯观测/管理字段明确排除。request fingerprint 使用规范化 ECS；无 ECS 时只编码 client address 的 `/24`（IPv4）或 `/56`（IPv6）网段，不把原始地址写入 key 或 `Debug`。最终 target/ECS 只有在 resolved mode 中加入。

资源成功刷新时，matcher/index 与对应 content hash 在同一次 Policy 状态发布中生效；因此新请求会切换 fast key，旧 entry 不必全局清理。成员特有 ECS 或其他不能在 matcher 前证明安全的路径必须标记 fast ineligible。

## 10. 一致性

一次 `prepare_context`/`evaluate_route` 使用请求捕获的同一个 `RuntimeSnapshot` 和同一次加载的 Policy 资源状态。资源刷新后，新请求使用新 matcher 与 content hash；已开始请求继续使用旧 `Arc`，不加全局读锁。

PolicyIndex 与 ResourceRegistrySnapshot 的组合由 Runtime 构建并原子发布，不能单独替换到互不匹配的 revision。

## 11. 错误语义

- prepare 阶段：重复 client、冲突 CIDR、缺失 strategy/upstream/resource、非法 selector 直接失败；
- 请求阶段：正常不匹配使用 default upstream；
- snapshot 不变量破坏返回 internal，DNS Core 映射 SERVFAIL；
- regex 或 matcher 不得在热路径产生 panic；
- client 信息缺失不是错误，进入 `unknown`。

## 12. 契约验证要求

- ID 优先于 CIDR、IPv4/IPv6 最长前缀、unknown；
- 冲突在 prepare 阶段拒绝；
- base strategy 与 client override；
- DoH canonical route ID 选择，尾部 `{client_id}` 的裸路径不依赖 client ID 重建；
- listener hosts、strategy first-match、default upstream；
- exact/suffix/wildcard/regex 优先级；
- `resource:selector` 解析和不存在错误；
- cache tri-state、TTL 和 ECS 全覆盖矩阵；
- `prepare_context` 不执行逐规则 matcher，fast miss 才进入 `evaluate_route`；
- policy/request/target/ECS fingerprint 的稳定性、模式隔离和敏感字段排除；
- 同一 snapshot 下决策确定性；
- 资源 swap 后 matcher 与 hash 同步切换，新旧请求各自保持一致。

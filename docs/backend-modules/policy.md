# Policy 模块设计

> 状态：v1 方案已完成，已实现 client/strategy/route immutable index、const/file resource loader 接线、请求级 rule/hosts ResolutionPlan 首轮组合、hosts/plain HTTP DoH direct registry wiring 和注入式 DoH request path；Runtime 已保存资源摘要并由 service 捕获同 revision core，资源原子 reload 尚未实现
>
> 更新日期：2026-09-01
>
> 目标代码：`backend/src/policy/*`
>
> 上位设计：[后端架构](../backend-architecture.md) · [配置字段参考](../configuration-reference.md)
>
> 相关方案：[DNS Core](dns-core.md) · [Resource](resource.md) · [Upstream](upstream.md)

## 1. 职责

Policy 模块把已解析配置和资源 snapshot 编译成纯内存决策索引，并为每个请求生成唯一 `ResolutionPlan`。

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
| `plan.rs` | client override、cache/TTL/ECS 生效值与请求级 ResolutionPlan 组合 |

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
- 用于观测的稳定、低基数 ID。

编译发生在 prepare/resource update，不在请求时解析字符串引用。

当前首轮实现已经提供：

- `ClientIndex`：exact client ID 优先，未命中时按 IPv4/IPv6 最长 CIDR 前缀匹配，最后进入 `Unknown`；重复 ID/CIDR 和空规则在构建时拒绝；
- `StrategyIndex`：将已解析策略编译为不可变 `BTreeMap<ConfigId, Arc<ResolvedStrategy>>`，重复策略 ID 在构建时拒绝；
- `RouteIndex`：编译 stream listener 与 DoH route，校验 `{client_id}` segment 模板并保留 typed listener/route 选择结果；
- `PolicyIndex::evaluate`：组合 client strategy override、cache tri-state、TTL/ECS effective value 和 upstream target，输出不可变 `ResolutionPlan`；
- `PolicyIndex::from_config`：通过 Resource loader 编译 const/file hosts 与 JSON/Clash rule-set；remote/dat/selector/缺失资源在边界返回显式错误；
- rule/hosts 执行：固定 listener hosts → strategy rule 顺序，输出不含原文的 matched-rule 摘要，并覆盖 local hosts、rule-set upstream 与 rule ECS；
- `PolicyDnsCore::UpstreamRuntime`：direct hosts/plain HTTP DoH connector 统一由 `UpstreamRegistry` 构造，Unsupported DoH 能力在 prepare 边界向上游构建错误传播；
- 这些索引只持有已解析 typed 值，不在请求路径读取 YAML 或执行网络 I/O。

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

匹配结果包含 client rule ID 与实际 identity。cache namespace 使用实际 client ID 的不可逆摘要，或规范化 IP 的受控表示，不只使用 client rule name。

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

`rule_set` 引用先尝试完整资源名；完整名不存在且包含 `:` 时，才按第一个 `:` 解释为 `resource:selector`。资源名大小写敏感；selector 必须是非空 ASCII 标识并归一化为小写。selector 只对支持子集的格式有效，不存在或格式不支持时在 prepare 阶段失败。

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

`disabled` 是明确结果，不继续继承。`custom` 必须已有合法 CIDR。

## 9. ResolutionPlan

输出为不可变值，包含：

- client bucket 和 identity digest；
- strategy、matched rule/resource IDs；
- `LocalAnswer` 或 typed `UpstreamTarget`；
- effective ECS；
- `CacheDecision`：disabled 或唯一 namespace；
- effective TTL override；
- runtime/resource revision 摘要；
- 决策 trace 的安全 ID。

Plan 不含原始配置字符串、regex 文本、SecretRef、HTTP URL 或具体 connector。

## 10. 一致性

一次 evaluate 使用请求捕获的同一个 `RuntimeSnapshot` 和其中同一个 ResourceRegistrySnapshot。资源刷新后，新请求使用新索引；已开始请求继续使用旧 `Arc`，不加全局读锁。

PolicyIndex 与 ResourceRegistrySnapshot 的组合由 Runtime 构建并原子发布，不能单独替换到互不匹配的 revision。

## 11. 错误语义

- prepare 阶段：重复 client、冲突 CIDR、缺失 strategy/upstream/resource、非法 selector 直接失败；
- 请求阶段：正常不匹配使用 default upstream；
- snapshot 不变量破坏返回 internal，DNS Core 映射 SERVFAIL；
- regex 或 matcher 不得在热路径产生 panic；
- client 信息缺失不是错误，进入 `unknown`。

## 12. 测试

- ID 优先于 CIDR、IPv4/IPv6 最长前缀、unknown；
- 冲突在 prepare 阶段拒绝；
- base strategy 与 client override；
- listener hosts、strategy first-match、default upstream；
- exact/suffix/wildcard/regex 优先级；
- `resource:selector` 解析和不存在错误；
- cache tri-state、TTL 和 ECS 全覆盖矩阵；
- 同一 snapshot 下决策确定性；
- 资源 swap 后新旧请求各自保持一致。

## 13. 实现检查清单

- [x] 实现 client ID/CIDR 索引；
- [x] 建立 strategy immutable lookup index；
- [x] 实现 strategy/route 编译；
- [x] 实现 rule/hosts/resource matcher 编排；
- [x] 实现覆盖矩阵与 ResolutionPlan（首轮 cache/TTL/ECS/client override）；
- [x] 将 direct hosts/plain HTTP DoH connector 通过 `UpstreamRegistry` 接入 `PolicyDnsCore`；
- [x] 提供 protocol-neutral registry 注入入口并验证 DoH request path；
- [ ] 接入 Runtime snapshot 原子发布；
- [x] 完成冲突、优先级、未知资源和 file loader 测试；
- [ ] 完成资源 swap、跨 transport contract 和完整覆盖矩阵测试。

阶段证据：Policy focused tests 12 项通过，覆盖 client strategy/cache 兼容、listener hosts 优先、strategy rule 顺序、rule-set upstream、缺失资源、const/file loader，以及 ConfigLoader 生成的 disabled ECS、direct plain HTTP DoH registry wiring、注入式 DoH request path、基础 Cache/Core 命中、snapshot-local optimistic refresh 和 unsupported feature propagation；当前 backend 全量测试为 372 passed、0 failed。Runtime 已提供资源摘要并由 service 捕获同 revision core，但仍未接入 Runtime ResourceRegistrySnapshot 的原子 reload、最新 snapshot refresh 和完整 late-window/nested sink 传播，也未完成真实网络的完整 DNS Core→Policy→Cache→Upstream 请求管线。

当前实现进度：**55%**（client/strategy/route immutable index、const/file resource loader、rule/hosts matcher 编排、请求级 plan 首轮组合、注入式 direct DoH request path、基础 Cache/Core request path 和当前 snapshot-local optimistic refresh；Runtime snapshot 原子 reload、最新 snapshot refresh、fast-positive late sink、完整 late-window/nested sink 传播、remote/dat selector、完整覆盖矩阵和跨 transport contract tests 未完成）。

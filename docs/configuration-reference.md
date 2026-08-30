# FluxDNS 配置字段参考

> 状态：v1 配置模板草案
> 依据：[config-example.yaml](../config-example.yaml)
> 适用范围：本文与当前模板同步，描述配置契约和校验意图。运行时尚未实现时，本文没有定义的行为不应视为已支持。

## 1. 配置模型概览

FluxDNS 的配置由命名资源和引用关系组成。请求入口选择策略，策略按顺序匹配规则，再决定使用本地 hosts 或远程上游：

```text
listener
  ├─ udp/tcp → strategy
  └─ doh → routes → strategy
                         ├─ hosts：本地回答
                         └─ rule_set → upstream / group → outbound
```

- `listener` 定义请求入口。一个普通 listener 可以展开为多个 IPv4/IPv6 socket。
- 一个 `type: doh` listener 是一个逻辑 DoH 服务，可通过多个 `endpoints` 同时提供不同绑定方式。
- `routes` 在同一个 DoH listener 内共享；每条路由选择一个策略。
- `clients` 按客户端标识或 IP 网段选择策略，并覆盖缓存、TTL 和 ECS 等设置。
- `upstreams`、`hosts`、`rule_set` 和 `outbound` 是可复用的命名资源。

## 2. 通用约定

### 2.1 版本

顶层 `version` 是配置 schema 版本。当前模板为 `version: 1`；字段重命名、删除和迁移规则以该字段为边界。加载器应拒绝不支持的版本，而不是静默按旧字段解释。

### 2.2 路径和工作目录

`work.path` 是独立的工作目录，v1 要求它是绝对路径，不能相对于配置文件目录解释。

- 启动时若配置文件所在目录不是 `work.path`，程序将配置复制到 `<work.path>/config.yaml`，固定文件名为 `config.yaml`。
- 该文件是工作目录中的配置快照；启动流程负责创建工作目录和所需父目录。
- 除明确写成绝对路径的字段外，其他相对路径均以 `work.path` 为基准。
- `work.rules_path`、数据库、日志、缓存、TLS 证书和资源文件路径都遵循上述规则。

配置副本的原子替换、权限和已有文件处理细节仍需在运行时实现时固定，但不得改变上述路径和文件名契约。

### 2.3 命名与引用

`listener`、`upstreams`、`strategy`、`hosts`、`outbound` 和 `rule_set` 中的 `name` 是资源标识，同一集合内必须唯一。引用字段必须指向对应集合中存在且类型正确的名称。

| 引用字段 | 目标集合或语法 |
| --- | --- |
| `listener[].strategy`、`clients[].strategy`、`listener[type=doh].routes[].strategy` | `strategy[].name` |
| `listener[].hosts`、`strategy[].rules[].hosts` | `hosts[].name` |
| `strategy[].rules[].rule_set` | `rule_set[].name`，或 `geosite:cn` 这类规则集子选择器 |
| `strategy[].rules[].upstream`、`strategy[].default_upstream` | `upstreams[].name` |
| `upstreams[].bootstrap` | `upstreams[].name` |
| `upstreams[type=group].upstreams[].name`、`fallbacks[].name` | `upstreams[].name` |
| `upstreams[].proxy`、`rule_set[].proxy` | `outbound[].name` |

应在加载阶段检查名称唯一性、引用存在性、引用类型和循环引用。

### 2.4 值类型

| 数据类型 | 模板示例 | 说明 |
| --- | --- | --- |
| 时长 | `10s`、`24h`、`1d` | 用于缓存、超时和定时更新；解析为 duration，而不是整数天数或秒数。 |
| 字节数 | `8388608` | `max_size_bytes` 使用整数，单位为字节。 |
| IP/CIDR | `192.168.1.0/24`、`fe80::/10` | 用于监听绑定、客户端匹配、ECS 和可信代理。 |
| URL | `https://dns.google/dns-query` | 必须按字段要求使用合法 URL。 |
| SecretRef | `{env: FLUXDNS_OUTBOUND_SG_URL}` | 从环境变量或文件取得完整敏感值，不能把真实凭据写入模板。 |

`outbound[].proxy_url` 是 SecretRef 对象，不是明文 URL 字符串。当前契约要求 `env` 与 `file` 二选一；读取结果应为完整代理 URL。

### 2.5 覆盖和继承

配置块未出现时继承较低优先级的设置；块中只出现的字段按字段继承。显式的禁用值用于停止继承。

#### ECS 优先级

当多个层级提供 `edns_client_subnet` 时，优先级由高到低为：

```text
strategy.rules[].edns_client_subnet
  > strategy[].edns_client_subnet
  > clients[].edns_client_subnet
  > upstreams[].edns_client_subnet
  > dns.edns_client_subnet
```

`mode` 的取值为 `disabled`、`client`、`custom`：

- `disabled` 显式停止当前请求继续向上游传递 ECS；
- `client` 优先使用请求携带的 ECS，没有时按客户端地址推导前缀；
- `custom` 使用 `custom_ip`，因此 `custom_ip` 必填。

#### 缓存和 TTL

缓存和 TTL 覆写是两个独立配置：

```text
clients[].cache > strategy[].cache > dns.cache
clients[].ttl_override > strategy[].ttl_override > dns.ttl_override
```

`ttl_override.enabled: false` 显式关闭当前层级的 TTL 覆写；它不等价于关闭缓存。`ttl_override.min` 或 `max` 为 `0s` 表示该边界不设限。

## 3. 顶层字段

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `version` | integer | 配置 schema 版本；当前必须为 `1`。 |
| `work` | object | 工作目录和规则资源落盘目录。 |
| `database` | object | 解析日志等持久化能力使用的数据库。 |
| `logs` | object | 服务日志输出。 |
| `webui` | object | Web 管理界面及登录用户。 |
| `dns` | object | 全局缓存、TTL、ECS 和解析日志默认值。 |
| `listener` | array | UDP、TCP 和 DoH 请求入口。 |
| `upstreams` | array | hosts 上游、DoH 上游和上游组。 |
| `strategy` | array | 策略和有序路由规则。 |
| `hosts` | array | 可被入口或策略复用的本地 hosts 资源。 |
| `outbound` | array | 远程上游或规则下载使用的代理出口。 |
| `rule_set` | array | 可被策略引用的规则资源。 |
| `clients` | array | 客户端匹配和客户端级覆盖。 |

## 4. `work`

| 字段 | 类型 | 条件 | 说明 |
| --- | --- | --- | --- |
| `work.path` | string | 必填 | 必须是绝对路径；不随启动配置文件位置变化。 |
| `work.rules_path` | string | 必填 | 规则资源落盘目录；相对路径以 `work.path` 为基准。 |

启动时，如果配置文件不在 `work.path` 目录中，应先确保工作目录存在，再将其复制为 `<work.path>/config.yaml`。数据库、日志、缓存和证书等其他相对路径仍以 `work.path` 为基准。

## 5. `database`

| 字段 | 类型 | 条件 | 说明 |
| --- | --- | --- | --- |
| `database.type` | string | 必填 | 当前模板仅定义 `sqlite`。未知类型应拒绝。 |
| `database.path` | string | 必填 | SQLite 数据库文件路径；相对路径以 `work.path` 为基准，父目录由程序创建。 |

当 `dns.resolve_log.enable` 为 `true` 时，数据库必须可用。`database.path` 表示文件，不表示目录。

## 6. `logs`

| 字段 | 类型 | 条件 | 说明 |
| --- | --- | --- | --- |
| `logs.enable` | boolean | 必填 | 是否启用服务日志。 |
| `logs.level` | string | 必填 | 日志级别；模板示例为 `info`，未知值应拒绝。 |
| `logs.path` | string | 必填 | 日志文件路径；相对路径以 `work.path` 为基准。 |

## 7. `webui`

| 字段 | 类型 | 条件 | 说明 |
| --- | --- | --- | --- |
| `webui.enable` | boolean | 必填 | 是否启用 Web 管理界面。 |
| `webui.address` | string | 必填 | Web UI 监听地址；模板默认绑定 `127.0.0.1`。 |
| `webui.port` | integer | 必填 | Web UI 监听端口。 |
| `webui.users` | array | `enable: true` 时至少一项 | 登录用户列表；当前版本没有匿名模式。 |
| `webui.users[].name` | string | 必填 | 用户名；列表内必须唯一。 |
| `webui.users[].password_hash` | string | 必填 | 初始化时由一次性明文密码生成的单向 hash。 |

配置中禁止出现 `webui.users[].password`。初始化流程只接收一次明文密码并写入 `password_hash`，运行时只执行密码校验；hash 不能直接反向还原密码，但弱密码仍可能被离线猜测。真实密码和 hash 不应提交到公开仓库。

## 8. `dns`

### 8.1 `dns.cache`

`cache` 只描述缓存是否启用、乐观缓存和全局持久化；TTL 覆写不再嵌套在其中。

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `dns.cache.enabled` | boolean | 是否启用 DNS 缓存。 |
| `dns.cache.optimistic.enabled` | boolean | 是否允许返回已过期记录并在后台刷新。 |
| `dns.cache.optimistic.answer_ttl` | duration | 乐观缓存应答使用的 TTL。 |
| `dns.cache.optimistic.max_age` | duration | 记录过期后仍可乐观返回的最长时间。 |
| `dns.cache.persistence.path` | string | 持久化缓存文件路径；相对路径以 `work.path` 为基准。 |
| `dns.cache.persistence.max_size_bytes` | integer | 持久化缓存文件最大大小，单位为字节。 |

策略级和客户端级 `cache` 只允许 `enabled` 与 `optimistic` 子对象，不包含 `persistence`。

### 8.2 `dns.ttl_override`

`ttl_override` 与 `cache` 平级，用于限制返回给客户端的 DNS TTL：

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `dns.ttl_override.enabled` | boolean | 是否启用当前层级的 TTL 覆写。 |
| `dns.ttl_override.min` | duration | 最小 TTL；`0s` 表示不设下限。 |
| `dns.ttl_override.max` | duration | 最大 TTL；`0s` 表示不设上限。 |

策略和客户端可使用同样的字段结构。省略字段按层级继承，显式 `enabled: false` 停止当前层级的 TTL 覆写。

### 8.3 `dns.edns_client_subnet`

| 字段 | 类型 | 条件 | 说明 |
| --- | --- | --- | --- |
| `mode` | enum | 必填 | `disabled`、`client` 或 `custom`。 |
| `custom_ip` | CIDR | `mode: custom` 时必填 | 要发送给上游的 ECS 网段。 |

全局 ECS 设置可由上游、客户端、策略和规则覆盖，详见[覆盖和继承](#25-覆盖和继承)。

### 8.4 `dns.resolve_log`

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `enable` | boolean | 是否记录解析请求、策略、规则、ECS、缓存、上游结果和耗时等信息。 |
| `max_records` | integer | 最多保留的记录数量。 |
| `max_record_age` | duration | 记录最长保留时间，例如 `7d`。 |

启用解析日志依赖可用的 `database`。

## 9. `listener[]`

一个 listener 是一个逻辑服务。`addresses` 中的每个地址由运行时展开为独立 socket；同一个逻辑 listener 的地址共享协议和端口。

### 9.1 UDP/TCP listener

| 字段 | 类型 | 条件 | 说明 |
| --- | --- | --- | --- |
| `name` | string | 必填 | 入口唯一名称。 |
| `type` | enum | 必填 | `udp` 或 `tcp`。 |
| `addresses` | array[string] | 必填 | IPv4/IPv6 绑定地址列表，例如 `[0.0.0.0, "::"]`。 |
| `port` | integer | 必填 | 所有地址共用的监听端口。 |
| `strategy` | string | 必填 | 默认策略，引用 `strategy[].name`。 |
| `hosts` | string | TCP 模板示例中可选 | 入口级本地 hosts 资源；命中时本地回答，未命中再使用 `strategy`。 |

通过把 IPv4 和 IPv6 地址放入同一个 `addresses` 数组，可以用一个逻辑 listener 表达双栈入口。实现必须明确 IPv6 socket 的 v6-only 行为，避免 `0.0.0.0` 与 `::` 展开后的绑定重叠。

### 9.2 DoH listener

DoH 不使用普通 listener 的顶层 `address`、`port`、`strategy` 或单个 `path` 字段，而是由共享路由和 endpoint 组成：

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `name` | string | 逻辑 DoH listener 名称，必须唯一。 |
| `type` | enum | 固定为 `doh`。 |
| `routes` | array | 所有 endpoint 共享的 HTTP 路由列表。 |
| `endpoints` | array | 一个或多个绑定端点；每个 endpoint 独立处理 TLS 和客户端 IP 来源。 |

#### `listener[type=doh].routes[]`

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `path` | string | HTTP 路径，可使用 `{client_id}` 占位符。 |
| `strategy` | string | 路由命中的策略，引用 `strategy[].name`。 |

模板示例使用 `/dns/inner/{client_id}` 和 `/dns/outside/{client_id}`。`/dns-quer/inner*` 与 `/dns-quer/outside*` 是服务保留路径，不能作为自定义路由；它们与示例中的 `/dns/...` 前缀不是同一组路径。

#### `listener[type=doh].endpoints[]`

| 字段 | 类型 | 条件 | 说明 |
| --- | --- | --- | --- |
| `name` | string | 必填 | endpoint 名称，在同一 DoH listener 内唯一。 |
| `addresses` | array[string] | 必填 | endpoint 的 IPv4/IPv6 绑定地址列表。 |
| `port` | integer | 必填 | endpoint 监听端口。 |
| `tls` | object | 必填 | 该 endpoint 的 TLS 所有权。 |
| `client_ip` | object | 必填 | 请求客户端 IP 的来源和可信边界。 |

同一个逻辑 DoH listener 可以同时包含以下两类 endpoint，而不需要创建两个顶层 DoH 配置：

- `tls.mode: terminate`：FluxDNS 自己完成 TLS 握手和解密，必须提供证书链和私钥文件；
- `tls.mode: external`：Nginx 等反向代理已经终止 TLS，FluxDNS 接收内部 HTTP，证书字段禁止出现。

#### `endpoints[].tls`

| 字段 | 类型 | 条件 | 说明 |
| --- | --- | --- | --- |
| `mode` | enum | 必填 | `terminate` 或 `external`。 |
| `certificate_file` | string | `terminate` 时必填 | 证书链文件路径，相对路径以 `work.path` 为基准。 |
| `private_key_file` | string | `terminate` 时必填 | 私钥文件路径，相对路径以 `work.path` 为基准。 |

`external` 模式不得配置 `certificate_file` 或 `private_key_file`。反向代理占用公网端口时，external endpoint 通常绑定本机或内网端口，例如模板中的 `127.0.0.1:8080`。

#### `endpoints[].client_ip`

| 字段 | 类型 | 条件 | 说明 |
| --- | --- | --- | --- |
| `source` | enum | 必填 | `peer`、`forwarded_header` 或 `proxy_protocol`。 |
| `header` | string | `forwarded_header` 时必填 | 允许 `X-Forwarded-For`、`X-Real-IP` 或 `Forwarded`。 |
| `trusted_proxies` | array[CIDR] | `forwarded_header`/`proxy_protocol` 时必填 | 可信的反代对端网段，不是最终客户端网段。 |
| `on_missing` | enum | `forwarded_header` 时可选 | Header 缺失时 `reject` 或 `use_peer`。 |
| `on_invalid` | enum | `forwarded_header` 时可选 | Header 非法时 `reject` 或 `use_peer`。 |

`peer` 直接使用 TCP 对端地址。`forwarded_header` 只接受来自 `trusted_proxies` 的请求；转发链应从右到左解析，并且反向代理必须先清理客户端自带的伪造 Header。`proxy_protocol` 也必须配置 `trusted_proxies`，只信任代理对端发送的 PROXY 信息。

### 9.3 绑定校验

`listener[].addresses` 和 `listener[type=doh].endpoints[].addresses` 展开后不得产生相同协议、地址、端口的冲突。IPv4/IPv6 双栈是否共享 socket、以及 v6-only 行为必须在运行时明确。

当前模板没有 `dot` listener 字段；DoT 应在协议、证书材料和握手校验契约确定后再加入。

## 10. `upstreams[]`

`upstreams` 中每个资源由 `type` 决定允许的字段集合。未知字段和不属于该类型的字段都应拒绝。

### 10.1 公共字段

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `name` | string | 上游唯一名称。 |
| `type` | enum | `hosts`、`doh` 或 `group`。 |

### 10.2 `type: hosts`

| 字段 | 类型 | 条件 | 说明 |
| --- | --- | --- | --- |
| `format` | string | 必填 | 内容格式；按该格式严格解析。模板示例为 `json`。 |
| `hosts` | string | 必填 | 内联 hosts 数据。 |

该类型可作为其他 DoH 上游的 `bootstrap`，用于避免首次解析依赖系统默认 DNS。内联内容格式错误应报告字段路径。

### 10.3 `type: doh`

| 字段 | 类型 | 条件 | 说明 |
| --- | --- | --- | --- |
| `address` | URL | 必填 | DoH 服务 URL。 |
| `bootstrap` | string | 可选 | 引导解析 `address` 主机名的上游名称。 |
| `connect_ip` | IP | 可选 | 实际连接目标 IP；不改变 HTTP `Host` 或 TLS SNI。 |
| `proxy` | string | 可选 | 代理出口名称，引用 `outbound[].name`。 |
| `edns_client_subnet` | object | 可选 | 上游级 ECS 覆盖。 |

`bootstrap` 和 `connect_ip` 解决的是连接建立路径：前者引用上游完成域名解析，后者直接指定连接 IP。二者都不表示修改远程服务的 HTTP Host 或 TLS SNI。

### 10.4 `type: group`

| 字段 | 类型 | 条件 | 说明 |
| --- | --- | --- | --- |
| `upstreams` | array[object] | 必填 | 主成员列表，每项为 `{name, weight}`。 |
| `upstream_mode` | enum | 必填 | `parallel`、`round-robin`、`load-balance` 或 `failover`。 |
| `timeout` | duration | 必填 | 主组总超时时间。 |
| `fallbacks` | array[object] | 可选 | 主组全部失败后的回退成员，每项同样为 `{name, weight}`。 |
| `fallback_upstream_mode` | enum | 有 `fallbacks` 时必填 | 回退成员选择模式。 |
| `fallback_timeout` | duration | 有 `fallbacks` 时必填 | 回退组总超时时间。 |
| `upstreams[].name` / `fallbacks[].name` | string | 必填 | 成员上游名称。 |
| `upstreams[].weight` / `fallbacks[].weight` | positive integer | 必填 | 正整数权重。 |

组成员使用对象而不是 `name:weight` 字符串，避免字符串解析歧义。`parallel` 表示并发尝试，`round-robin`、`load-balance` 和 `failover` 的精确轮询、权重和失败判定算法仍需在运行时定稿；`fallbacks` 只在主组失败后使用。

## 11. `strategy[]`

策略定义规则顺序、默认上游以及策略级覆盖。

| 字段 | 类型 | 条件 | 说明 |
| --- | --- | --- | --- |
| `name` | string | 必填 | 策略唯一名称。 |
| `rules` | array | 必填 | 有序规则列表；从上到下匹配，命中第一条后停止。 |
| `default_upstream` | string | 必填 | 没有规则命中时使用的上游名称。 |
| `cache` | object | 可选 | 策略级缓存覆盖，高于 `dns.cache`。 |
| `ttl_override` | object | 可选 | 策略级 TTL 覆写，与 `cache` 平级。 |
| `edns_client_subnet` | object | 可选 | 策略级 ECS 覆盖。 |

### 11.1 `strategy[].rules[]`

每条规则必须且只能提供一个匹配目标：`rule_set` 与 `hosts` exactly-one-of。

| 字段 | 类型 | 条件 | 说明 |
| --- | --- | --- | --- |
| `rule_set` | string | 与 `hosts` 二选一 | 规则集名称或子选择器，例如 `geosite:cn`。 |
| `hosts` | string | 与 `rule_set` 二选一 | 本地 hosts 资源名称。 |
| `upstream` | string | `rule_set` 规则通常必填 | 命中规则集后使用的上游；引用 `upstreams[].name`。 |
| `edns_client_subnet` | object | 可选 | 规则级 ECS 覆盖，优先级最高。 |

校验约束：

- `rule_set` 和 `hosts` 不能同时出现，也不能同时缺失；
- `rule_set` 规则应提供 `upstream`；
- `hosts` 规则是本地回答，不应依赖 `upstream`；
- 所有资源引用必须存在且类型正确。

### 11.2 策略级 `cache`

策略级缓存只允许以下字段，不包含全局持久化设置：

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `enabled` | boolean | 是否在该策略启用缓存。 |
| `optimistic.enabled` | boolean | 是否允许过期记录先返回并异步刷新。 |
| `optimistic.answer_ttl` | duration | 乐观缓存应答 TTL。 |
| `optimistic.max_age` | duration | 过期记录可被乐观返回的最长时间。 |

策略级 `ttl_override` 使用 [`dns.ttl_override`](#82-dnsttl_override) 的相同字段结构和继承语义。

## 12. `hosts[]`

顶层 `hosts` 是可被 listener 或策略规则直接命中的本地回答资源；它不同于 `upstreams[type=hosts]`，后者是一个解析上游资源。

| 字段 | 类型 | 条件 | 说明 |
| --- | --- | --- | --- |
| `name` | string | 必填 | hosts 资源唯一名称。 |
| `type` | enum | 必填 | `const` 或 `file`。 |
| `format` | enum | 必填 | `json` 或 `hosts`；内容必须按格式严格解析。 |
| `hosts` | string | `type: const` 时必填 | 内联 hosts 内容。 |
| `path` | string | `type: file` 时必填 | 本地 hosts 文件路径，相对路径以 `work.path` 为基准。 |
| `auto_update` | boolean | `type: file` 时可选 | 是否自动重载本地文件。 |
| `update_interval` | duration | `auto_update: true` 时必填 | 自动重载间隔。 |

文件型 hosts 的 `auto_update` 只表示本地文件重载，不表示远程同步。

## 13. `outbound[]`

`outbound` 定义访问远程上游或远程规则集时使用的代理出口。

| 字段 | 类型 | 条件 | 说明 |
| --- | --- | --- | --- |
| `name` | string | 必填 | 出口唯一名称。 |
| `type` | enum | 必填 | 当前模板仅展示 `socks5`。 |
| `proxy_url` | SecretRef object | 必填 | 通过 `env` 或 `file` 取得完整代理 URL，二选一。 |

SecretRef 的实际值可能包含用户名、密码或令牌；不要将环境变量内容、文件内容或真实 URL 写入 Git 跟踪文件。其他代理类型尚未形成配置契约。

## 14. `rule_set[]`

规则集是策略匹配的数据源。资源可以内联、来自本地文件或从远程 URL 下载。

| 字段 | 类型 | 条件 | 说明 |
| --- | --- | --- | --- |
| `name` | string | 必填 | 规则集唯一名称，也可作为落盘文件名前缀。 |
| `type` | enum | 必填 | `const`、`file` 或 `remote`。 |
| `format` | enum | 必填 | 模板示例为 `json`、`clash`、`dat`。 |
| `rule` | string | `type: const` 时必填 | 内联规则内容。 |
| `path` | string | `type: file` 时必填 | 本地规则文件路径，相对路径以 `work.path` 为基准。 |
| `url` | URL | `type: remote` 时必填 | 远程规则下载地址。 |
| `proxy` | string | `type: remote` 时可选 | 下载远程规则使用的出口，引用 `outbound[].name`。 |
| `auto_update` | boolean | `type: file` 或 `remote` 时可选 | 是否定期刷新或重载资源。 |
| `update_interval` | duration | `auto_update: true` 时必填 | 刷新/重载间隔。 |

`format: clash` 表示 Clash 行格式，不是标准 YAML。远程资源在下载成功且格式校验通过后才能替换当前内容；失败时应保留旧资源并返回带字段路径的错误。模板暂未提供远程版本锁定或 checksum 字段，生产部署的固定版本策略需后续定稿。

`geosite:cn` 这类写法表示引用 `dat` 地理规则集中的命名子集；完整选择器语法、大小写规则和不存在时的失败语义仍需固定。

## 15. `clients[]`

客户端规则按请求级选择策略，并提供客户端级缓存、TTL 和 ECS 覆盖。

| 字段 | 类型 | 条件 | 说明 |
| --- | --- | --- | --- |
| `name` | string | 必填 | 客户端规则名称，列表内唯一。 |
| `match` | object | 必填 | 客户端匹配条件，至少包含 `ids` 或 `ips`。 |
| `match.ids` | array[string] | 可选 | 精确匹配 DoH `client_id` 或其他协议提供的客户端标识。 |
| `match.ips` | array[CIDR] | 可选 | 按客户端 IP/CIDR 匹配。 |
| `strategy` | string | 可选 | 命中后使用的策略；省略时继承 listener 或 DoH route 的策略。 |
| `cache` | object | 可选 | 客户端级缓存覆盖，优先级最高。 |
| `ttl_override` | object | 可选 | 客户端级 TTL 覆写，与 `cache` 平级。 |
| `edns_client_subnet` | object | 可选 | 客户端级 ECS 覆盖。 |

匹配优先级固定为：先精确 `id`，再按 IP 的最长 CIDR 前缀。在同一优先级出现多个冲突规则时应在配置校验阶段报错，而不是依赖数组顺序。`match.ids` 和 `match.ips` 至少提供一个；`id`、`ip` 不再是顶层客户端字段。

客户端级 `cache`、`ttl_override` 和 `edns_client_subnet` 分别遵循[覆盖和继承](#25-覆盖和继承)中的层级规则。

## 16. 配置校验清单

建议 v1 配置加载阶段至少执行以下校验：

1. `version` 存在且为 `1`；未知版本拒绝加载。
2. 拒绝未知字段；每种 `type` 只允许自己的字段集合，条件字段满足 exactly-one-of/required-if 约束。
3. 所有资源集合中的 `name` 唯一，所有引用存在、类型正确且无循环引用。
4. `work.path` 是绝对路径；目录不存在时创建；启动配置不在该目录时复制为 `<work.path>/config.yaml`。
5. `listener.addresses` 和 DoH endpoint 地址展开后不存在 bind 冲突，并明确 IPv6 v6-only 行为。
6. `mode`、`format`、`type`、端口、CIDR、URL、duration、权重和内嵌内容格式合法。
7. `ttl_override` 与 `cache` 平级，不能把 TTL 字段写入 `cache`；未配置字段按层级继承。
8. ECS 块未配置时继承，显式 `mode: disabled` 才停止继续传递。
9. `webui.users[].password_hash` 必须是受支持算法生成的单向 hash，禁止明文 `password` 字段。
10. DoH endpoint 的 `tls.mode` 独立校验：`terminate` 必须有证书和私钥，`external` 不得有证书字段。
11. `forwarded_header`/`proxy_protocol` 必须配置 `trusted_proxies`，且可信范围只覆盖反代对端；Header 来源和失败动作合法。
12. `dns.resolve_log.enable: true` 时数据库可用；远程资源加载或校验失败提供带字段路径的错误，并保留旧资源。

## 17. 实现前仍需定稿的语义

以下项目没有在当前模板中形成完整契约，不应通过猜测扩展字段：

1. `dot` listener 的字段、TLS 材料来源和协议特有校验。
2. 日志级别、资源格式、代理类型等枚举的完整列表及未知值错误码。
3. 上游组 `round-robin`、`load-balance` 和 `failover` 的精确算法、权重计算和失败判定。
4. 远程规则的版本锁定、checksum、首次下载失败和更新并发控制。
5. 配置副本覆盖时的原子替换、文件权限和恢复策略。
6. 配置热加载、数据库迁移和未来 schema 版本的兼容策略。

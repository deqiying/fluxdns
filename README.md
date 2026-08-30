# FluxDNS

面向策略分流的 DNS 服务。

FluxDNS 计划为 DNS 请求提供基于域名、客户端、规则集和上游健康状态的解析决策，并支持常见的 DNS 传输方式。

## 项目状态

项目目前处于命名和设计阶段，尚未开始正式实现。

## 目标

- 提供传统 DNS 和 DNS over HTTPS（DoH）服务；DNS over TLS（DoT）是后续版本目标。
- 根据域名、客户端与规则集，将请求分流到合适的解析策略和上游。
- 支持本地与远程规则集、缓存、上游组与故障回退。
- 保持配置清晰，并在策略或上游不可用时提供可诊断的失败信息。

## 配置

[config-example.yaml](config-example.yaml) 是配置草案。实际运行时可复制为 `config.yaml` 后按部署环境调整；`config.yaml`、规则文件、运行数据、缓存与日志均不会被 Git 跟踪。

字段语义见 [docs/configuration-reference.md](docs/configuration-reference.md)，Rust 后端实现基线见 [docs/backend-architecture.md](docs/backend-architecture.md)。

配置中的示例地址、账号、密码和客户端标识仅用于说明格式，部署前必须替换为实际且受保护的配置。

## 命名约定

- 项目与仓库目录：`FluxDNS`
- 二进制与服务标识：`fluxdns`
- 项目描述：`A policy-driven DNS server`

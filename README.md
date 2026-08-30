# FluxDNS

面向策略分流的 DNS 服务。

FluxDNS 计划为 DNS 请求提供基于域名、客户端和规则集的解析决策，并支持常见的 DNS 传输方式；主动上游健康检查属于后续版本能力。

## 项目状态

后端总体架构和 12 个模块方案已经完成，代码实现尚未开始。当前后端实现进度为 0%，包含设计阶段的 v1 交付总进度为 10%。

## 目标

- 提供传统 DNS 和 DNS over HTTPS（DoH）服务；DNS over TLS（DoT）和 DNS over QUIC（DoQ）是后续版本目标。
- 根据域名、客户端与规则集，将请求分流到合适的解析策略和上游。
- 支持本地与远程规则集、缓存、上游组与故障回退。
- 保持配置清晰，并在策略或上游不可用时提供可诊断的失败信息。

## 配置

[config-example.yaml](config-example.yaml) 是配置草案。实际运行时可复制为 `config.yaml` 后按部署环境调整；`config.yaml`、规则文件、运行数据、缓存与日志均不会被 Git 跟踪。

字段语义见 [docs/configuration-reference.md](docs/configuration-reference.md)，Rust 后端总体方案见 [docs/backend-architecture.md](docs/backend-architecture.md)，模块方案、阶段安排和当前进度见 [docs/backend-development-plan.md](docs/backend-development-plan.md)。

配置中的示例地址、账号、密码和客户端标识仅用于说明格式，部署前必须替换为实际且受保护的配置。

## 命名约定

- 项目与仓库目录：`FluxDNS`
- 二进制与服务标识：`fluxdns`
- 项目描述：`A policy-driven DNS server`

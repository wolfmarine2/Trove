# Trove

[English](README.md)

Trove 是一个自托管的通用制品仓库（私有源），设计上参照 Sonatype Nexus Repository：
一台服务同时托管内部发布的包、缓存公共源，并按生态把两者聚合到同一个地址。

整个系统是一个 Rust 单二进制文件，内嵌 React 管理界面。默认使用 SQLite 即可运行，
需要高可用时切换到 PostgreSQL（兼容 openGauss）和 S3 兼容对象存储（MinIO、Ceph RGW）。

| 格式 | Hosted（发布） | Proxy（代理缓存） | Group（聚合） | 已验证客户端 |
| ---- | -------------- | ----------------- | ------------- | ------------ |
| Maven | ✓ 正式版/快照版策略，自动生成校验和 | ✓ | ✓ 合并 `maven-metadata.xml` | Maven 2 HTTP 布局 |
| npm | ✓ publish、dist-tag、unpublish、`npm login` | ✓ | ✓ 合并 packument | npm 10 |
| PyPI | ✓ twine 上传（PEP 503 simple 索引） | ✓ 支持 JSON/HTML 索引，校验 sha256 | ✓ | pip、twine、uv |
| Cargo | ✓ sparse 索引、publish、yank | ✓ crates.io，校验 checksum | ✓ | cargo 1.97 |
| Raw | ✓ 任意文件 HTTP PUT | ✓ | ✓ | curl |
| Docker / OCI | —（推送计划中） | ✓ Docker Hub、GHCR、Quay、Harbor 等，支持 token 鉴权，校验摘要 | ✓ | Registry API v2（docker、containerd） |
| APT | — | ✓ Debian、Ubuntu、麒麟、统信 UOS，支持 by-hash | — | apt 2.7 |
| YUM / DNF | — | ✓ openEuler、RHEL 系、Fedora、EPEL | — | 仓库目录布局（dnf） |

## 特点

* **适合隔离网络**：不依赖任何外部服务和 CDN，字体随包打包。每个代理仓库可单独选择出网
  路径（服务器默认、直连，或带认证的 HTTP/HTTPS/SOCKS5 代理），支持自定义 CA 证书，
  可切换为"只使用缓存"模式。
* **可审计**：登录、发布、删除、配置变更全部写入审计日志，界面可查，同时输出结构化日志。
* **存储省空间**：按内容寻址存储，不同仓库里的相同文件只存一份；垃圾回收清理无引用的数据块。
* **易运维**：提供 `/healthz`、`/readyz`、Prometheus `/metrics`、JSON 日志，以及单节点和
  高可用两套 Kustomize 清单。

## 快速开始

```sh
# 依赖：Rust 1.85+、Node.js 20+
make web      # 构建前端到 web/dist
make run      # 启动于 http://127.0.0.1:8081（admin / admin12345）
```

或使用容器（含 PostgreSQL）：

```sh
docker compose up -d    # http://localhost:8081，admin / change-me-now
```

登录后进入「仓库 → 新建仓库」，创建完成后按仓库页面「客户端配置」里的片段配置构建工具。
每个生态推荐的组织方式：

```
npm-hosted  (hosted) ─┐
npm-proxy   (proxy → https://registry.npmjs.org/) ─┤→ npm-public (group) ← 客户端统一用这个地址
```

在 group 里把 hosted 仓库排在最前面，同名时私有包优先，可防范依赖混淆攻击。

## 文档

* [架构](docs/architecture.md)：crate 划分、存储模型、请求流程、路线图
* [配置](docs/configuration.md)：全部配置项与环境变量覆盖
* [客户端配置](docs/formats.md)：Maven、Gradle、npm、pip、twine、uv、Cargo、curl
* [部署](docs/deployment.md)：Docker、Kubernetes、高可用、备份与升级
* [REST API](docs/api.md)：前端和自动化脚本使用的管理接口

## 目录结构

```
crates/
  trove-core      领域模型、配置、错误类型（不做 I/O）
  trove-db        元数据存储，基于 sqlx 支持 SQLite / PostgreSQL
  trove-storage   按内容寻址的 blob 存储（文件系统 / S3）
  trove-server    HTTP 服务：各格式协议、管理 API、前端托管
web/              React + TypeScript 前端（Vite）
deploy/           Dockerfile 与 Kustomize 清单
config/           配置示例
docs/             文档
```

## 许可证

Apache License 2.0，见 [LICENSE](LICENSE)。

---
title: "部署到生产"
description: "把 awaken-server 从开发二进制带到加固的生产部署:构建、持久化 store、密钥、反向代理 + TLS、健康探针,以及 admin 加固。"
---

本页把在生产环境运行 `awaken-server` 所需的拼图串起来。假设你已经把 agent runtime
接进了 server(见 [Serve & Integrate](/awaken/zh-cn/serve-and-integrate/))。

## 1. 按需开启 feature 构建

编译 release 二进制(或容器镜像),只开启部署用得到的 cargo feature —— 持久化 store
是按 feature 门控的:

```sh
cargo build --release -p your-server-crate \
  --features "server,postgres,observability,a2a"
```

| Feature | 启用 |
|---|---|
| `postgres` | `PostgresStore` + `PgCommitCoordinator`(持久化 config/thread/run) |
| `nats` | NATS 后端 mailbox + buffered thread store,用于多副本 |
| `file` | `FileStore` —— 单节点够用,不适合 HA |
| `observability` / `otel` | runtime stats、Prometheus、OpenTelemetry 导出 |
| `permission` | 工具权限 HITL |
| `a2a` | Agent-to-Agent 后端与路由 |

## 2. 用环境变量配置

| 变量 | 用途 |
|---|---|
| `AWAKEN_HTTP_ADDR` | 绑定地址。生产:代理后用 `0.0.0.0:<port>`(`ServerConfig` 默认 `0.0.0.0:3000`)。 |
| `AWAKEN_ADMIN_API_BEARER_TOKEN` | 保护所有 config/admin 路由的 bearer token。生产**必填**。 |
| `AWAKEN_ADMIN_CORS_ALLOWED_ORIGINS` | 允许从浏览器控制台调用 admin API 的来源(逗号分隔)。 |
| `AWAKEN_STORAGE_DIR` | 文件型 store 的存储目录(开发/单节点)。 |
| `AWAKEN_SEED_PROFILE` | 内置 seed(`minimal` / `demo`)。自己管理配置后,生产通常不设。 |
| `AWAKEN_EXPOSE_TRACE_ROUTES` | 暴露 trace 读取路由。trace 含 prompt/工具参数 —— 只在鉴权之后暴露。 |

Provider 凭据是配置而非代码:在 provider 上设 `api_key`,或用
`adapter_options.allow_env_credentials = true` + 适配器环境变量(如 `VERTEX_API_KEY`)
跑 keyless。见 [Provider & Model 配置](/awaken/zh-cn/reference/provider-model-config/)。

## 3. 使用持久化 store

开发二进制可能跑在内存或文件 store 上;生产不应如此。

- **Config / threads / runs** → Postgres。接入 `PostgresStore` 与
  `PgCommitCoordinator`(所有持久化 runtime 写入都走 commit coordinator)。见
  [使用 Postgres Store](/awaken/zh-cn/how-to/use-postgres-store/)。
- **多副本派发** → NATS mailbox + buffered thread store。见
  [使用 NATS Store](/awaken/zh-cn/how-to/use-nats-stores/)。
- 文件/内存的开发版 coordinator 除非设了 `AWAKEN_ALLOW_DEV_FILE_COORDINATOR` 否则被
  拒绝 —— 生产**不要**设它。

## 4. 管理密钥

- 从平台的密钥管理器以环境变量注入 admin token 与 provider 凭据;绝不烤进镜像或提交。
- 上游支持短时 env token 时,优先用 keyless provider(`allow_env_credentials`),让配置
  里不留长期密钥。
- 定期轮换 admin bearer token 与 provider key。

## 5. 在反向代理处终止 TLS

让 server 跑在私有网卡上,前面放反向代理(nginx、Caddy、Envoy 或云 LB)终止 TLS 并转发
到 `AWAKEN_HTTP_ADDR`。server 走明文 HTTP/SSE,别直接暴露到公网。确保代理**不缓冲** SSE
响应(对 `/v1/**` 流式路由关闭 proxy buffering),否则实时 token 流会卡住。

## 6. 接健康探针

| 端点 | 用途 |
|---|---|
| `GET /health/live` | Liveness —— 进程存活即 200。 |
| `GET /health` | Readiness —— 按关键依赖决定是否放流量。 |
| `GET /metrics` | Prometheus 抓取(需 `observability` feature)。 |

把编排器的 `livenessProbe` 指向 `/health/live`,`readinessProbe` 指向 `/health`。

## 7. 可观测性

启用 observability 插件和一个导出器(Prometheus 或 OTel),让 runtime stats、延迟、
工具/推理指标可见 —— 也让管理控制台的 per-agent 统计能渲染。见
[启用可观测性](/awaken/zh-cn/how-to/enable-observability/)。

## 8. 加固 admin 与 config 面

- 保持 `AWAKEN_ADMIN_API_BEARER_TOKEN` 已设,并把 CORS 收敛到你的控制台来源。
- 把[管理控制台](/awaken/zh-cn/how-to/use-admin-console/)放在你的边缘鉴权之后;它只是同一
  admin API 的浏览器客户端。
- 把 audit log 当作变更记录接入(retention 单独配置),让每次配置写入都可追溯。

## 检查清单

- [ ] 只开必要 feature 的 release 构建
- [ ] 接好 Postgres(多副本再加 NATS);开发版 coordinator **关闭**
- [ ] 从密钥注入 admin bearer token + 收敛 CORS
- [ ] provider 凭据从密钥注入(或 keyless env)
- [ ] 代理处终止 TLS;关闭 SSE 缓冲
- [ ] 接好 liveness `/health/live` + readiness `/health` 探针
- [ ] 启用可观测性导出器;打开 audit log

## 相关

- [Serve & Integrate](/awaken/zh-cn/serve-and-integrate/)
- [通过 HTTP/SSE 暴露](/awaken/zh-cn/how-to/expose-http-sse/)
- [使用 Postgres Store](/awaken/zh-cn/how-to/use-postgres-store/)
- [使用 NATS Store](/awaken/zh-cn/how-to/use-nats-stores/)
- [调优与运营](/awaken/zh-cn/operate/)

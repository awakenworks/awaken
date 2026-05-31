---
title: "使用文件存储"
description: "当你希望在不引入外部数据库的情况下，用文件系统持久化 threads、runs 和 messages 时，使用本页。"
---

当你希望在不引入外部数据库的情况下，用文件系统持久化 threads、runs 和 messages 时，使用本页。

## 前置条件

- `awaken-stores` 启用了 `file` feature

## 步骤

1. 添加依赖：

```toml
[dependencies]
awaken-stores = { git = "https://github.com/AwakenWorks/awaken", features = ["file"] }
```

如果使用 `awaken` 门面 crate，也建议直接加 `awaken-stores` 来启用 `file` feature。

2. 创建 `FileStore`：

```rust
use std::sync::Arc;
use awaken::stores::FileStore;

let store = Arc::new(FileStore::new("./data"));
```

目录会在首次写入时自动创建，布局如下：

```text
./data/
  threads/<thread_id>.json
  messages/<thread_id>.json
  message_records/<thread_id>/<seq>.json
  pending_messages/<thread_id>/<pending_id>.json
  runs/<run_id>.json
  thread_states/<thread_id>.json
  profiles/<scope>/<id>.json
  config/<namespace>/<id>.json
```

`messages/` 保存 materialized conversation view；`message_records/` 和
`pending_messages/` 保存序号、可见性和 staged input 记录。`thread_states/`、
`profiles/`、`config/` 会在同一个 file store 被接成 `ThreadStateStore`、
`ProfileStore` 或 `ConfigStore` 时使用。写入使用 staged file + rename，具体原子性取决于平台支持。

3. 接入 runtime：

```rust
use std::sync::Arc;
use awaken::contract::commit_coordinator::CommitCoordinator;
use awaken::AgentRuntimeBuilder;
use awaken::engine::GenaiExecutor;
use awaken::registry_spec::ModelSpec;
use awaken::stores::FileCommitCoordinator;

let coordinator = FileCommitCoordinator::wrap(store.clone())? as Arc<dyn CommitCoordinator>;
let runtime = AgentRuntimeBuilder::new()
    .with_commit_coordinator(coordinator)
    .with_agent_spec(spec)
    .with_provider("anthropic", Arc::new(GenaiExecutor::new()))
    .with_model(ModelSpec::new("claude-sonnet", "anthropic", "claude-sonnet-4-20250514"))
    .build()?;
```

`FileCommitCoordinator` 面向开发和本地部署。release build 中需要设置
`AWAKEN_ALLOW_DEV_FILE_COORDINATOR=true` 显式启用；它只提供 best-effort
跨 store 原子性，严格的多 store commit 原子性应使用 Postgres。

4. 生产环境建议使用绝对路径：

```rust
use std::path::PathBuf;

let data_dir = PathBuf::from("/var/lib/myapp/awaken");
let store = Arc::new(FileStore::new(data_dir));
```

## 验证

运行 agent 后检查目录，应该看到 `threads/`、`messages/`、`runs/` 下生成了 JSON
文件；server/control-plane wiring 还可能创建 `config/`、`profiles/`、
`thread_states/` 或 staged message records。

## 常见错误

| 错误 | 原因 | 修复 |
|---|---|---|
| `StorageError::Io` | 目录没有读写权限 | 确保进程对目标路径有权限 |
| `StorageError::Io` 且 ID 为空或非法 | thread/run ID 包含非法字符 | 使用 UUID 风格或简单字母数字 ID |
| 重启后找不到数据 | 相对路径在不同启动目录下解析不同 | 改用绝对路径 |

## 相关示例

`crates/awaken-stores/src/file.rs`

## 关键文件

- `crates/awaken-stores/Cargo.toml`
- `crates/awaken-stores/src/file.rs`
- `crates/awaken-stores/src/lib.rs`

## 相关

- [构建 Agent](/awaken/zh-cn/how-to/build-an-agent/)
- [使用 Postgres 存储](/awaken/zh-cn/how-to/use-postgres-store/)

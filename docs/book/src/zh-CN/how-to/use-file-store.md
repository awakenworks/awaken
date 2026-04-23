# 使用文件存储

当你希望在不引入外部数据库的情况下，用文件系统持久化 threads、runs 和 messages 时，使用本页。

## 前置条件

- `awaken-stores` 启用了 `file` feature

## 步骤

1. 添加依赖：

```toml
[dependencies]
awaken-stores = { version = "0.4.0-dev", features = ["file"] }
```

如果使用 `awaken` 门面 crate，也建议直接加 `awaken-stores` 来启用 `file` feature。

2. 创建 `FileStore`：

```rust,ignore
use std::sync::Arc;
use awaken::stores::FileStore;

let store = Arc::new(FileStore::new("./data"));
```

目录会在首次写入时自动创建，布局如下：

```text
./data/
  threads/<thread_id>.json
  messages/<thread_id>.json
  runs/<run_id>.json
```

3. 接入 runtime：

```rust,ignore
use std::sync::Arc;
use awaken::AgentRuntimeBuilder;
use awaken::engine::GenaiExecutor;
use awaken::registry::ModelBinding;

let runtime = AgentRuntimeBuilder::new()
    .with_thread_run_store(store)
    .with_agent_spec(spec)
    .with_provider("anthropic", Arc::new(GenaiExecutor::new()))
    .with_model_binding("claude-sonnet", ModelBinding {
        provider_id: "anthropic".into(),
        upstream_model: "claude-sonnet-4-20250514".into(),
    })
    .build()?;
```

4. 生产环境建议使用绝对路径：

```rust,ignore
use std::path::PathBuf;

let data_dir = PathBuf::from("/var/lib/myapp/awaken");
let store = Arc::new(FileStore::new(data_dir));
```

## 验证

运行 agent 后检查目录，应该看到 `threads/`、`messages/`、`runs/` 下生成了 JSON 文件。

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

- [构建 Agent](./build-an-agent.md)
- [使用 Postgres 存储](./use-postgres-store.md)

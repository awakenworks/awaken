---
title: "使用 Postgres 存储"
description: "当你需要可持久化、可多实例共享的存储后端时，使用 PostgreSQL。"
---

当你需要可持久化、可多实例共享的存储后端时，使用 PostgreSQL。

## 前置条件

- `awaken-stores` 启用了 `postgres` feature
- 有一个可连接的 PostgreSQL 实例
- `sqlx` 所需 tokio 运行时依赖已就绪

## 步骤

1. 添加依赖：

```toml
[dependencies]
awaken-stores = { version = "0.4.0", features = ["postgres"] }
```

2. 创建连接池：

```rust
use sqlx::PgPool;

let pool = PgPool::connect("postgres://user:pass@localhost:5432/mydb").await?;
```

3. 创建 `PostgresStore`：

```rust
use std::sync::Arc;
use awaken::stores::PostgresStore;

let store = Arc::new(PostgresStore::new(pool));
```

默认表名：

- `awaken_threads`
- `awaken_runs`
- `awaken_messages`

4. 使用自定义前缀：

```rust
let store = Arc::new(PostgresStore::with_prefix(pool, "myapp"));
```

5. 接入 runtime：

```rust
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

6. Schema 初始化：

表会在首次访问时自动通过 `ensure_schema()` 创建。初始接入无需手动 migration。

## 验证

执行 agent 后，在数据库中查询：

```sql
SELECT id, updated_at FROM awaken_threads;
SELECT id, updated_at FROM awaken_runs;
```

应该能看到对应记录。

## 常见错误

| 错误 | 原因 | 修复 |
|---|---|---|
| `sqlx::Error` connection refused | PostgreSQL 未启动或连接串错误 | 检查 `DATABASE_URL` 和数据库状态 |
| 首次写入报 `StorageError` | 数据库用户权限不足 | 授予建表和写入权限 |
| 表名冲突 | 其他应用共用了默认表名 | 用 `with_prefix()` 做命名空间隔离 |

## 相关示例

`crates/awaken-stores/src/postgres.rs`

## 关键文件

- `crates/awaken-stores/Cargo.toml`
- `crates/awaken-stores/src/postgres.rs`
- `crates/awaken-stores/src/lib.rs`

## 相关

- [构建 Agent](/build-an-agent/)
- [使用文件存储](/use-file-store/)

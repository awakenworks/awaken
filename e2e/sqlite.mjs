import { DatabaseSync } from 'node:sqlite';
import fs from 'node:fs';
import path from 'node:path';

function withDatabase(database, readOnly, operation) {
  // E2E database-access cause graph:
  // C1 the scenario needs a deterministic retained-row boundary; C2 the file
  // exists; C3 a killed writer left WAL recovery work; C4 the operation is a
  // query. Query connections open read/write only long enough for SQLite's own
  // recovery, then enable query_only before executing caller SQL. This avoids
  // both creating a misleading missing DB and rejecting valid post-crash reads.
  //
  // | Rule | C2 exists | C3 recovery | C4 query | Result |
  // |---|---|---|---|---|
  // | D1 | F | any | T | fail without creating a DB |
  // | D2 | T | T/F | T | recover, then enforce query_only |
  // | D3 | any | any | F | normal writable operation |
  if (readOnly && !fs.existsSync(database)) {
    throw new Error(`SQLite database does not exist: ${database}`);
  }
  const connection = new DatabaseSync(database);
  try {
    connection.exec('PRAGMA busy_timeout = 10000');
    if (readOnly) connection.exec('PRAGMA query_only = ON');
    return operation(connection);
  } finally {
    connection.close();
  }
}

export function sqliteExec(database, sql) {
  return withDatabase(database, false, (connection) => connection.exec(sql));
}

export function sqliteRows(database, sql, ...parameters) {
  return withDatabase(database, true, (connection) => connection.prepare(sql).all(...parameters));
}

export function sqliteRun(database, sql, ...parameters) {
  return withDatabase(database, false, (connection) => connection.prepare(sql).run(...parameters));
}

export function sqliteScalar(database, sql, ...parameters) {
  const row = sqliteRows(database, sql, ...parameters)[0];
  if (!row) return undefined;
  return Object.values(row)[0];
}

function filesUnder(root) {
  const pending = [root];
  const found = [];
  while (pending.length > 0) {
    const current = pending.pop();
    for (const entry of fs.readdirSync(current, { withFileTypes: true })) {
      const candidate = path.join(current, entry.name);
      if (entry.isDirectory()) pending.push(candidate);
      else if (entry.isFile()) found.push(candidate);
    }
  }
  return found.sort();
}

export function sqliteDatabaseForThread(root, threadId, table) {
  const allowedTables = new Set(['runtime_message', 'runtime_state_command', 'runtime_waiting']);
  if (!allowedTables.has(table)) throw new Error(`unsupported runtime thread table: ${table}`);
  const files = filesUnder(root);
  const matches = files
    .filter((candidate) => candidate.endsWith('.db'))
    .filter((candidate) => {
      const present = sqliteRows(
        candidate,
        "SELECT 1 AS present FROM sqlite_master WHERE type = 'table' AND name = ?",
        table,
      )[0];
      if (Number(present?.present) !== 1) return false;
      const row = table === 'runtime_waiting'
        ? sqliteRows(
            candidate,
            `SELECT COUNT(*) AS count
             FROM runtime_waiting AS waiting
             JOIN runtime_run_record AS run ON run.run_id = waiting.run_id
             WHERE run.thread_id = ?`,
            threadId,
          )[0]
        : sqliteRows(
            candidate,
            `SELECT COUNT(*) AS count FROM ${table} WHERE thread_id = ?`,
            threadId,
          )[0];
      return Number(row?.count) > 0;
    });
  if (matches.length !== 1) {
    throw new Error(
      `thread must have one ${table} SQLite boundary; thread=${threadId} matches=${JSON.stringify(matches)} files=${JSON.stringify(files.map((candidate) => path.relative(root, candidate)))}`,
    );
  }
  return matches[0];
}

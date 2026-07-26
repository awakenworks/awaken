import { DatabaseSync } from 'node:sqlite';

function withDatabase(database, readOnly, operation) {
  // E2E database-access cause graph:
  // C1 the scenario needs a deterministic retained-row boundary;
  // C2 Node provides in-process SQLite; C3 an external sqlite3 CLI is installed.
  // C1+C2 is sufficient, so C3 must never be an environment prerequisite.
  //
  // | Rule | C1 DB boundary | C2 node:sqlite | C3 CLI | Result |
  // |---|---|---|---|---|
  // | D1 | T | T | F | execute/inspect in process |
  // | D2 | T | T | T | execute/inspect in process |
  const connection = new DatabaseSync(database, { readOnly });
  try {
    connection.exec('PRAGMA busy_timeout = 10000');
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

export interface SqliteWorkerOpenOptions {
  filename: string;
  readOnly: boolean;
  requireWal: boolean;
  /** Prepared-statement LRU cache capacity. Default 200. */
  prepareCacheSize?: number;
  /** Bounded `PRAGMA cache_size` in KiB. Default 4000 for readers, 8000 for the writer. */
  cacheSizeKb?: number;
}

export interface SqliteWorkerExecuteRequest {
  sql: string;
  /** Positional params, or `[{ name: value }]` for named parameters. */
  params?: unknown[];
  cached?: boolean;
  /** Return rows as arrays instead of objects. */
  values?: boolean;
  /** Return `{ changes, lastInsertRowid }` for statements without columns. */
  raw?: boolean;
  /** Read SQLite integers as `bigint`. */
  safeIntegers?: boolean;
  /** Savepoint SQL executed before `sql` on the worker's connection. */
  savepoints?: string[];
}

export interface SqliteRunResult {
  changes: number | bigint;
  lastInsertRowid: number | bigint;
}

export interface NativeSqliteError extends Error {
  operation: "open" | "prepare" | "execute";
  errcode?: number;
  errstr?: string;
  code: "ERR_SQLITE_ERROR" | "ERR_SQLITE_NOT_WAL";
}

export declare class NativeSqliteWorker {
  private constructor();
  static open(options: SqliteWorkerOpenOptions): Promise<NativeSqliteWorker>;
  execute(request: SqliteWorkerExecuteRequest): Promise<unknown>;
  close(): Promise<void>;
}

export interface CheckpointWorkerOptions {
  filename: string;
  /** Run PASSIVE checkpoints when `log - checkpointed >= frames`. Default 1000. */
  frames?: number;
  /** Idle re-check delay in ms. Default 100. */
  idleMs?: number;
}

export declare class NativeCheckpointWorker {
  private constructor();
  static start(options: CheckpointWorkerOptions): NativeCheckpointWorker;
  notify(): void;
  stop(): Promise<void>;
  lastError(): string | null;
}

export declare function sqliteVersion(): string;

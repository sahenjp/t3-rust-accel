import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { DatabaseSync } from "node:sqlite";
import { after, test } from "node:test";

import { sqliteModuleUrl } from "../../scripts/t3-accel.mjs";

const { NativeCheckpointWorker, NativeSqliteWorker, sqliteVersion } = await import(sqliteModuleUrl);

const dir = mkdtempSync(join(tmpdir(), "t3 native sqlite 前-"));
const dbFile = join(dir, "state.sqlite");
{
  const seed = new DatabaseSync(dbFile);
  seed.exec("PRAGMA journal_mode = WAL; CREATE TABLE t(id INTEGER PRIMARY KEY, name TEXT, data BLOB);");
  seed.exec("INSERT INTO t(name) VALUES ('seed')");
  seed.close();
}

const exec = (w, sql, opts = {}) =>
  w.execute({
    sql,
    params: [],
    cached: true,
    values: false,
    raw: false,
    safeIntegers: false,
    ...opts,
  });

test("sqliteVersion() reports bundled SQLite >= 3.51", () => {
  const v = sqliteVersion();
  console.log("bundled SQLite:", v);
  const [maj, min] = v.split(".").map(Number);
  assert(maj > 3 || (maj === 3 && min >= 51), `expected >= 3.51, got ${v}`);
});

test("open rejects a non-WAL database with ERR_SQLITE_NOT_WAL", async () => {
  const badFile = join(dir, "bad.sqlite");
  const db = new DatabaseSync(badFile);
  db.exec("CREATE TABLE x(a)");
  db.close();
  await assert.rejects(
    NativeSqliteWorker.open({ filename: badFile, readOnly: true, requireWal: true }),
    (e) => e.code === "ERR_SQLITE_NOT_WAL" && e.operation === "open",
  );
});

let writer, readers, ckpt;
after(async () => {
  await ckpt?.stop();
  await Promise.all(readers?.map((r) => r.close()) ?? []);
  await writer?.close();
  rmSync(dir, { recursive: true, force: true });
});
test("opens 4 WAL readers + 1 writer + 1 checkpoint worker", async () => {
  readers = await Promise.all(
    Array.from({ length: 4 }, () =>
      NativeSqliteWorker.open({ filename: dbFile, readOnly: true, requireWal: true }),
    ),
  );
  writer = await NativeSqliteWorker.open({ filename: dbFile, readOnly: false, requireWal: false });
  ckpt = NativeCheckpointWorker.start({ filename: dbFile, frames: 5, idleMs: 20 });
  const rows = await exec(readers[0], "SELECT name FROM t WHERE id = 1");
  assert.deepEqual(rows, [{ name: "seed" }]);
});

test("concurrent reads across all 4 readers", async () => {
  const results = await Promise.all(
    readers.map((r, i) => exec(r, "SELECT ? AS n", { params: [i] })),
  );
  assert.deepEqual(results, readers.map((_, i) => [{ n: i }]));
});

test("insert via writer: raw mode returns changes + lastInsertRowid", async () => {
  const res = await exec(writer, "INSERT INTO t(name) VALUES (?)", {
    params: ["alice"],
    raw: true,
  });
  assert.equal(res.changes, 1);
  assert.equal(res.lastInsertRowid, 2);
  assert.equal(typeof res.lastInsertRowid, "number");
});

test("safeIntegers returns bigint", async () => {
  const rows = await exec(writer, "SELECT id, name FROM t WHERE id = 2", { safeIntegers: true });
  assert.equal(rows[0].id, 2n);
  assert.equal(typeof rows[0].id, "bigint");
});

test("values mode returns arrays", async () => {
  const rows = await exec(readers[1], "SELECT id, name FROM t ORDER BY id", { values: true });
  assert.deepEqual(rows, [[1, "seed"], [2, "alice"]]);
});

test("positional + named params", async () => {
  const a = await exec(writer, "SELECT ?1 + ?2 AS sum", { params: [40, 2] });
  assert.deepEqual(a, [{ sum: 42 }]);
  const b = await exec(writer, "SELECT $x + :y + @z AS sum", {
    params: [{ $x: 10, ":y": 20, "@z": 12 }],
  });
  assert.deepEqual(b, [{ sum: 42 }]);
  const c = await exec(writer, "SELECT $x AS v", { params: [{ x: 7 }] });
  assert.deepEqual(c, [{ v: 7 }]);
});

test("Uint8Array BLOB roundtrip + param types", async () => {
  const blob = new Uint8Array([0, 1, 2, 255, 128]);
  await exec(writer, "INSERT INTO t(name, data) VALUES (?, ?)", {
    params: ["blob", blob],
  });
  const rows = await exec(writer, "SELECT data FROM t WHERE name = 'blob'");
  assert(rows[0].data instanceof Uint8Array);
  assert.deepEqual([...rows[0].data], [0, 1, 2, 255, 128]);
  const types = await exec(writer, "SELECT ? AS n, ? AS r, ? AS s, ? AS b, ? AS u", {
    params: [null, 1.5, "txt", true, undefined],
  });
  assert.deepEqual(types, [{ n: null, r: 1.5, s: "txt", b: 1, u: null }]);
});

test("empty BLOBs, offset views and float16 buffers", async () => {
  const bytes = Uint8Array.of(99, 1, 2, 3, 88);
  const float16 = typeof Float16Array === "function" ? new Float16Array([1]) : new Uint16Array([1]);
  const buffers = [
    new ArrayBuffer(0),
    new Uint8Array(0),
    new DataView(new ArrayBuffer(0)),
    new DataView(bytes.buffer, 1, 3),
    new Uint16Array(bytes.buffer, 0, 2),
    float16,
    Buffer.from([0, 1, 2]),
  ];
  for (const buffer of buffers) {
    const expected = ArrayBuffer.isView(buffer)
      ? new Uint8Array(buffer.buffer, buffer.byteOffset, buffer.byteLength)
      : new Uint8Array(buffer);
    const [[data]] = await exec(writer, "SELECT ?", { params: [buffer], values: true });
    assert.deepEqual(data, expected);
  }
});

test("parameter errors preserve JS exceptions and the worker stays usable", async () => {
  const original = new Error("invalid binding");
  const params = [{ get value() { throw original; } }];
  assert.throws(() => exec(writer, "SELECT :value", { params }), (error) => error === original);
  assert.throws(
    () => exec(writer, "SELECT ?", { params: new Array(32_767) }),
    { operation: "execute", code: "ERR_SQLITE_ERROR" },
  );
  assert.throws(
    () => exec(writer, "SELECT ?", { params: [new Uint8Array(new SharedArrayBuffer(1))] }),
    { operation: "execute", code: "ERR_SQLITE_ERROR" },
  );
  assert.deepEqual(await exec(writer, "SELECT 1 AS n"), [{ n: 1 }]);
});

test("row properties and named bindings have ordinary own-property semantics", async () => {
  const [row] = await exec(writer, 'SELECT 1 AS "__proto__", 2 AS duplicate, 3 AS duplicate');
  assert.equal(Object.getPrototypeOf(row), Object.prototype);
  assert.deepEqual(row, { ["__proto__"]: 1, duplicate: 3 });

  const params = Object.create({ n: 10 });
  assert.deepEqual(await exec(writer, "SELECT :n AS n", { params: [params] }), [{ n: null }]);
  const boundary = await exec(writer, "SELECT typeof(?) AS type, ? AS val", {
    params: [2 ** 63, 2n ** 63n - 1n],
    safeIntegers: true,
  });
  assert.deepEqual(boundary, [{ type: "real", val: 2n ** 63n - 1n }]);
});

test("transaction + savepoints + rollback", async () => {
  await exec(writer, "BEGIN IMMEDIATE", { cached: false });
  await exec(writer, "INSERT INTO t(name) VALUES ('tx1')");
  await exec(writer, "INSERT INTO t(name) VALUES ('tx2')", {
    savepoints: ["SAVEPOINT effect_sql_0"],
  });
  await exec(writer, "ROLLBACK TO SAVEPOINT effect_sql_0", { cached: false });
  await exec(writer, "COMMIT", { cached: false });
  const rows = await exec(writer, "SELECT name FROM t WHERE name LIKE 'tx%'");
  assert.deepEqual(rows, [{ name: "tx1" }]);
});

test("rollback discards transaction", async () => {
  await exec(writer, "BEGIN", { cached: false });
  await exec(writer, "INSERT INTO t(name) VALUES ('gone')");
  await exec(writer, "ROLLBACK", { cached: false });
  const rows = await exec(writer, "SELECT count(*) AS c FROM t WHERE name = 'gone'");
  assert.equal(rows[0].c, 0);
});

test("UNIQUE constraint carries extended errcode", async () => {
  await exec(writer, "CREATE TABLE uniq(name TEXT UNIQUE)", { cached: false });
  await exec(writer, "INSERT INTO uniq VALUES ('dup')");
  const err = await exec(writer, "INSERT INTO uniq VALUES ('dup')").then(
    () => null,
    (e) => e,
  );
  assert(err, "expected rejection");
  assert.equal(err.operation, "execute");
  assert.equal(err.code, "ERR_SQLITE_ERROR");
  assert.equal(err.errcode & 0xff, 19, `errcode ${err.errcode}`);
  assert.equal(err.errcode, 2067);
  assert.equal(typeof err.errstr, "string");
});

test("prepare error has operation=prepare", async () => {
  const err = await exec(writer, "SELECT bogus FROM").then(() => null, (e) => e);
  assert.equal(err.operation, "prepare");
  assert.equal(err.code, "ERR_SQLITE_ERROR");
});

test(">64KB binding works uncached", async () => {
  const big = "x".repeat(70 * 1024);
  const res = await exec(writer, "INSERT INTO t(name) VALUES (?)", {
    params: [big],
    cached: false,
    raw: true,
  });
  assert.equal(res.changes, 1);
  const rows = await exec(writer, "SELECT length(name) AS len FROM t WHERE length(name) > 70000");
  assert.equal(rows[0].len, big.length);
});

test("checkpoint worker flushes a write below the notification threshold after idle", async () => {
  const observe = new DatabaseSync(dbFile);
  try {
    observe.exec("PRAGMA wal_checkpoint(TRUNCATE)");
    await exec(writer, "INSERT INTO t(name) VALUES ('checkpoint')");
    const current = () => observe.prepare("PRAGMA wal_checkpoint(NOOP)").get();
    assert(current().log > current().checkpointed);
    ckpt.notify();
    let state;
    for (let attempt = 0; attempt < 50; attempt++) {
      state = current();
      if (state.log === state.checkpointed) break;
      await new Promise((resolve) => setTimeout(resolve, 20));
    }
    assert.equal(ckpt.lastError(), null);
    assert(state.log > 0);
    assert.equal(state.log, state.checkpointed);
  } finally {
    observe.close();
  }
});

test("execute after close rejects; close() is graceful", async () => {
  const w = await NativeSqliteWorker.open({ filename: dbFile, readOnly: true, requireWal: true });
  const closing = w.close();
  assert.equal(w.close(), closing);
  await closing;
  await assert.rejects(exec(w, "SELECT 1"), /closed/);
  await w.close(); // idempotent
});

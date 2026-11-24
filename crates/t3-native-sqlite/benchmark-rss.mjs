// RSS benchmark: 6 V8 worker isolates (node:sqlite) vs Rust OS threads (t3-native-sqlite)
import { readFileSync } from "node:fs";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { DatabaseSync } from "node:sqlite";
import { Worker } from "node:worker_threads";

import { NativeCheckpointWorker, NativeSqliteWorker } from "./index.mjs";

const dir = mkdtempSync(join(tmpdir(), "t3-sqlite-bench-"));
const dbFile = join(dir, "state.sqlite");
{
  const db = new DatabaseSync(dbFile);
  db.exec("PRAGMA journal_mode = WAL; CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT);");
  db.close();
}

const vmRss = () =>
  Number(readFileSync("/proc/self/status", "utf8").match(/VmRSS:\s*(\d+) kB/)[1]) * 1024;
const fmt = (b) => `${(b / 1024 / 1024).toFixed(1)} MB`;
const settle = () => new Promise((r) => setTimeout(r, 150));
if (global.gc) global.gc();

const workerSource = `
const { parentPort, workerData } = require("node:worker_threads");
const { DatabaseSync } = require("node:sqlite");
if (workerData.checkpoint) {
  const db = new DatabaseSync(workerData.filename);
  db.exec("PRAGMA busy_timeout = 5000; PRAGMA checkpoint_fullfsync = ON;");
  const frames = db.prepare("PRAGMA wal_checkpoint(NOOP)");
  const passive = db.prepare("PRAGMA wal_checkpoint(PASSIVE)");
  let idle;
  const check = (force) => {
    const before = frames.get();
    let state = before;
    if (state.log - state.checkpointed >= workerData.frames || (force && state.log > state.checkpointed)) {
      passive.get(); state = frames.get();
    }
    const progressed = !force || state.checkpointed > before.checkpointed;
    if (state.log > state.checkpointed && progressed) idle = setTimeout(() => run(true), workerData.idleMs);
  };
  const run = (f) => { clearTimeout(idle); try { check(f); } catch {} };
  parentPort.on("message", () => run(false));
} else {
  const db = new DatabaseSync(workerData.filename, { readOnly: workerData.readOnly });
  db.exec("PRAGMA busy_timeout = 5000;");
  const stmts = new Map();
  parentPort.on("message", (req) => {
    if (req.type === "close") { db.close(); parentPort.close(); return; }
    let s = stmts.get(req.sql);
    if (!s) { s = db.prepare(req.sql); stmts.set(req.sql, s); }
    const rows = s.all(...req.params);
    parentPort.postMessage({ id: req.id, rows });
  });
  parentPort.postMessage({ type: "ready" });
}`;

const ready = (w) => new Promise((r) => w.once("message", r));
const ask = (w, sql, params = []) =>
  new Promise((res) => {
    const id = Math.random();
    const on = (m) => { if (m.id === id) { w.off("message", on); res(m.rows); } };
    w.on("message", on);
    w.postMessage({ id, sql, params });
  });

// ---- baseline: 6 node:worker_threads V8 isolates ----
const base0 = vmRss();
const baseWorkers = [];
for (let i = 0; i < 4; i++) {
  const w = new Worker(workerSource, { eval: true, workerData: { filename: dbFile, readOnly: true } });
  baseWorkers.push(w);
}
const baseWriter = new Worker(workerSource, { eval: true, workerData: { filename: dbFile, readOnly: false } });
const baseCkpt = new Worker(workerSource, { eval: true, workerData: { filename: dbFile, checkpoint: true, frames: 1000, idleMs: 100 } });
await Promise.all([...baseWorkers, baseWriter].map(ready));
await settle();
const baseAfterSpawn = vmRss();
// exercise each worker so lazy allocs happen
await Promise.all(baseWorkers.map((w, i) => ask(w, "SELECT ? AS n", [i])));
await ask(baseWriter, "INSERT INTO t(v) VALUES ('x')");
baseCkpt.postMessage(0);
await settle();
const baseAfterWork = vmRss();
const memBase = process.memoryUsage();

// teardown baseline
for (const w of [...baseWorkers, baseWriter]) w.postMessage({ type: "close" });
await new Promise((r) => setTimeout(r, 200));
for (const w of [...baseWorkers, baseWriter, baseCkpt]) await w.terminate();
await settle();
if (global.gc) global.gc();
await settle();
const mid = vmRss();

// ---- optimized: Rust OS threads ----
const opt0 = vmRss();
const readers = await Promise.all(
  Array.from({ length: 4 }, () =>
    NativeSqliteWorker.open({ filename: dbFile, readOnly: true, requireWal: true })),
);
const writer = await NativeSqliteWorker.open({ filename: dbFile, readOnly: false, requireWal: false });
const ckpt = NativeCheckpointWorker.start({ filename: dbFile, frames: 1000, idleMs: 100 });
await settle();
const optAfterSpawn = vmRss();
await Promise.all(readers.map((r, i) => r.execute({ sql: "SELECT ? AS n", params: [i] })));
await writer.execute({ sql: "INSERT INTO t(v) VALUES (?)", params: ["x"], raw: true });
ckpt.notify();
await settle();
const optAfterWork = vmRss();
const memOpt = process.memoryUsage();

await ckpt.stop();
await Promise.all(readers.map((r) => r.close()));
await writer.close();

console.log("=== RSS benchmark: 4 readers + 1 writer + 1 checkpoint ===");
console.log(`process start           ${fmt(base0)}`);
console.log(`baseline  spawned       ${fmt(baseAfterSpawn)}  (+${fmt(baseAfterSpawn - base0)})`);
console.log(`baseline  after work    ${fmt(baseAfterWork)}  (+${fmt(baseAfterWork - base0)})`);
console.log(`baseline  heapUsed      ${fmt(memBase.heapUsed)}  rss ${fmt(memBase.rss)}`);
console.log(`after teardown          ${fmt(mid)}`);
console.log(`native    spawned       ${fmt(optAfterSpawn)}  (+${fmt(optAfterSpawn - opt0)})`);
console.log(`native    after work    ${fmt(optAfterWork)}  (+${fmt(optAfterWork - opt0)})`);
console.log(`native    heapUsed      ${fmt(memOpt.heapUsed)}  rss ${fmt(memOpt.rss)}`);
console.log(`delta (spawn)           ${fmt(baseAfterSpawn - base0 - (optAfterSpawn - opt0))} saved`);
console.log(`delta (after work)      ${fmt(baseAfterWork - optAfterWork)} saved`);
rmSync(dir, { recursive: true, force: true });

import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { spawnSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

const launcher = fileURLToPath(new URL("./t3-accel.mjs", import.meta.url));

test("launcher passes arguments without a shell and defaults to loopback and no telemetry", (t) => {
  const dir = mkdtempSync(join(tmpdir(), "t3 launcher-"));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  const server = join(dir, "fake server.mjs");
  writeFileSync(server, `
    import { statSync } from "node:fs";
    const value = (name) => process.env[name];
    console.log(JSON.stringify({
      args: process.argv.slice(2),
      host: value("T3CODE_HOST"),
      lan: value("T3CODE_LAN"),
      analytics: value("T3CODE_TELEMETRY_ENABLED"),
      tracingDisabled: value("T3CODE_OTEL_SDK_DISABLED"),
      nativeFiles: ["T3_NATIVE_SQLITE_PATH", "T3_RUST_BRIDGE_BIN", "T3_MAKI_TOOLS_BIN"]
        .every((name) => statSync(value(name)).isFile()),
    }));
    process.exitCode = 23;
  `);
  const result = spawnSync(process.execPath, [launcher, "serve", server, dir], {
    cwd: dir,
    encoding: "utf8",
    timeout: 10000,
    env: {
      ...process.env,
      T3CODE_HOST: "0.0.0.0",
      T3CODE_LAN: "true",
      T3CODE_TELEMETRY_ENABLED: "true",
      T3CODE_OTEL_SDK_DISABLED: "false",
    },
  });
  assert.equal(result.status, 23, result.error?.message ?? result.stderr);
  assert.deepEqual(JSON.parse(result.stdout), {
    args: ["serve", "--no-browser", dir],
    host: "127.0.0.1",
    lan: "false",
    analytics: "false",
    tracingDisabled: "true",
    nativeFiles: true,
  });
});

test("a checksummed package runs the CLI and opens SQLite after extraction", (t) => {
  const dir = mkdtempSync(join(tmpdir(), "t3 release "));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  const archiveName = `t3-rust-accel-${process.platform}-${process.arch}.tar.gz`;
  const archive = join(dir, archiveName);
  const run = (command, args) => {
    const result = spawnSync(command, args, { cwd: dir, encoding: "utf8", timeout: 30_000 });
    assert.equal(result.status, 0, result.error?.message ?? result.stderr);
    return result.stdout;
  };
  run(process.execPath, [launcher, "package", dir]);
  const digest = createHash("sha256").update(readFileSync(archive)).digest("hex");
  assert.equal(readFileSync(join(dir, "SHA256SUMS"), "utf8"), `${digest}  ${archiveName}\n`);
  run("tar", ["-xzpf", archiveName]);
  const unpacked = join(dir, archiveName.replace(/\.tar\.gz$/, ""));
  if (process.platform !== "win32") {
    const releaseDir = process.platform === "darwin"
      ? (process.arch === "arm64" ? "aarch64-apple-darwin/release" : "x86_64-apple-darwin/release")
      : "release";
    for (const [file, mode] of [
      ["t3-rust-bridge", 0o755],
      ["t3-maki-tools", 0o755],
      ["t3-native-sqlite/t3_native_sqlite.node", 0o644],
    ]) {
      const extracted = join(unpacked, "target", releaseDir, file);
      assert.equal(statSync(extracted).mode & 0o777, mode, extracted);
    }
  }
  const server = join(dir, "test server.mjs");
  writeFileSync(server, `
    import assert from "node:assert/strict";
    import { execFileSync } from "node:child_process";
    import { pathToFileURL } from "node:url";
    const { NativeSqliteWorker } = await import(pathToFileURL(process.env.T3_NATIVE_SQLITE_PATH));
    const db = await NativeSqliteWorker.open({ filename: ":memory:" });
    try {
      assert.deepEqual(await db.execute({ sql: "SELECT 42" }), [{ "42": 42 }]);
    } finally {
      await db.close();
    }
    assert.match(execFileSync(process.env.T3_RUST_BRIDGE_BIN, ["--version"], { encoding: "utf8" }),
      /^t3-rust-bridge /u);
    assert.match(execFileSync(process.env.T3_MAKI_TOOLS_BIN, ["--version"], { encoding: "utf8" }),
      /^t3-maki-tools /u);
  `);
  run(process.execPath, [join(unpacked, "scripts", "t3-accel.mjs"), "serve", server]);
});

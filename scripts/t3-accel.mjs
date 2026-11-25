import { spawn, spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
  chmodSync, copyFileSync, mkdirSync, mkdtempSync, readFileSync, rmSync, statSync, writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, relative, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const root = fileURLToPath(new URL("../", import.meta.url));
const target = join(root, "target");
const macTargets = { arm64: "aarch64-apple-darwin", x64: "x86_64-apple-darwin" };
const macTarget = process.platform === "darwin" ? macTargets[process.arch] : undefined;
const release = join(target, macTarget ?? "", "release");
const win = process.platform === "win32";
const exe = win ? ".exe" : "";
const sqlite = join(release, "t3-native-sqlite");
const sqliteModule = join(sqlite, "index.mjs");
export const sqliteModuleUrl = pathToFileURL(sqliteModule).href;
const addon = join(sqlite, "t3_native_sqlite.node");
const bridge = join(release, `t3-rust-bridge${exe}`);
const maki = join(release, `t3-maki-tools${exe}`);

function build() {
  for (const name of ["t3-rust-bridge", "t3-maki-tools", "t3-native-sqlite"]) {
    const result = spawnSync("cargo", [
      "build", "--locked", "--release", "--manifest-path",
      join(root, "crates", name, "Cargo.toml"), "--target-dir", target,
      ...(macTarget === undefined ? [] : ["--target", macTarget]),
    ], { cwd: root, stdio: "inherit" });
    if (result.error) throw result.error;
    if (result.status !== 0) return result.status ?? 1;
  }

  const library = win ? "t3_native_sqlite.dll" : process.platform === "darwin"
    ? "libt3_native_sqlite.dylib"
    : "libt3_native_sqlite.so";
  mkdirSync(sqlite, { recursive: true });
  copyFileSync(join(release, library), addon);
  for (const name of ["index.mjs", "index.d.ts"]) {
    copyFileSync(join(root, "crates", "t3-native-sqlite", name), join(sqlite, name));
  }
  return 0;
}

function packageBuild(outputPath) {
  const name = `t3-rust-accel-${process.platform}-${process.arch}`;
  const archiveName = `${name}.tar.gz`;
  const output = resolve(outputPath);
  const stage = mkdtempSync(join(tmpdir(), "t3-accel-package-"));
  try {
    const packageRoot = join(stage, name);
    for (const file of [
      fileURLToPath(import.meta.url), join(root, "README.md"), bridge, maki, addon,
      sqliteModule, join(sqlite, "index.d.ts"),
      join(root, "patches/0000-integrate-rust-accelerator.patch"),
      join(root, "patches/0001-telemetry-opt-in-by-default.patch"),
      join(root, "patches/0002-scope-mcp-registration-buffer.patch"),
      join(root, "patches/0003-use-installed-antigravity-cli.patch"),
      join(root, "patches/0004-t3-connect-e2ee.patch"),
      join(root, "patches/T3CODE-LICENSE"),
    ]) {
      const copy = join(packageRoot, relative(root, file));
      mkdirSync(dirname(copy), { recursive: true });
      copyFileSync(file, copy);
      if (!win) chmodSync(copy, file === bridge || file === maki ? 0o755 : 0o644);
    }

    const archive = join(stage, archiveName);
    const result = spawnSync("tar", ["-czf", archiveName, name], {
      cwd: stage,
      stdio: "inherit",
    });
    if (result.error) throw result.error;
    if (result.status !== 0) return result.status ?? 1;

    const checksum = createHash("sha256").update(readFileSync(archive)).digest("hex");
    mkdirSync(output, { recursive: true });
    copyFileSync(archive, join(output, archiveName));
    writeFileSync(join(output, "SHA256SUMS"), `${checksum}  ${archiveName}\n`);
    return 0;
  } finally {
    rmSync(stage, { recursive: true, force: true });
  }
}

function serve(serverPath, args) {
  const server = resolve(serverPath);
  for (const file of [server, bridge, maki, sqliteModule, addon]) {
    if (!statSync(file, { throwIfNoEntry: false })?.isFile()) {
      throw new Error(`Missing file ${file}. Build T3 Code and run t3-accel.mjs build.`);
    }
  }
  const child = spawn(process.execPath, [server, "serve", "--no-browser", ...args], {
    stdio: "inherit",
    env: {
      ...process.env,
      T3_NATIVE_SQLITE_PATH: sqliteModule,
      T3_RUST_BRIDGE_BIN: bridge,
      T3_MAKI_TOOLS_BIN: maki,
      T3CODE_HOST: "127.0.0.1",
      T3CODE_LAN: "false",
      T3CODE_TELEMETRY_ENABLED: "false",
      T3CODE_OTEL_SDK_DISABLED: "true",
    },
  });
  // Console Ctrl+C is delivered to both processes on Windows.
  const signals = win ? [] : ["SIGINT", "SIGTERM"];
  const handlers = signals.map((signal) => {
    const handler = () => child.kill(signal);
    process.on(signal, handler);
    return [signal, handler];
  });
  child.once("error", (error) => {
    console.error(error.message);
    process.exitCode = 1;
  });
  child.once("close", (code, signal) => {
    for (const [name, handler] of handlers) process.off(name, handler);
    if (signal && !win) process.kill(process.pid, signal);
    else process.exitCode = code ?? 1;
  });
}

if (import.meta.main) {
  try {
    const [command, ...args] = process.argv.slice(2);
    if (command === "build" && args.length === 0) {
      process.exitCode = build();
    } else if (command === "package" && args.length === 1) {
      process.exitCode = packageBuild(args[0]);
    } else if (command === "serve" && args.length > 0) {
      serve(args[0], args.slice(1));
    } else {
      throw new Error(
        "Usage: node scripts/t3-accel.mjs build | package <output-dir> | serve <server/bin.mjs> [serve options]",
      );
    }
  } catch (error) {
    console.error(error.message);
    process.exitCode = 1;
  }
}

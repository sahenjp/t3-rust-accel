#!/usr/bin/env bash
set -euo pipefail

dist="$(cd -- "${1:?usage: bash scripts/test-linux-package.sh <archive-directory>}" && pwd)"
images=(
  "node:26.13.1-bookworm-slim@sha256:0042e17d232a87a2cd485be1da7900fca4d10a4ad11b29e9285da1d2e54798c9"
  "node:26.13.1-trixie-slim@sha256:abe7e5a82b61a8ef67b315b85497c9e65173387ccf2f4a029248184b8e020be5"
)

for image in "${images[@]}"; do
  docker run --rm --network none --read-only --cap-drop ALL --security-opt no-new-privileges \
    --user "$(id -u):$(id -g)" --tmpfs /tmp:rw,exec,nosuid,nodev,mode=1777,size=128m \
    --mount "type=bind,src=$dist,target=/artifact,readonly" "$image" sh -eu -c '
      . /etc/os-release
      printf "%s\n" "$PRETTY_NAME"
      cd /artifact
      sha256sum --strict -c SHA256SUMS
      case "$(node -p process.arch)" in
        x64|arm64) name="t3-rust-accel-linux-$(node -p process.arch)" ;;
        *) exit 1 ;;
      esac
      tar -xzf "$name.tar.gz" -C /tmp
      cd "/tmp/$name"
      cat > /tmp/server.mjs <<\SCRIPT
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { pathToFileURL } from "node:url";
const { NativeSqliteWorker } = await import(pathToFileURL(process.env.T3_NATIVE_SQLITE_PATH));
const worker = await NativeSqliteWorker.open({ filename: ":memory:" });
try {
  assert.deepEqual(await worker.execute({ sql: "SELECT 42" }), [{ 42: 42 }]);
} finally {
  await worker.close();
}
for (const key of ["T3_RUST_BRIDGE_BIN", "T3_MAKI_TOOLS_BIN"]) {
  const version = execFileSync(process.env[key], ["--version"], { encoding: "utf8" });
  assert.match(version, /^t3-/u);
}
assert.equal(process.env.T3CODE_HOST, "127.0.0.1");
console.log("Packaged launcher, SQLite and Rust CLIs passed");
SCRIPT
      node scripts/t3-accel.mjs serve /tmp/server.mjs
    '
done

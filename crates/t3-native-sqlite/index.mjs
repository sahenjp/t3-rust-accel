import { createRequire } from "node:module";

const require = createRequire(import.meta.url);
const native = require("./t3_native_sqlite.node");

const { NativeSqliteWorker, NativeCheckpointWorker, sqliteVersion } = native;

// `open` lives on the class but must be async (a factory can only return Self
// synchronously), so the addon exports it as `openSqliteWorker`.
NativeSqliteWorker.open = (options) => native.openSqliteWorker(options);

// Concurrent shutdown callers must all wait until the native connection is closed.
function shareShutdown(shutdown) {
  const pending = new WeakMap();
  return function () {
    let result = pending.get(this);
    if (result === undefined) {
      result = shutdown.call(this);
      pending.set(this, result);
    }
    return result;
  };
}
NativeSqliteWorker.prototype.close = shareShutdown(NativeSqliteWorker.prototype.close);
NativeCheckpointWorker.prototype.stop = shareShutdown(NativeCheckpointWorker.prototype.stop);

export { NativeSqliteWorker, NativeCheckpointWorker, sqliteVersion };

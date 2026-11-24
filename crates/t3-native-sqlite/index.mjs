import { createRequire } from "node:module";

const require = createRequire(import.meta.url);
const native = require("./t3_native_sqlite.node");

const { NativeSqliteWorker, NativeCheckpointWorker, sqliteVersion } = native;

// `open` lives on the class but must be async (a factory can only return Self
// synchronously), so the addon exports it as `openSqliteWorker`.
NativeSqliteWorker.open = (options) => native.openSqliteWorker(options);

export { NativeSqliteWorker, NativeCheckpointWorker, sqliteVersion };

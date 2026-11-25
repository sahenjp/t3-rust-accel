# t3-rust-accel

[T3 Code](https://github.com/pingdotgg/t3code) サーバを個人環境で動かすための、Rust 加速コンポーネントとテレメトリ無効化のセットです。Linux、macOS、Windows で、ビルド済みの T3 Code サーバに組み込めます。

## 普通の T3 Code との違い

普通の T3 Code は、サーバの動作は上流のままで、使用状況を T3 運営のサーバに自動送信します。このキットは次の 2 点が違います。

- テレメトリを既定で送りません。上流では起動状況や使用統計 (イベント名、匿名 ID、OS やモデル名などのメタデータ) が `ingest.t3.codes` に送られます。使用統計にはトークン数が含まれ、キャッシュヒットの量 (`cachedInputTokens` / `cacheCreationTokens`) もこの中です。本文、プロンプト、コード、プロバイダの鍵は上流でも送られません。このキットでは製品テレメトリと relay tracing を環境変数とソースパッチで無効にします。モデルとの通信には、各プロバイダの CLI や API を使います。
- サーバの土台を Rust で置き換え、少ないメモリで動くようにします。上流は Node の SQLite ドライバと既定のメモリ設定で動きます。このキットはネイティブ SQLite アドオンと Rust bridge を使います。Linux の systemd 設定では jemalloc と V8 も調整できます。あわせて `code_index` / `code_slice` / `code_grep` のコード読み替えツール (maki tools) を使えます。

そのほかの部分 (エージェントの動き、クライアント、セッションの仕組み) は上流と同じです。セッションや鍵は `T3CODE_HOME` のままなので、このキットを外せば元の T3 Code に戻せます。

## このリポジトリの中身

| パス | 役割 |
| --- | --- |
| `crates/t3-rust-bridge/` | サーバから使う Rust bridge バイナリ (`T3_RUST_BRIDGE_BIN`) |
| `crates/t3-native-sqlite/` | `index.mjs` 経由で使うネイティブ sqlite バインディング (`T3_NATIVE_SQLITE_PATH`) |
| `crates/t3-maki-tools/` | `code_index` / `code_slice` / `code_grep` の CLI (`T3_MAKI_TOOLS_BIN`) |
| `systemd/` | user unit の drop-in テンプレート (phase 2 サーバとメモリ調整)。`%h` は systemd がユーザーのホームディレクトリに展開 |
| `patches/` | T3 Code サーバのテレメトリ、MCP 登録、Antigravity 連携のソースパッチ |
| `scripts/` | 共通のビルド・起動コマンドと Linux の systemd 設定スクリプト |

## ビルドと起動

Node.js 26、Rust stable、ビルド済みの T3 Code サーバが必要です。GitHub Actions は Rust 1.100 を使います。Windows では Rust の MSVC ツールチェーンと、Visual Studio Build Tools の C++ ビルドツールおよび Windows SDK を使います。Git は `diff`、Python 3 は `exec` を使うときに必要です。

[上流の T3 Code](https://github.com/pingdotgg/t3code) をチェックアウトし、加速コンポーネントを組み込むパッチを順に適用します。このリポジトリのルートで実行します。

```sh
git -C /path/to/t3code switch --detach 4e6b1fa7710515cee652370b534e041a372648ea
for patch in patches/000*.patch; do
  git -C /path/to/t3code apply "$PWD/$patch"
done
```

T3 Code のソースで `pnpm install --frozen-lockfile` と `pnpm exec vp run --filter t3 build` を実行すると、Web クライアントを含む `apps/server/dist/bin.mjs` ができます。

このリポジトリのルートで実行します。

```sh
node scripts/t3-accel.mjs build
node --test crates/t3-native-sqlite/test.mjs scripts/t3-accel.test.mjs
```

### 配布ファイル

GitHub Actions のビルド対象は Linux と macOS の x64・arm64、および Windows x64 です。Linux 版は Debian 12 のコンテナでビルドし、Debian 12 と 13 で展開と起動をテストします。macOS 版と Windows 版は各 OS のランナーでテストします。

バージョンタグ（例 `v1.2.3`）を push すると、全プラットフォームのテストに合格した配布ファイルと `SHA256SUMS` を GitHub Release にまとめます。利用する OS と CPU に合うアーカイブを選び、展開前に SHA-256 を照合します。GitHub CLI の `gh attestation verify <archive> --repo sahenjp/t3-rust-accel` でビルドの来歴も確認できます。

### macOS

Xcode Command Line Tools をインストールします。Node.js と Rust のアーキテクチャをそろえてビルドし、Linux と同じ `serve` コマンドで起動します。

### Windows

PowerShell からサーバを起動します。

```powershell
node scripts/t3-accel.mjs serve 'C:\src\t3code\apps\server\dist\bin.mjs'
```

このコマンドは `.exe` 2 本と SQLite アドオンへのパスを渡し、既定でループバックに待ち受けます。既存の `T3CODE_HOME` と T3 Code の設定を使います。Python の場所を明示するには `T3_MAKI_PYTHON` に実行ファイルの絶対パスを指定します。

### Linux の systemd

フォアグラウンドでは Windows と同じ `serve` コマンドを使えます。既存の `t3code.service` に組み込む場合は、ビルド済みの CLI を配置して drop-in を準備します。

```sh
install -d ~/.local/bin
install -m 755 target/release/t3-rust-bridge target/release/t3-maki-tools ~/.local/bin/
scripts/apply-phase2.sh
systemctl --user daemon-reload
systemctl --user restart t3code.service
```

drop-in は、既定で `~/Projects/t3code` のサーバと `~/Projects/t3-rust-accel` のアドオンを指します。別の場所でビルドした場合は、生成された drop-in の `ExecStart` と `T3_NATIVE_SQLITE_PATH` を編集します。サービスの再起動は実行中の T3 Code セッションを閉じてから行います。

## テレメトリは無効

drop-in には次の設定が入っています。

```ini
Environment=T3CODE_TELEMETRY_ENABLED=false
Environment=T3CODE_OTEL_SDK_DISABLED=true
```

この設定で、製品テレメトリ (`ingest.t3.codes`) と relay の OTLP tracing が止まり、環境変数を継承するバンドルエージェントの analytics connect-RPC も止まります。

ソースの `patches/0001-telemetry-opt-in-by-default.patch` を適用すると、環境変数の指定がない場合もテレメトリは無効です。続く `patches/0002-scope-mcp-registration-buffer.patch` は、`T3_DEFER_MCP_TOOLS` や `T3_STATIC_TOOL_LIST` を使う際の MCP 登録エラーを修正します。

## Antigravity の接続

サーバのソースに `patches/0003-use-installed-antigravity-cli.patch` を適用した状態で接続します。Google 公式の `agy` を別途インストールし、T3 Code のプロバイダ設定に実行ファイルのパスを指定するか、サーバの `PATH` に含めます。

T3 Code は公式 CLI を介して認証とセッションを扱います。利用履歴はローカルファイルから読み取ります。残りクォータの表示と、T3 Code からの CLI の自動ダウンロードは無効です。

## セッションは置き換えても残る

セッション、スレッド、プロバイダ鍵、ログは `T3CODE_HOME` (既定 `~/.t3`) の下にあります。サーバのバイナリを差し替えても、drop-in を付けて再起動しても、これらはそのまま引き継がれます。

## ロールバック

```sh
sh scripts/rollback-phase2.sh
systemctl --user daemon-reload
systemctl --user restart t3code.service
```

## 注意

- `exec` には信頼できる Python スクリプトを渡してください。実行時間とメモリに上限を設け、API キーなどの環境変数を既定で渡さず、ヘルパーで作業ディレクトリ外を参照できないようにしています。Python 自体には実行ユーザーの権限があります。
- ランチャーと systemd drop-in は既定でループバックに待ち受けます。
- drop-in の `T3CODE_RELAY_URL` と `T3CODE_CLERK_*` は、上流のリリースバイナリが埋め込む公開値です。T3 Connect を使わないなら削除してください。

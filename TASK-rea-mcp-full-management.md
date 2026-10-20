# TASK: REA（インタプリタ+スクリプト型MCP）の完全自動更新管理

作成日: 2026-10-20 / 作成セッション: REA導入作業側

## 目的

`node <script.mjs>` 形式で登録されたMCPサーバー「rea」を、DTMの完全な自動更新管理下に置く。
現状 `dtm manage rea` はアダプター未実装で拒否されるため、ランチャーが `node <generation>/scripts/rea.mjs mcp` のようにインタプリタ経由の世代切替起動を扱えるようにする実装が必要。

## 対象リポジトリ・環境

- DTM ソース: `C:\Users\avalo\projects\Devin-Tool-Manager`（このファイルのあるリポジトリ）
- 仕様書: `Devin-Tool-Manager-Specification.md` — §14 第1段階・§15 受け入れ条件、§9/§15-13（パッケージ管理下の実体を直接上書きしない）、§13（設定変更バックアップ・競合検知）を遵守
- dev 用 DTM ルート: `C:\work\dtm-dev`（`dtm.root` マーカーにより `C:\work\dtm-dev\dtm.exe` 実行時に自動選択）
- 本番 DTM ルート: `%LOCALAPPDATA%\Programs\Devin Next\DevinToolManager`（実運用の管理MCPはこちら）
- REA 実体: `C:\work\rea`（git checkout、morluto/rea、v6.7.0、build・依存導入済み）
- MCP 設定: `C:\Users\avalo\AppData\Roaming\devin\mcp_config.json` の `rea` エントリ = `node C:\work\rea\scripts\rea.mjs mcp`

## 現状（本セッションで実施済み・変更不要な作業）

- `rea` は **dev ルート（C:\work\dtm-dev）の registry に登録済み**
  - installationId: `6873ce5baf5a9c6adfce2d4dd673c0ed`、kind `mcp-stdio`、usages = user mcp_config `rea`
  - 登録コマンド: `dtm register-mcp rea --source morluto/rea` → `dtm set-source` → `recipe set` で調整済み
- 現在の recipe（`state/registry.json` 内 rea エントリ）:
  - `updateProvider.type = "package-manager"`（manager: `npm`, package: `rea-agents`）
  - `artifact`: kind `zip`, rootPattern `"package"`, entrypoint `"scripts/rea.mjs"`
  - `verify`: `node {entrypoint} --version` → semver
- `dtm check rea` → `Unmanaged`。`dtm manage rea` → 失敗:
  - `'rea' はインタプリタ+スクリプト形式のため管理化できません (アダプター未実装。一覧登録のみ可)`
- mcp_config.json は未変更（元の `node ... rea.mjs mcp` のまま）。

## 調査済みの実装根拠（再調査不要）

- 管理化拒否の判定: `crates/dtm-core/src/register.rs` `ensure_manageable()`（約220-252行目付近）
  - パッケージ管理下（npx/uvx）拒否 → 委譲（仕様 §9, §15-13）
  - インタプリタ+スクリプト拒否 → 「アダプター未実装」
  - 約663-666行目に表示用の同一判定あり（「インタプリタ+スクリプト形式」）
- スクリプト形式の検出基盤は存在: `crates/dtm-core/src/resolve.rs`
  - `is_interpreter_command()`（node/python/bun/deno、約236行目）
  - `interpret_local_script()`（`node <script>` からスクリプト実パスと成果物ルートを推定、約262行目）
- 管理エントリの実例: 本番 `state/managed-mcp.json`（flaui-mcp / serena / windows-mcp / excel-mcp-forFILES）
  - `{usageKey, installationId, configPath, serverName, original:{command,args,env,cwd}, configSha256AtWrite, managedAt}`
  - 設定書換後は `dtm.exe launch mcpkey:<usageKey> -- <元args...>` 経由起動
- UpdateProvider 実装済み型: `github-release` / `manual` / `package-manager`（バイナリ文字列確認）
- `dtm probe-mcp` は `mcpkey:` 起動を実体 exe+元引数へ解決して spawn 済み（インタプリタ形式への対応追加時はここも要確認）
- REA の GitHub リリース（`rea-agents-6.7.0`）には **アセット無し** → `github-release` 型では取得不可
- npm パッケージ `rea-agents` の tarball は `package/` ルート配下に `dist/`（ビルド済み）・`scripts/rea.mjs`・`native/windows/build/*` を同梱 → **単体で `node scripts/rea.mjs mcp` が起動する**
- npm tarball は tar.gz。仕様「初期版の圧縮対応はZIP。tar.gzや7zは対応状況を明示し、未対応形式を暗黙に処理しない」に注意

## 要件・設計論点（結論は仕様書と実装に従って決定）

1. `manage` がインタプリタ+スクリプト形式を受理できること（解決実体=スクリプトパス、起動はインタプリタ維持）。`original` の保存・`unmanage` 復元・設定競合検知を従来通り行う
2. 世代（generation）適用後の起動: ランチャーが確定世代内のエントリポイント（`scripts/rea.mjs`）を解決し `node <path> mcp` を spawn。引数・env・cwd・stdio 透過性・終了コードを保持（仕様 §156行目付近の汎用ランチャー要件）
3. 更新取得経路の選定（要設計判断）:
   - (a) npm レジストリから tarball を取得する新プロバイダー、または `manual`+`import` 経路（外部で `npm pack` → `dtm import`）
   - (b) `package-manager` 委譲（npm 自身へ更新委譲）— ただし現在の rea は npm 経由インストールではなくローカルスクリプト起動のため、委譲先が存在しない。選ぶなら起動形態自体を npm 管理下（`npx rea-agents` 等）に変える必要があり、DTM 管理外となる
   - 「DTMが版・世代・rollback・pin を管理する」が目的なら (a) 系
4. 世代適用先は DTM 管理下の世代ディレクトリ。`C:\work\rea` の git checkout を成果物で上書きしない（checkout は開発用として残す）
5. `probe-mcp` / scan / list / status / extension 表示がインタプリタ形式でも破綻しないこと（ツール数取得・帰属・分類表示）
6. 既存管理エントリ（windows-mcp, serena, excel-mcp-forFILES, flaui-mcp）の挙動を変えない

## 禁止・境界

- `main` へ直接コミットしない。作業ブランチまたはPRを成果物にする
- mcp_config.json の変更は DTM の既存機構（バックアップ・ハッシュ競合検知）経由のみ
- MCP stdio の stdout に管理ログを出さない（§15-9）
- 未検証の透過性を「対応済み」と見なさない（仕様: 未検証なら固定パス方式または一覧管理のみ）
- rea リポジトリ（C:\work\rea）・本番 DTM ルートの既存エントリは実装の検証で破壊しない。検証は dev ルート（`--root C:\work\dtm-dev`）で行い、本番反映は別途承認を得る

## 検証方法

1. `cargo build` / `cargo test` / `cargo clippy` / `cargo fmt`（既存16件を維持し、インタプリタ形式の受理・起動解決のテストを追加）
2. dev ルートで実機確認:
   - `dtm manage rea` → mcp_config が `dtm.exe launch mcpkey:...` 経由になること（バックアップ生成・original 保存）
   - 管理化状態で `dtm launch mcpkey:<key> -- mcp` 相当が `node C:\work\rea\scripts\rea.mjs mcp` を正しく起動（現行版フォールバック）
   - 更新フロー: `check` → `prepare` → `apply` で世代が作られ、`node <世代>\scripts\rea.mjs --version` が成功し、次回起動で新世代に切替わること
   - `dtm unmanage rea` で元設定へ復元できること
   - `dtm probe-mcp rea` でツール数が取得できること（実機観測: 管理化MCPは実体解決して spawn）
3. Devin 再起動後に rea MCP が接続されること（`node C:\work\rea\scripts\rea.mjs doctor --client devin --json` が healthy: true を返す）

## 成果物

- 作業ブランチ/PR（変更ファイル一覧・設計判断の根拠を説明）
- 実施した検証と結果（コマンド・出力要約）、未検証事項の明示
- 仕様書・AGENTS.md の実環境メモへの追記（該当する場合）

## レビュー観点

- インタプリタ判定・スクリプト解決が既存の `is_interpreter_command` / `interpret_local_script` を再利用し一貫しているか
- 世代の成果物形式（npm tarball=tar.gz）の扱いが仕様の圧縮対応方針と整合するか
- `package-manager` 委譲との住み分けが仕様 §9/§15-13 と矛盾しないか

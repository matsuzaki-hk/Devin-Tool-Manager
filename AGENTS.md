# AGENTS.md — Devin-Tool-Manager

## 目的・概要

Devin Desktop (Devin Next) が利用するローカル MCP サーバーと一般 CLI ツール（RTK 等）を一元管理するツールマネージャ。更新確認・成果物取得/検証・世代別配置・次回起動時適用・復元を担う。

- 仕様書: `Devin-Tool-Manager-Specification.md`（実装の絶対条件。特に §14 第1段階・§15 受け入れ条件に従う）

## 環境・前提

- OS: Windows 11 / Devin Next `1.126.0-next`（Windsurf=VS Code フォーク）
- MCP 設定: `%APPDATA%\devin\mcp_config.json`（v3000.3+ 形式、`mcpServers` キー）
- 拡張機能: `~/.devin-next/extensions` に VSIX 導入可（標準 VS Code 拡張 API + Webview 利用可）
- 配置先（DTM_ROOT）: `C:\Users\avalo\AppData\Local\Programs\Devin Next\DevinToolManager\`
- Rust ツールチェーン、Node.js（拡張開発用）が必要

## 構成

- `crates/dtm-core` — 更新エンジン等のライブラリ（UI 非依存）
- `crates/dtm-cli` — `dtm.exe`（CLI + `launch` サブコマンドが MCP/CLI ランチャー兼用）
- `extension/` — VS Code 拡張（TypeScript + Webview、後続実装）

## 主要コマンド

- ビルド: `cargo build` / `cargo build --release`
- テスト: `cargo test` (16 件: 版比較/パス正規化/ZIP安全性/アセット選択/pkg検出)
- lint: `cargo clippy`、`cargo fmt`
- インストール: `target/release/dtm.exe` → `<DTM_ROOT>\dtm.exe` + `dtm.root` マーカー同梱
- 拡張: `extension/` → VSIX (ZIP: `[Content_Types].xml`, `extension.vsixmanifest`, `extension/*`) → `devin-desktop-next --install-extension <vsix>`

## 実運用メモ

- dev 用ルート: `--root C:\work\dtm-dev` (本番とは分離)
- サンドボックス検証ツール: `ttool`(fixed-path), `vtool`(versioned) → `C:\work\dtm-test-tool*` (削除可)
- PATH は `export PATH="$HOME/.cargo/bin:$PATH"` が必要 (rustup 経由)
- Python は UTF-8 非対応コンソールで日本語出力に注意 (`PYTHONUTF8=1`)

## 規約・注意点

- コード・コメント・ドキュメントは日本語または英語。回答は日本語
- MCP stdio 経路の stdout に管理ログを出さない（受け入れ条件 §15-9）
- MCP 設定変更前に必ずバックアップ、外部編集は競合検知（仕様 §13）
- `installationId` = 正規化配置先 + 実行環境（Windows 大小文字差・リンクを考慮）
- チェックサム/署名未提供は「未検証」記録。検証成功扱いにしない
- パッケージ管理下の実体（npx/uvx/winget）は直接上書きしない

## 判明している実環境メモ（2026-10-07 調査）

- 実 MCP 登録: npx/uvx 経由・絶対パス EXE・裸コマンド名・リモート HTTP/SSE が混在
- RTK: `C:\work\rtk\rtk.exe`（`rtk-x86_64-pc-windows-msvc.zip` 配布、固定パス参照あり→固定パス差し替え方式の候補）

## MCP表示改修・連携検証（2026-10-12）

- 合意した分類: DTM管理中MCP / DTM未管理MCP / CLIツール。未管理は A:更新元未設定、B:URL接続型（サーバー本体は管理対象外）、C:Devinカタログ導入（DTMから更新しない）。C/B/Aの優先順位で判定し、表示はA/B/C・各分類内で名前順。導入元は名前から推測しない。
- 接続状態と更新状態は別管理。プロセス存在や設定の有効だけを根拠に接続済みと表示しない。
- 同梱 `extensions/windsurf/dist/acp/AGENTS.md` に `cognition.ai/mcp/{listServers,toggleServer,toggleTool}` が記載されているが、DTMから利用する入口は未検証。
- 拡張コマンド `dtm.diagnoseMcp` はAPI名・コマンド名だけを出力する読み取り専用診断。外部拡張の起動、MCPの起動、設定変更は行わない。
- 拡張診断テスト: リポジトリルートで `node --test extension.test.cjs`。構文確認: `node --check extension/extension.js`、`node --check extension/media/webview.js`。
- VSIX版番号は `extension/package.json` と `extension/vsixmanifest.xml` の双方を一致させる。
- 第2段階（0.4.7）: 設定基準のMCP一覧統合・A/B/C分類・名前順を実装。分類と更新除外の共通ロジックは `extension/media/catalog.js`（NodeとWebviewで共有）。
- `dtm --json scan --diagnostics` は `{servers, errors}` を返す。通常の `--json scan` は従来の配列形式を維持。設定読み取りの一部失敗でも他の設定は列挙し、拡張は失敗した設定の前回行を警告付きで保持する。
- スキャンはユーザー設定・開いているワークスペース・明示した追加プロジェクトを対象とし、設定の作成/変更/削除に追従する。`serverUrl` と `url` の双方に対応。
- C分類は `registry` 記録またはユーザーの明示指定で判断する。記録のないカタログ導入は推測しない。明示指定は拡張の `globalState.catalogEntries` に保存し、MCP設定を変更しない。
- 0.4.7では有効状態は読み取り表示のみ。接続状態は不明（無効設定の場合は灰色）、ツール数は未取得として表示する。トグルの動作連携とDevin実行時状態の取得は未完了。
- 実機診断で `windsurfMcp.updateMcpServers`、`windsurfAcp`、標準拡張の `devClient` を確認したが、公開された状態取得経路と標準MCP画面との対応は未確定。名前だけを根拠にAPIを呼び出さない。
- `devClient` の公開メンバーは `dev` と `getLSPCompletionItems` のみで、MCP状態の読み取り経路ではないことが確定（0.4.8診断で実機確認）。
- `workbench.action.getAcpState` は 0.4.9 の `probeAcpState` で `vscode.version === "1.126.0-next"` の時だけ読み取り専用実行し、`reduxStore` 配下のスライス名とフィールド名（型のみ）を診断出力する。値・チャット本文・認証情報は出さない。版不一致では実行しない。
- 有効切替のDevin本体への即時反映は実機確認済み。Devin標準MCPリストのトグル表示は自動再読込されない（ユーザー判断で問題なしとされた）。
- 0.4.8: MCP一覧は幅300pxでも全列を表示（版列省略は280px以下、ツール数列省略は220px以下）。CLIもクリック展開型の行表示で、有効状態・接続・ツール数は設けない。カタログ分類の変更成功時は対象行の詳細・フォームを閉じる。
- `dtm set-mcp-enabled <server> --config <path> --enabled true|false [--expected-disabled true|false]` はdisabledだけを変更し、未知キーを保持する。DTM間の設定変更はmcp-configロックで直列化。バックアップ名は時刻・PID・連番・設定元キーを含め、短時間の切替や別設定で上書きしない。
- 自分の有効切替前の設定ハッシュに一致する管理記録だけを更新する。外部編集が含まれるハッシュを自動承認しない。標準Devin側の接続開始・終了が即時に反映されることは未検証で、接続状態・ツール数の実行時連携は引き続き未完了。
- CLIまで含む拡張テスト: `DTM_TEST_CLI` にビルド済みdtm.exeの絶対パスを設定して `node --test extension.test.cjs`。未設定時は実CLIテストのみスキップする。
- 0.4.9-0.4.11 で実機確定: `vscode.lm.tools` はMCPツールを含まない、`mcpServerDefinitions` は空、`lm.startMcpGateway` はスタブ、`handleClientRequest("devin-cli"|"cascade", …)` は ok:true だが応答を返さない転送API → 公開API経由の状態取得は不可。
- 0.4.12 の実行時状態取得 (`extension/livestate.js`): `%APPDATA%\Devin - Next\logs\<session>\window<N>\exthost\codeium.windsurf\` 配下を読み取り専用で解析。`MCP <name>.log` は拡張ホスト側の接続イベント、`Devin.log` は devin-cli エージェント (mcp-go) の `Server '<key>' initialized successfully: <表示名> v<版>`、`readResponses got line: {"result":{"tools":[…]}}`、`All connection methods failed for <url>` を持つ。
- tools応答にサーバー名が無いため帰属は署名(先頭ツール名→サーバー)＋「pendingが1件の一意な組のみ帰属・学習」。署名は cutoff 無視で全行から学習する（cutoff後のwave単独では順序交差を解決できず誤帰属する実例あり: context7/pdf-reader 入替）。pending複数＋署名不一致の曖昧な応答は帰属せず `tools=null`（誤った数を出さない）。HTTP/SSEのMcpRemoteはSTDIO応答を出さないためpendingに入れない。
- ウィンドウ特定: activate時に診断出力へ書く `[marker] dtm-window-<uuid>` を `output_logging_*/*DTM*.log` から検索。無い場合は全ウィンドウ併合で `approx=true`。
- 既知の制約: github-mcp-server(リモート)はtools数をログから取得できず null、serena(exthost側)は応答が無く tools=null、再読込直後は未取得表示になり得る。Devin.log の読み取り上限は 2MB。
- 再起動ヘルパー (relaunch-wait): プロセス存在確認は tasklist ではなく Toolhelp32 (`windows` クレート, dtm-cli のみ依存)。コンソール無しのヘルパーから tasklist を起動すると毎回CMD窓が点滅するため。ポーリングは 250ms/300s上限。
- 子プロセスの窓点滅抑制: `dtm-core::procutil` の `hide_window` をパイプ回収する spawn 全般に適用 (git/npm/uv/powershell 等)。`launcher.rs` の inherit 起動は `parent_has_console()` で分岐—対話CLI利用時は親コンソールを継承させる。
- 委譲更新(pm-sync)のロック対応: 実行中ツールで `os error 32` (ファイル使用中、ロケール非依存の安定文字列) が出た場合は `InstallState.pending_pm_sync` に予約を記録し即失敗にしない。`dtm launch` 境界で `ops::run_pending_pm_sync` が未起動状態で再実行し、成功時に解除。失敗しても現行版で起動継続。
- uv-tool の版検出: shim は Python スタブで `--version`/PE版を返さない。`uv tool run --from <pkg> python -c importlib.metadata` で env 内版をプローブ (実行中でも可能)。check と pm-sync の before/after で `"uv" | "uv-tool"` を同一扱いにする。
- 中断したジョブの回復 (2026-10-14): 進行中ステータス (Checking/Preparing/Applying) が残存したまま実行者が死亡した場合、`lock::heal_interrupted` がロック取得成功=実行者不在を根拠に Failed+「中断」へ織り下げる (list/status の読み取り経路で発動)。JobGuard は次回 begin 時に prev running → interrupted へ自浄するが InstallState.status は残るため、この読み取り側回復が必要。
- prepare の内部デッドライン: 拡張の execFile kill (prepare=900s) より先に自分で失敗終了するため 780s。reqwest blocking の `timeout` は `send()`/ヘッダ受信までしか効かず、ボディの `read()` はソケット停滞時に無制限ブロックする実測あり → `fetch::bounded` でダウンロード/テキスト取得/releases をワーカースレッド実行し deadline で遮断する (残留スレッドは one-shot CLI 終了時に OS 回収)。`layout.staging_dir()` = `<DTM_ROOT>\staging` (state\ 直下ではない)。prepare 開始時に同 installation の `<id>-*` staging 残骸をロック内で回収する。
- 実測 (2026-10-14): GitHub release 資産 68MB が ~100-500KB/s でしか落ちない環境では prepare に約5分かかる (拡張タイムアウトは prepare のみ 900s)。再起動は quit→全プロセス終了 約36-39s (Devin 自身の終了処理、DTM側オーバーヘッドは検知0.25s+spawn5-8sのみ)。
- ビュー末尾の Devin 操作ボタン (0.4.12): webview `__ui:<action>` → extension `uiAction()` が `executeCommand` へ転送。検証済みコマンド: `workbench.action.newWindow` (新規ウィンドウ), `devin.openCustomizationsTab {tabId:"rules"|"skills"|"mcps"|"plugins"|"subagents"|"hooks"}` (カスタマイズエディタのタブ。workbench.desktop.main.js に実装あり), `revealFileInOS` (ワークスペースフォルダをOS表示)。`devin.lifeguard.openRules` は activeEditor の `.devin/lifeguard.yaml` コンテキスト依存で汎用でないため不採用。
- 進捗表示 (0.4.12): `lock::Progress` が `state/jobs/<id>.progress.json` を書く (ワーカースレッド単独書き込みのため JobState と別ファイル。atomic_write は全置換のため同一ファイルだと finish/fail と競合する)。読み取り側は `active_progress` で job=running をゲート。拡張は longOps 実行中 2.5s 間隔で `list` のみ再取得して表示 (VM テスト環境に setInterval が無いため typeof ゲート必須)。
- `pending_pm_sync` は list JSON の `pendingPmSync` として UI 表示 + `Status::display` の末尾サフィックス (startsWith による webview 表示分類を壊さないよう末尾追記)。
- `dtm cleanup [--id] [--keep N] [--dry-run]`: `stage::prune_candidates` (prune_generations の選定部を分離) で候補列挙→非ブロッキングロック内で削除。現行・適用待ち世代は保護。prepare 成功時の自動整理 (keep_generations 既定3) は従来どおり動作するため、これは手動トリガ用。
- `dtm diagnose [--out <zip>]`: registry/managed/installations/jobs/log末尾1MB/マスク済み mcp_config を zip 化 (既定 `<root>/state/diagnostics/`)。`redact_json` で env/headers/token/secret/password/apikey/credential を含むキー名の値を "***" にマスク (usage_key 等 "key" を含むが秘匿でないキーは対象外にするため裸の "key" はマスク対象に含めない)。拡張は完了後 revealFileInOS で保存先を表示。
- 「全件 準備」(prepare-all): 「更新あり」状態の非委譲インストールを順に `prepare` 逐次実行 (ロック・帯域競合を避けるため並列化しない)。`pkg:` 委譲は対象外。
- 起動時適用完了の通知: `refreshState` で前回 `prepared` → 今回 `current` の遷移を検出し `showInformationMessage` (初回ロードは通知しない。テストVMには同APIが無いため try/catch 隔離)。
- 実行時状態の自動更新は `fs.watch`(livestate ディレクトリ) + 1.2s debounce で実装済み (`scheduleLive`) — 定期ポーリングではなくログ更新駆動。
- フッター追加ボタン: `__ui:open-mcps` (カスタマイズMCPタブ), `__ui:open-mcp-config` (`%APPDATA%\devin\mcp_config.json` を `vscode.open`), `__ui:open-dtm-log` (`<root>/state/logs/dtm.log`)。
- `dtm.exe.old-*` 残骸は `run()` 冒頭の `cleanup_old_exe()` が自動回収 (使用中のファイルは削除失敗するだけで無害)。
- 配布インストーラ (0.4.12): `installer/setup.iss` (Inno Setup 6, `C:\Users\avalo\AppData\Local\Programs\Inno Setup 6\ISCC.exe`)。`installer\build.ps1` が cargo build → pack_vsix.py → payload ステージング → ISCC を一括実行し `installer/dist/dtm-setup-<版>.exe` を生成。版は `extension/package.json` から自動取得 (`-Version` で上書き可)。
- インストーラ仕様: ユーザーモード (`PrivilegesRequired=lowest`)、固定パス `{localappdata}\Programs\Devin Next\DevinToolManager` (拡張の candidatePath に一致、ディレクトリ選択不可)。`BeforeInstall` で既存 dtm.exe を `.old-<時刻>` へ退避してから上書き (実行中ロック対策)。`ssPostInstall` で Devin CLI (`{localappdata}\Programs\Devin Next\bin\devin-desktop-next.cmd` → `{autopf}` → `where` の順に探索) を非表示で呼び `--install-extension`。Devin CLI 不在時は拡張のみスキップして手動導入手順を案内。アンインストール時は `--uninstall-extension local.devin-tool-manager` + 導入ファイルのみ削除 (`state/`/`generations/`/`backups/` 等の管理データは温存)。`/VERYSILENT /NORESTART` で無人導入可。実機で導入→アンインストール→再導入の全サイクル検証済み。

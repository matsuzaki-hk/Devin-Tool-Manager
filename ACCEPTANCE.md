# 受け入れ条件 (仕様 §15) 検証結果

2026-10-07 初回実施 → レビュー指摘 (R1–R11) 修正後 2026-10-07 再検証。
検証環境: Windows 11 / Devin Next 1.126.0-next / dtm 0.2.0
(dev root `C:\work\dtm-dev`, サンドボックス `C:\work\dtm-test-tool*`)。

結果区分: **実測**=実環境・実プロセスで確認 / **コード確認**=修正コード+ユニットテストで担保 / **未検証**=要実機または第2段階。

| # | 条件 | 結果 | 検証方法 |
|---|---|---|---|
| 1 | 複数ウィンドウ同時起動で同一候補の取得・適用が重複しない | 実測 | `check`×3 並行で 1件実行+2件 `[既ジョブ結果]` 再利用。`prepare` は Ready 済み同版を世代再利用 |
| 2 | 同じ EXE が異なる登録名でも同一インストールとして排他 | 実測+テスト | installationId = 正規化配置先+環境の SHA-256。`manage` が既存 `ttool` と同一 ID で usages マージ。ユニットテストで等価パス一致確認 |
| 3 | 準備/適用途中終了→旧版または検証済み新版へ復旧、中間状態を起動しない | 実測+コード確認 | (a) `JobGuard` が `interrupted` 記録 (b) fixed-path は計画を**先行記録**したジャーナル+実ファイル照合で冪等復旧、復旧失敗は `rollback-failed` で起動中止 (c) `atomic_write` は `MoveFileExW(REPLACE_EXISTING)` 相当の単一 rename で gap なし (d) 不完全世代は `verified/applied` ゲートで起動除外。中断区間の故障注入試験はコード確認＋ `swap_journal_recovers_interrupted_state` テストで担保 |
| 4 | 取得失敗/破損ZIP/検証不一致/複数一致で現在版を変更しない | 実測+テスト | ステージング完遂まで現行不変。アセット 0件/複数件 → `NeedUser` (テスト)。checksum 不一致 → `Verify` 拒否。ZIP 安全性テスト4件 |
| 5 | 配置プレビューと実配置の一致・保持データ維持 | 実測 | `preview` と `materialize` が同一 `plan_placement` を使用。`+ /= /-` 表示を実測 |
| 6 | 準備完了のみ「次回起動時に更新」表示・取消後は不適用 | 実測 | Ready 時のみ表示。`cancel` 後は prepared 解除・launch で非適用を実測 |
| 7 | 使用中の固定パス更新は保留・理由表示 | 実測 | `CreateFile(share=0)` 排他保持中の `apply` → `更新待機中：他のウィンドウで使用中` + 対象ファイル表示。解除後は適用成功 |
| 8 | ソース未指定の MCP/一般ツールが通常起動 | 実測 | `updateProvider: null` → `更新元未設定`。launch/手動 import で運用可 |
| 9 | MCP ランチャーで初期化・呼出・終了、stdout 非混入 | 実測 | 実 MCP サーバー (FlaUI.Mcp.exe) をテスト設定で管理化し `dtm launch mcpkey:*` 経由で JSON-RPC 送受信: `initialize`→`protocolVersion`/`serverInfo` 応答、`tools/list`→tools 配列返却、**stdout 非 JSON 行 0**、stdin 切断で code=0 終了、unmanage で元設定復元。残るは本番設定適用後の Devin 実機確認のみ |
| 10 | 空白・Unicode・引用符引数・stdin/out/err・終了コード保持 | 一部実測 | 引数は配列渡し・stdio inherit・終了コード透過 (異常終了 `exit=127` 実測)。cwd は呼び出し元継承を `rtk ls` で実測 (R9 修正)。**未実施**: 専用子プロセスでの引数境界/Unicode/stderr 分離の網羅試験 |
| 11 | RTK 実体特定・EXE更新後も設定/統計保持 | 一部実測 | `rtk-ai/rtk` + 既知アセット + `C:\work\rtk` 照合。`data_preserve` で `backup/**,.rtk/**,hooks/**,*.toml,*.json` 除外。**サンドボックスでは差し替え・復元・旧配布物退避を実測**。本番 `rtk.exe` 差し替えはユーザー判断で実施のため実環境「更新後の保持」は未実測 |
| 12 | MCP 設定並行編集で競合検出・管理解除で元起動設定復元 | 実測 | SHA-256 照合で外部編集検出 → strict `unmanage` 拒否、`--force` で復元。未知キー (`custom-unknown-key`) 保持を実測 |
| 13 | パッケージ管理下の EXE を ZIP 更新経路で上書きしない | 一部実測 | npx/uvx 検出 → `PackageManager` provider で `unmanaged` + `ensure_manageable` が manage を拒否 (R5)。**残課題**: winget/Cargo 由来 EXE を絶対パス登録した場合の出典管理は「登録項目保持」の設計追加として未対応 |
| 14 | (第2段階) ビルド失敗・タイムアウト・成果物不一致で旧版維持 | — | 対象外。現状は手動取込のみ |

## R 指摘への対応状況

| 指摘 | 対応 | 状態 |
|---|---|---|
| R1 Webview 構文エラー | ブラウザ JS を `media/webview.js` へ外部化。テンプレート二重エスケープ解消。`node --check` 合格 | 修正済 |
| R2 atomic_write 非原子 | `fs::rename` (Windows: MoveFileExW REPLACE_EXISTING) の単一 rename へ変更 | 修正済+テスト |
| R3 ジャーナル事後記録 | 計画 (backup/place/stale) を変更前に記録、復旧は実ファイル照合で冪等。失敗は Err 伝播し `rollback-failed` で起動遮断 | 修正済+テスト |
| R4 ロック解放〜spawn 競合 | 復旧確認→pending適用→世代解決→spawn を同一ロック内に移動。fixed-path のロック timeout は起動拒否 (旧版不在区間を起動根拠にしない) | 修正済+テスト |
| R5 npx/uvx・スクリプト型 manage 破壊 | `ensure_manageable` が pkg管理下/インタプリタ/非exe を設定変更前に拒否 | 修正済+テスト |
| R6 同名 MCP の誤解決 | `mcpkey:<config+name hash>` を起動引数へ記録。同名は一意キー必須 | 修正済+実測 |
| R7 pin 未強制 | `select_release_for_policy` を check/prepare 共有化、apply/import は版一致ゲート | 修正済+テスト+実測 |
| R8 未検証世代の rollback 可 | genmeta に `verified`/`applied_at`。復元は適用済履歴から選択 | 修正済+テスト |
| R9 cwd が EXE 親になる | 呼び出し元 cwd 継承、利用元の明示 cwd/env のみ適用 | 修正済+実測 (`rtk ls` が呼出元dir) |
| R10 旧配布ファイル残存 | genmeta `files` マニフェスト差分で旧配布物をバックアップ退避 | 修正済+実測 (extra.dll 除去) |
| R11 引数ログ記録 | 起動引数本文をログから除去 | 修正済 |

## サイドバー対応

| 要件 | 対応 |
|---|---|
| `viewsContainers.activitybar` (id:dtm, アイコン) | `media/dtm.svg` 追加、manifest 登録済 |
| `views.dtm` webview `dtm.manager` | `registerWebviewViewProvider` で登録 |
| `dtm.open` → ビューフォーカス | `dtm.manager.focus` を実行 |
| 縦型カード UI | 8列テーブル廃止 → カード+展開詳細 |
| MCP/CLI 区分・名前/状態/現在→準備表示 | セクション分離 + バッジ表示 |
| 処理中の重複操作抑制・行へエラー保持 | `inFlight` 集合 + `itemErrors` で実装 |
| 登録/ソース/アセット/保持除外/プレビュー/版固定 | 詳細パネルに操作入口追加 |
| プロジェクト MCP の設定パス伝搬 | `scan` JSON に `configPath`、action で `--config` 付与 |
| CSP+nonce・外部JS・イベント委譲 | `media/webview.js` + nonce CSP。インラインonclick廃止 |

## 未検証・残課題

- Devin Next 実機でのサイドバー表示 (VSIX 0.2.0 導入済み、要 Devin 再起動)
- 本番 `mcp_config.json` への `manage` 適用 + Devin 実機での対象サーバー接続確認 (プロトコル自体は実 MCP サーバーで E2E 実測済み)
- `all-windows-closed` 適用条件・Devin 起動ランチャー・署名検証・pkg委譲・自動ビルド (第2段階)
- winget/Cargo 由来 EXE のインストール出典管理 (登録時メタとして保持する設計追加)
- 実環境 `mcp_config.json` への `manage` 適用はユーザー判断

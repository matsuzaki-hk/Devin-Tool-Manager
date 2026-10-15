# Devin-Tool-Manager

Devin Desktop (Devin Next) が利用するローカル MCP サーバーと一般 CLI ツール
(RTK 等) を一元管理し、更新確認→取得→検証→世代別配置→次回起動時適用→復元を行う。

仕様: `Devin-Tool-Manager-Specification.md`

## インストール

配布インストーラ `dtm-setup-<版>.exe` (Inno Setup) を実行すると、
`dtm.exe` を `%LOCALAPPDATA%\Programs\Devin Next\DevinToolManager\` へ配備し、
同梱 VSIX を `devin-desktop-next --install-extension` で自動導入する
(ユーザーモード・管理者権限不要・アンインストーラ付き)。
ビルド: `powershell -ExecutionPolicy Bypass -File installer\build.ps1`

前提: Devin Next と Rust ツールチェーン (ソースからビルドする場合)。

## 構成

| 要素 | 実装 | 場所 |
|---|---|---|
| 更新エンジン+ランチャー | `dtm.exe` (Rust 単一バイナリ) | `%LOCALAPPDATA%\Programs\Devin Next\DevinToolManager\` |
| 管理 UI | VS Code 拡張 (プライマリサイドバー WebviewView) | `~/.devin-next/extensions/local.devin-tool-manager-0.2.0` |
| 状態/世代データ | JSON + 世代ディレクトリ | `<root>/state`, `<root>/generations` |
| ソース | Cargo ワークスペース | `crates/dtm-core`, `crates/dtm-cli`, `extension/` |

`dtm.exe` は起動経路そのものとして使われるため、MCP 設定上の `command` は
`<root>\dtm.exe` + `launch mcpkey:<key>` へ書き換えられる (管理化)。
`mcpkey` は「設定ファイルパス+サーバー名」のハッシュで、同名サーバーが
複数プロジェクトにあっても一意に解決される。元設定は `state/managed-mcp.json`
に保存され管理解除で復元される。

## ルート解決順

1. `--root <path>` / `DTM_ROOT` 環境変数
2. `dtm.exe` 隣接の `dtm.root` マーカー (インストール配置)
3. `%LOCALAPPDATA%\Programs\Devin Next\DevinToolManager`

## 基本操作

```powershell
dtm init                      # ルート初期化
dtm scan                      # MCP 設定スキャン (user + --project で指定プロジェクト)
dtm list                      # 管理対象一覧
dtm add-cli <名> <パス|dir|rel|コマンド名> [--strategy versioned]
dtm add-rtk [--dir <RTK配置Dir>]  # RTK を初期レシピで登録
dtm register-mcp <サーバー名> # MCP エントリから installation 登録 (設定は変えない)
dtm manage <サーバー名>       # mcp_config.json をランチャー経由へ書き換え
                            #   (直接EXE起動のみ。npx/uvx/スクリプト型は拒否)
dtm unmanage <サーバー名>     # 元の起動設定へ復元 (--force で競合時も復元優先)
dtm managed                 # 管理化済みエントリ一覧 (usage key/config/name)
#   register-mcp/manage/unmanage は --config <設定パス> でプロジェクト/ローカル指定可

dtm check <id|--all>          # 更新確認
dtm prepare <id>              # 取得・検証・世代作成・版確認 → 適用待ち
dtm apply <id>                # 有効化 (versioned: ポインタ切替 / fixed-path: 差し替え)
dtm cancel <id>               # 予約取消 (現在版は変わらない)
dtm pin <id> [ver|--clear]    # 版固定/解除
dtm import <id> <file> --version <v>  # 手動成果物取込
dtm rollback <id> [generation]        # 旧版復元
dtm preview <id> <zip|dir|exe>        # 配置プレビュー
dtm set-source <id> <owner/repo> --asset <glob>
dtm generations|history|status <id>
dtm logs [--tail N]
dtm auth github --token <t>   # OS 資格情報ストアへ保存 (env: DTM_GITHUB_TOKEN でも可)

dtm launch <id|mcp:name|mcpkey:key> -- <args>  # ランチャー (stdio/終了コード透過)
dtm resolve <id|mcp:name|mcpkey:key>           # 起動ファイル実パス解決
```

全コマンド `--json` で機械可読出力 (拡張 UI が利用)。

## 拡張機能 (VSIX)

```powershell
# 再ビルド・再インストール
# installer\pack_vsix.py が extension/ を ZIP 化して .vsix を生成する
python installer\pack_vsix.py devin-tool-manager-x.y.z.vsix
devin-desktop-next --install-extension devin-tool-manager-x.y.z.vsix
```

Devin 内: アクティビティバーのレンチアイコン、またはコマンドパレット
`Devin Tool Manager: 開く` → プライマリサイドバーに一覧・操作 UI が出る。

## 状態機械 (仕様 §8)

`source-unset`(更新元未設定) → `checking` → `up-to-date` / `available`
→ `preparing` → `ready`(適用待ち) → `applying` → `up-to-date`
+ `waiting-in-use` / `failed` / `unmanaged`(pkg管理下・一覧のみ)

## 更新元の判定ルール (仕様 §5)

- GitHub URL 等の根拠ある情報のみ自動取得。名前推測は候補止まり。
- GitHub 指定だけでは更新不可。アセット条件(1件一致)・配置・版判定が
  確定して初めて自動更新が有効。
- npx/uvx 等のパッケージ管理下の実体は直接上書きせず `unmanaged` 表示
  (第2段階でアダプター委譲)。
- チェックサム/署名未提供は `unverified` 記録 (成功扱いしない)。

## 管理解除

```powershell
dtm unmanage <サーバー名>            # 競合なし: 元command/argsへ復元
dtm unmanage <サーバー名> --force    # 外部編集があっても復元優先
```

レジストリから installation 自体を外す場合は
`state/registry.json` の該当エントリを編集する (UI 化は今後)。

## 既知の制限 (第1段階)

- `applyWhen: all-windows-closed` 未実装 (next-launch / manual のみ)
- 署名検証 (`.sig`/PGP) は未実装。sha256 チェックサムのみ検証
- パッケージマネージャアダプター・自動ビルドは第2段階
- Devin 起動ランチャー (本体起動前の一括適用) は未実装
- WSL/SSH/コンテナー環境は識別のみ (windows-x64 のみ管理)

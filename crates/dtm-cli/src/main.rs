//! dtm — Devin Tool Manager CLI / ランチャー
//! `dtm launch` は stdout を一切汚染しない (MCP stdio 透過のため)。

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use dtm_core::models::*;
use dtm_core::{discover, mcpcfg, ops, paths, probe, register, stage, Store};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "dtm",
    version,
    about = "Devin Tool Manager: MCP/CLI ツールの更新管理"
)]
struct Cli {
    /// DTM ルート (既定: DTM_ROOT env → exe隣接 dtm.root → %LOCALAPPDATA%\Programs\Devin Next\DevinToolManager)
    #[arg(long, global = true)]
    root: Option<PathBuf>,
    /// JSON 出力
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// ルート構成を初期化 (dtm.root マーカー作成)
    Init,
    /// MCP 設定をスキャンしてサーバー一覧を表示
    Scan {
        /// 追加で読むプロジェクトルート
        #[arg(long)]
        project: Vec<PathBuf>,
        #[arg(long)]
        diagnostics: bool,
    },
    /// 管理対象一覧と状態
    List,
    /// 対象の詳細状態
    Status { id: String },
    /// CLI ツールを手動登録 (spec: 絶対パス | dir|相対 | コマンド名)
    AddCli {
        name: String,
        specifier: String,
        #[arg(long)]
        source: Option<String>,
        /// 適用方式: fixed-path(既定) | versioned
        #[arg(long)]
        strategy: Option<String>,
    },
    /// RTK を初期レシピで登録
    AddRtk {
        #[arg(long, default_value = "C:\\work\\rtk")]
        dir: PathBuf,
    },
    /// MCP サーバーを管理対象に登録 (設定はまだ変更しない)
    RegisterMcp {
        server_name: String,
        #[arg(long)]
        source: Option<String>,
        /// 設定ファイルパス (既定: ユーザースコープ)
        #[arg(long)]
        config: Option<PathBuf>,
    },
    #[command(about = "MCPの有効状態を変更（バックアップ・競合検知付き）")]
    SetMcpEnabled {
        server_name: String,
        #[arg(long)]
        config: Option<PathBuf>,
        #[arg(long, action = clap::ArgAction::Set, required = true)]
        enabled: bool,
        #[arg(long)]
        expected_disabled: Option<bool>,
    },
    /// MCP 設定をランチャー経由へ書き換え (管理化)
    Manage {
        server_name: String,
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// 管理解除: 元の起動設定へ復元
    Unmanage {
        server_name: String,
        #[arg(long)]
        config: Option<PathBuf>,
        /// 外部編集競合を警告のうえ復元優先
        #[arg(long)]
        force: bool,
    },
    /// 管理化済み MCP エントリ一覧 (usage key / config / server)
    Managed,
    /// MCP サーバーへ短命接続し tools/list でツール数を取得 (stdio 型のみ。
    /// 結果は state/mcp-probe-cache.json にキャッシュ)
    ProbeMcp {
        server_name: String,
        /// 設定ファイルパス (既定: ユーザースコープ)
        #[arg(long)]
        config: Option<PathBuf>,
        /// プローブ全体の上限秒
        #[arg(long, default_value = "25")]
        timeout: u64,
        /// キャッシュを使わず実プローブする
        #[arg(long)]
        no_cache: bool,
    },
    /// MCP プローブキャッシュの内容を JSON 出力 (拡張の表示用)
    ProbeCache,
    /// 登録解除: registry・状態・世代を削除 (実ファイルは消さない。
    /// 管理化済み MCP は unmanage 先行が必要)
    Remove {
        /// 対象 (id | name)
        id: String,
        /// 管理化済み MCP でも強制的に登録解除 (Devin設定は mcpkey: を指したまま残る)
        #[arg(long)]
        force: bool,
    },
    /// 更新確認
    Check {
        /// 対象 (省略時は --all 必須)
        id: Option<String>,
        #[arg(long)]
        all: bool,
    },
    /// 更新の準備 (取得・検証・世代作成・版確認)
    Prepare { id: String },
    /// 準備済み更新を適用
    Apply { id: String },
    /// 適用予約を取消 (現在版は変わらない)
    Cancel { id: String },
    /// 版固定 / 解除
    Pin {
        id: String,
        /// 固定する版 (省略時は現在の最新候補)
        version: Option<String>,
        #[arg(long)]
        clear: bool,
    },
    /// ローカル成果物を取り込み
    Import {
        id: String,
        file: PathBuf,
        #[arg(long)]
        version: String,
    },
    /// 旧版へ復元
    Rollback {
        id: String,
        /// 対象世代 (省略時は現行以外の最新)
        generation: Option<String>,
    },
    /// 世代一覧
    Generations { id: String },
    /// 外部パッケージマネージャへ更新を委譲 (委譲エントリ専用: uv tool upgrade / npm exec キャッシュ更新等)
    PmSync { id: String },
    /// git 作業ツリー+ビルド型の更新元を設定 (リリース資産を持たないソース型ツール向け)
    SetGitSource {
        id: String,
        /// 参照リポジトリ (表示用)
        #[arg(long)]
        repo: String,
        /// 追跡ブランチ (省略時は作業ディレクトリの現在ブランチ。大文字小文字は厳密)
        #[arg(long)]
        branch: Option<String>,
        /// git 作業ディレクトリ (既存クローン。.git 必須)
        #[arg(long = "work-dir")]
        work_dir: PathBuf,
        /// ビルドコマンド (複数回指定。順に実行)
        #[arg(long = "build")]
        build: Vec<String>,
        /// 成果物ディレクトリ (work_dir 相対。これが世代ペイロード)
        #[arg(long)]
        out: String,
    },
    /// Devin 本体の再起動予約。切り離されたヘルパーが全プロセス終了を
    /// 検知して exe を再実行する (本コマンド自体は即復帰)
    RestartDevin {
        /// Devin Next の exe パス (既定: dtm.exe の上位ディレクトリから推定)
        #[arg(long)]
        exe: Option<PathBuf>,
    },
    /// (内部) 指定イメージ名のプロセス消滅を待ち exe を起動するヘルパー
    #[command(hide = true)]
    RelaunchWait {
        /// プロセスイメージ名 (例: "Devin - Next.exe")
        #[arg(long)]
        image: String,
        /// 再起動で実行する exe
        #[arg(long)]
        exe: PathBuf,
    },
    /// イベント履歴
    History { id: String },
    /// 保存世代の整理 (保持数超過分を削除。現行・適用待ち世代は保護)
    Cleanup {
        /// 対象 (省略時は全管理対象)
        id: Option<String>,
        /// 保持世代数 (省略時はレシピの keep_generations)
        #[arg(long)]
        keep: Option<u32>,
        /// 削除せず対象列挙のみ
        #[arg(long)]
        dry_run: bool,
    },
    /// 診断情報を zip にまとめる (問題報告用。設定内の認証情報はマスク)
    Diagnose {
        /// 出力先 (省略時は <root>/state/diagnostics/dtm-diagnose-<ts>.zip)
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// 配置プレビュー (レシピ試行)
    Preview {
        id: String,
        /// ZIP or フォルダ or EXE
        source: PathBuf,
    },
    /// 更新元リポジトリを設定
    SetSource {
        id: String,
        /// "owner/repo" または GitHub URL
        repository: String,
        /// アセット選択パターン (glob)
        #[arg(long)]
        asset: Option<String>,
    },
    /// npm レジストリ取得元を設定 (tarball 取得・世代管理。
    /// PM 委譲の set-source/pkg: とは別経路)
    SetNpmSource {
        id: String,
        /// npm パッケージ名 ("rea-agents", "@scope/name")
        package: String,
        /// レジストリ URL (省略時 https://registry.npmjs.org)
        #[arg(long)]
        registry: Option<String>,
        /// 世代化後に世代ルートで実行するインストールコマンド
        /// (繰り返し可。例: --install "npm install --omit=dev")
        #[arg(long = "install")]
        install_commands: Vec<String>,
    },
    /// レシピを JSON で保存/表示
    Recipe {
        id: String,
        #[command(subcommand)]
        action: RecipeCmd,
    },
    /// ランチャー: 確定世代のツールを透過起動
    Launch {
        /// installationId | name | mcp:<serverName>
        target: String,
        /// ツールへ渡す引数 ("--" 以降)
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// 起動ファイル実パスのみ解決 (診断)
    Resolve { target: String },
    /// 管理ログの場所を表示
    Logs {
        #[arg(long)]
        tail: Option<usize>,
    },
    /// GitHub トークンを OS 資格情報ストアへ保存
    Auth {
        #[command(subcommand)]
        action: AuthCmd,
    },
}

#[derive(Subcommand)]
enum RecipeCmd {
    Show,
    /// ファイルからレシピを取り込み
    Set {
        file: PathBuf,
    },
    /// レシピファイルを既定名でエクスポート
    Export,
}

#[derive(Subcommand)]
enum AuthCmd {
    Github {
        /// 省略時は環境変数 DTM_GITHUB_TOKEN/GITHUB_TOKEN から取る
        #[arg(long)]
        token: Option<String>,
        /// 確認のみ (設定されているか)
        #[arg(long)]
        show: bool,
    },
}

fn main() {
    let cli = Cli::parse();
    // launch 以外は初期化+ログ設定
    let code = run(cli);
    std::process::exit(code);
}

/// デプロイ時にリネーム退避された旧バイナリ残骸 (`dtm.exe.old-*`) を回収する。
/// 使用中のファイルは削除に失敗するだけで問題ない (ベストエフォート)。
fn cleanup_old_exe() {
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(_) => return,
    };
    let Some(dir) = exe.parent() else { return };
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        if e.file_name().to_string_lossy().starts_with("dtm.exe.old") {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

fn run(cli: Cli) -> i32 {
    let layout = match ops::root_layout(cli.root.as_deref()) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[dtm] ルート解決失敗: {e}");
            return 2;
        }
    };
    dtm_core::log::init(&layout);
    cleanup_old_exe();

    // `launch` は別経路 (stdout 非汚染が必須)
    if let Cmd::Launch { target, args } = &cli.cmd {
        return match Store::open(layout) {
            Ok(store) => match dtm_core::launcher::launch(&store, target, args) {
                Ok(status) => status.code().unwrap_or(1),
                Err(e) => {
                    // MCP経路では stderr もプロトコル外表示の可能性があるため
                    // 最小限に留め、詳細はログへ (起動失敗は明示する必要がある)
                    dtm_core::dloge!("launch 失敗 {target}: {e}");
                    eprintln!("[dtm-launcher] {e}");
                    125
                }
            },
            Err(e) => {
                eprintln!("[dtm-launcher] store open: {e}");
                125
            }
        };
    }

    let store = match Store::open(layout) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[dtm] ストア初期化失敗: {e}");
            return 2;
        }
    };

    let json = cli.json;
    match dispatch(&store, cli.cmd, json) {
        Ok(()) => 0,
        Err(e) => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({"ok": false, "error": e.to_string()})
                );
            } else {
                eprintln!("エラー: {e:#}");
            }
            1
        }
    }
}

fn dispatch(store: &Store, cmd: Cmd, json: bool) -> Result<()> {
    match cmd {
        Cmd::Init => {
            store.layout.ensure()?;
            std::fs::write(store.layout.root.join("dtm.root"), b"dtm root marker\n")?;

            // DTM 自身を CLI ツール管理下に登録 (冪等)。
            // 既登録なら更新元の欠落のみ補完し、それ以外は変更しない。
            const SELF_REPO: &str = "matsuzaki-hk/Devin-Tool-Manager";
            const SELF_ASSET: &str = "dtm-*-win-x64.zip";
            let set_self_provider = |iid: &str| -> Result<()> {
                store.update_registry(|reg| {
                    let i = reg
                        .installations
                        .iter_mut()
                        .find(|i| i.id == iid)
                        .ok_or_else(|| dtm_core::Error::NotFound(iid.into()))?;
                    i.recipe.update_provider =
                        Some(UpdateProvider::GithubRelease(GithubReleaseProvider {
                            repository: SELF_REPO.into(),
                            channel: ReleaseChannel::Stable,
                            asset: AssetSelector {
                                match_type: AssetMatchType::Glob,
                                pattern: SELF_ASSET.into(),
                                tag: None,
                                platform: Some("windows".into()),
                                architecture: Some("x64".into()),
                            },
                            version_scheme: VersionScheme::Semver,
                        }));
                    Ok(())
                })?;
                Ok(())
            };

            // 同一 install_root+env 由来の id で既登録判定する
            // (別ツールを同名 "dtm" で登録していても誤認しない)。
            let exe = std::env::current_exe()?;
            let exe_dir = exe.parent().map(|p| p.to_path_buf()).unwrap_or_default();
            let env = dtm_core::id::current_environment();
            let self_iid = dtm_core::id::installation_id(&exe_dir, &env);
            let self_note = match store.find_installation(&self_iid) {
                Ok(inst) if inst.recipe.update_provider.is_some() => "登録済み".to_string(),
                Ok(inst) => {
                    set_self_provider(&inst.id)?;
                    "更新元を補完".to_string()
                }
                Err(dtm_core::Error::NotFound(_)) => {
                    let (inst, _, _) = register::register_cli_tool(
                        store,
                        "dtm",
                        &exe.to_string_lossy(),
                        Some(format!("https://github.com/{SELF_REPO}")),
                    )?;
                    set_self_provider(&inst.id)?;
                    let _ = ops::detect_current_version(store, &inst);
                    "登録".to_string()
                }
                Err(e) => return Err(e.into()),
            };
            dtm_core::dlog!("init: DTM 自己管理 = {} ({})", self_note, self_iid);

            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "ok": true,
                        "root": store.layout.root,
                        "selfRegistered": self_note,
                    })
                );
            } else {
                println!(
                    "初期化完了: {} / DTM自己管理: {}",
                    store.layout.root.display(),
                    self_note
                );
            }
        }
        Cmd::Scan {
            project,
            diagnostics,
        } => {
            let r = register::scan(&project)?;
            if json {
                let v: Vec<_> = r
                    .servers
                    .iter()
                    .map(|s| {
                        serde_json::json!({
                            "name": s.name, "scope": s.scope,
                            "kind": format!("{:?}", s.kind),
                            "command": s.command_summary,
                            "resolved": s.resolved_path,
                            "configPath": s.config_path,
                            "packageManager": s.package_manager,
                            "manageable": s.manageable,
                            "manageNote": s.manage_note,
                            "disabled": s.disabled,
                            "registry": s.registry,
                        })
                    })
                    .collect();
                let output = if diagnostics {
                    serde_json::json!({
                        "servers": v,
                        "errors": r.errors.iter().map(|(path, message)| serde_json::json!({
                            "configPath": path, "message": message,
                        })).collect::<Vec<_>>(),
                    })
                } else {
                    serde_json::json!(v)
                };
                println!("{}", serde_json::to_string_pretty(&output)?);
            } else {
                for (path, message) in &r.errors {
                    eprintln!("設定読み取り失敗: {}: {message}", path.display());
                }
                println!("設定ファイル:");
                for (scope, p) in &r.config_files {
                    println!("  [{scope}] {}", p.display());
                }
                println!("\nサーバー {} 件:", r.servers.len());
                for s in &r.servers {
                    let kind = match s.kind {
                        ToolKind::McpStdio => "stdio",
                        ToolKind::McpRemote => "remote",
                        ToolKind::CliTool => "cli",
                    };
                    let pm = s
                        .package_manager
                        .as_ref()
                        .map(|m| format!(" [pkg:{m}]"))
                        .unwrap_or_default();
                    let dis = if s.disabled { " (無効)" } else { "" };
                    let rp = s
                        .resolved_path
                        .as_ref()
                        .map(|p| format!(" -> {}", p.display()))
                        .unwrap_or_default();
                    println!(
                        "  {:24} {:6} {}{}{}{}",
                        s.name, kind, s.command_summary, pm, dis, rp
                    );
                }
            }
        }
        Cmd::List => {
            let reg = store.load_registry()?;
            let mut rows = vec![];
            for i in &reg.installations {
                let mut st = store.load_state(&i.id).unwrap_or_default();
                // 中断したジョブの残留表示を回復 (実行者不在をロックで判定)
                if dtm_core::lock::heal_interrupted(&store.layout, &i.id, &mut st) {
                    let _ = store.save_state(&i.id, &st);
                }
                let status = st
                    .status
                    .map(|s| s.display(&st))
                    .unwrap_or_else(|| "未確認".into());
                // 実行中ジョブの進捗 (running 時のみ。残留ファイルは返さない)
                let progress = dtm_core::lock::active_progress(&store.layout, &i.id);
                let strategy = i
                    .recipe
                    .installation
                    .as_ref()
                    .map(|s| format!("{:?}", s.strategy))
                    .unwrap_or_default();
                rows.push(serde_json::json!({
                    "id": i.id, "name": i.name,
                    "kind": format!("{:?}", i.kind),
                    "status": status,
                    "current": st.current_version,
                    "prepared": st.prepared_version,
                    "candidate": st.latest_version().map(String::from),
                    "lastChecked": st.last_checked,
                    "lastError": st.last_error,
                    "progress": progress,
                    "pendingPmSync": st.pending_pm_sync,
                    "installRoot": i.install_root,
                    "fixedPath": i.fixed_path,
                    "strategy": strategy,
                    "pinned": i.recipe.policy.as_ref().and_then(|p| p.pinned_version.clone()),
                    // 更新元は update_provider 側を優先表示 (set-source が書く先)。
                    // source_repository は登録時の出自情報で、更新元とは別フィールド。
                    "sourceRepo": i.recipe.update_provider.as_ref().and_then(|p| match p {
                        dtm_core::models::UpdateProvider::GithubRelease(g) => Some(g.repository.clone()),
                        dtm_core::models::UpdateProvider::GitBuild(g) => Some(g.repository.clone()),
                        dtm_core::models::UpdateProvider::NpmRegistry(n) => Some(format!("npm:{}", n.package)),
                        _ => None,
                    }).or_else(|| i.recipe.source_repository.clone()),
                    // set-source フォームの初期値用に現在のアセット条件も返す
                    "sourceAsset": i.recipe.update_provider.as_ref().and_then(|p| match p {
                        dtm_core::models::UpdateProvider::GithubRelease(g) => Some(g.asset.pattern.clone()),
                        _ => None,
                    }),
                    // ビルド元設定フォームの初期値用
                    "gitSource": i.recipe.update_provider.as_ref().and_then(|p| match p {
                        dtm_core::models::UpdateProvider::GitBuild(g) => Some(serde_json::json!({
                            "repo": g.repository,
                            "branch": g.branch,
                            "workDir": g.work_dir,
                            "build": g.build_commands.join("; "),
                            "out": g.output_dir,
                        })),
                        _ => None,
                    }),
                    // npm レジストリ取得元フォームの初期値用
                    "npmSource": i.recipe.update_provider.as_ref().and_then(|p| match p {
                        dtm_core::models::UpdateProvider::NpmRegistry(n) => Some(serde_json::json!({
                            "package": n.package,
                            "registry": n.registry,
                            "install": n.install_commands.join("; "),
                        })),
                        _ => None,
                    }),
                    "provider": match &i.recipe.update_provider {
                        Some(dtm_core::models::UpdateProvider::PackageManager { manager, package }) =>
                            Some(format!("pkg:{manager}:{package}")),
                        Some(dtm_core::models::UpdateProvider::GithubRelease(p)) =>
                            Some(format!("github:{}", p.repository)),
                        Some(dtm_core::models::UpdateProvider::GitBuild(p)) =>
                            Some(format!("gitbuild:{}@{}", p.repository, p.branch)),
                        Some(dtm_core::models::UpdateProvider::NpmRegistry(n)) =>
                            Some(format!("npm:{}", n.package)),
                        Some(dtm_core::models::UpdateProvider::Manual) => Some("manual".into()),
                        None => None,
                    },
                    "usages": i.usages.iter().filter_map(|u| match u {
                        dtm_core::models::UsageRef::McpServer { config_path, server_name, .. } => Some(serde_json::json!({
                            "configPath": config_path, "serverName": server_name,
                        })),
                        _ => None,
                    }).collect::<Vec<_>>(),
                }));
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else {
                if rows.is_empty() {
                    println!(
                        "管理対象なし。`dtm scan` → `dtm register-mcp` / `dtm add-cli` で登録"
                    );
                }
                for r in &rows {
                    println!(
                        "{:16} {:10} {:30} {} -> {} (候補: {})",
                        r["name"].as_str().unwrap_or(""),
                        r["kind"].as_str().unwrap_or(""),
                        r["status"].as_str().unwrap_or(""),
                        r["current"].as_str().unwrap_or("-"),
                        r["prepared"].as_str().unwrap_or("-"),
                        r["candidate"].as_str().unwrap_or("-"),
                    );
                    println!("  id={} root={}", r["id"], r["installRoot"]);
                }
            }
        }
        Cmd::Status { id } => {
            let inst = store.find_installation(&id)?;
            let mut st = store.load_state(&inst.id)?;
            // 残留した進行中ステータスを中断として回復してから表示
            if dtm_core::lock::heal_interrupted(&store.layout, &inst.id, &mut st) {
                let _ = store.save_state(&inst.id, &st);
            }
            let job = dtm_core::lock::peek_job(&store.layout, &inst.id);
            let progress = dtm_core::lock::active_progress(&store.layout, &inst.id);
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "installation": inst, "state": st, "job": job,
                        "progress": progress,
                    }))?
                );
            } else {
                println!(
                    "{} ({})\n  種別: {:?}  環境: {}",
                    inst.name, inst.id, inst.kind, inst.environment
                );
                println!("  配置: {}", inst.install_root.display());
                if let Some(fp) = &inst.fixed_path {
                    println!("  固定パス: {}", fp.display());
                }
                let status = st
                    .status
                    .map(|s| s.display(&st))
                    .unwrap_or_else(|| "未確認".into());
                println!("  状態: {status}");
                println!(
                    "  現在版: {} / 準備: {} / 候補: {}",
                    st.current_version.as_deref().unwrap_or("-"),
                    st.prepared_version.as_deref().unwrap_or("-"),
                    st.latest_version().unwrap_or("-")
                );
                if let Some(rv) = &st.running_version {
                    println!("  実行中: {rv}");
                }
                if let Some(e) = &st.last_error {
                    println!("  最終エラー: {e}");
                }
                if let Some(j) = job {
                    println!("  ジョブ: {} {} ({:?})", j.kind, j.status, j.started_at);
                }
                if let Some(p) = &progress {
                    let total = p
                        .bytes_total
                        .map(|t| format!("{:.1}MB", t as f64 / 1_048_576.0))
                        .unwrap_or_else(|| "不明".into());
                    println!(
                        "  進捗: {} {:.1}MB / {}",
                        p.phase,
                        p.bytes_done as f64 / 1_048_576.0,
                        total
                    );
                }
            }
        }
        Cmd::AddCli {
            name,
            specifier,
            source,
            strategy,
        } => {
            let (inst, created, exe) =
                register::register_cli_tool(store, &name, &specifier, source)?;
            if let Some(s) = &strategy {
                let kind = match s.as_str() {
                    "versioned" => StrategyKind::Versioned,
                    "fixed-path" | "fixed" => StrategyKind::FixedPath,
                    other => bail!("未知の strategy: {other} (versioned|fixed-path)"),
                };
                store.update_registry(|reg| {
                    let i = reg
                        .installations
                        .iter_mut()
                        .find(|i| i.id == inst.id)
                        .ok_or_else(|| dtm_core::Error::NotFound(inst.id.clone()))?;
                    if let Some(st) = i.recipe.installation.as_mut() {
                        st.strategy = kind;
                    }
                    Ok(())
                })?;
            }
            let inst = store.find_installation(&inst.id)?;
            // versioned 方式は既存配置を世代0として取り込む
            if matches!(
                inst.recipe.installation.as_ref().map(|s| s.strategy),
                Some(StrategyKind::Versioned)
            ) {
                if let Some(g) = ops::seed_initial_generation(store, &inst)? {
                    print_simple(json, &format!("初期世代: {g}"))?;
                }
            }
            let _ = ops::detect_current_version(store, &inst);
            print_simple(
                json,
                &format!(
                    "{}: {} -> {} ({})",
                    if created {
                        "登録"
                    } else {
                        "既存に関連付け"
                    },
                    inst.name,
                    exe.display(),
                    inst.id
                ),
            )?;
        }
        Cmd::AddRtk { dir } => {
            let (inst, created) = register::register_rtk(store, &dir)?;
            let _ = ops::detect_current_version(store, &inst);
            print_simple(
                json,
                &format!(
                    "RTK {}: {} ({})",
                    if created { "登録" } else { "既存" },
                    inst.install_root.display(),
                    inst.id
                ),
            )?;
        }
        Cmd::SetMcpEnabled {
            server_name,
            config,
            enabled,
            expected_disabled,
        } => {
            let cfg_path = config.map(Ok).unwrap_or_else(discover::user_config_path)?;
            mcpcfg::set_enabled(store, &cfg_path, &server_name, enabled, expected_disabled)?;
            print_simple(
                json,
                &format!(
                    "MCP設定を保存: {server_name} → {}（接続反映はDevin側で確認してください）",
                    if enabled { "有効" } else { "無効" }
                ),
            )?;
        }
        Cmd::RegisterMcp {
            server_name,
            source,
            config,
        } => {
            let cfg_path = config.map(Ok).unwrap_or_else(discover::user_config_path)?;
            let cfg = discover::read_config(discover::ConfigScope::User, &cfg_path)?;
            let entry = discover::list_servers(&cfg)
                .into_iter()
                .find(|e| e.name == server_name)
                .with_context(|| format!("サーバー '{server_name}' が設定にありません"))?;
            let (inst, created) = register::register_mcp_server(store, &entry, source)?;
            let _ = ops::detect_current_version(store, &inst);
            print_simple(
                json,
                &format!(
                    "{}: {} ({})",
                    if created {
                        "登録"
                    } else {
                        "既存に関連付け"
                    },
                    inst.name,
                    inst.id
                ),
            )?;
        }
        Cmd::Manage {
            server_name,
            config,
        } => {
            let cfg_path = config.map(Ok).unwrap_or_else(discover::user_config_path)?;
            let cfg = discover::read_config(discover::ConfigScope::User, &cfg_path)?;
            let entry = discover::list_servers(&cfg)
                .into_iter()
                .find(|e| e.name == server_name)
                .with_context(|| format!("サーバー '{server_name}' が設定にありません"))?;
            let original = entry
                .launch
                .clone()
                .with_context(|| "リモートサーバーは管理化できません")?;
            // ランチャーが扱えない起動形式は設定変更前に拒否 (R5)
            register::ensure_manageable(&entry)?;
            let (inst, _) = register::register_mcp_server(store, &entry, None)?;
            if matches!(
                inst.recipe.installation.as_ref().map(|s| s.strategy),
                Some(StrategyKind::Versioned)
            ) {
                ops::seed_initial_generation(store, &inst)?;
            }
            mcpcfg::manage(store, &inst, &cfg, &server_name, &original)?;
            print_simple(
                json,
                &format!(
                    "管理化完了: {server_name} → {}",
                    mcpcfg::launcher_exe_path(&store.layout).display()
                ),
            )?;
        }
        Cmd::Unmanage {
            server_name,
            config,
            force,
        } => {
            let cfg_path = config.map(Ok).unwrap_or_else(discover::user_config_path)?;
            let orig = mcpcfg::unmanage(store, &cfg_path, &server_name, !force)?;
            print_simple(
                json,
                &format!(
                    "管理解除: {server_name} → {} {}",
                    orig.command,
                    orig.args.join(" ")
                ),
            )?;
        }
        Cmd::Managed => {
            let map = store.load_managed_map()?;
            if json {
                let v: Vec<_> = map
                    .entries
                    .iter()
                    .map(|e| {
                        serde_json::json!({
                            "usageKey": e.usage_key,
                            "installationId": e.installation_id,
                            "configPath": e.config_path,
                            "serverName": e.server_name,
                            "managedAt": e.managed_at,
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else {
                for e in &map.entries {
                    println!(
                        "  {} {} -> {} ({})",
                        e.usage_key,
                        e.server_name,
                        e.installation_id,
                        e.config_path.display()
                    );
                }
                if map.entries.is_empty() {
                    println!("管理化済み MCP はありません");
                }
            }
        }
        Cmd::ProbeMcp {
            server_name,
            config,
            timeout,
            no_cache,
        } => {
            let cfg_path = config.map(Ok).unwrap_or_else(discover::user_config_path)?;
            let o = probe::probe_server(store, &cfg_path, &server_name, timeout, no_cache)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&o)?);
            } else {
                let tools = o.tools.map(|t| t.to_string()).unwrap_or_else(|| "-".into());
                println!(
                    "{}: tools={} ver={}{}",
                    o.server_name,
                    tools,
                    o.server_version.as_deref().unwrap_or("-"),
                    o.error
                        .as_ref()
                        .map(|e| format!("  error={e}"))
                        .unwrap_or_default()
                );
            }
        }
        Cmd::ProbeCache => {
            let cache = probe::load_cache(&store.layout);
            println!("{}", serde_json::to_string_pretty(&cache)?);
        }
        Cmd::Remove { id, force } => {
            let inst = store.find_installation(&id)?;
            let o = ops::remove(store, &inst, force)?;
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "ok": true,
                        "message": format!("登録解除: {}", o.name),
                        "removed": o.name,
                        "removedManagedEntries": o.removed_managed_entries,
                        "warnings": o.warnings,
                    })
                );
            } else {
                println!("登録解除: {}", o.name);
            }
            for w in o.warnings {
                eprintln!("警告: {w}");
            }
        }
        Cmd::Check { id, all } => {
            let targets = pick_targets(store, id, all)?;
            for inst in &targets {
                match ops::check(store, inst) {
                    Ok(o) => {
                        let c = o
                            .candidate
                            .map(|c| format!("{} ({})", c.version, c.tag))
                            .unwrap_or_else(|| "-".into());
                        print_simple(
                            json,
                            &format!(
                                "{}: {:?} 候補={c}{}",
                                inst.name,
                                o.status,
                                if o.reused {
                                    " [既ジョブ結果]"
                                } else {
                                    ""
                                }
                            ),
                        )?;
                    }
                    Err(e) => {
                        print_simple(json, &format!("{}: 確認失敗 {e}", inst.name))?;
                    }
                }
            }
        }
        Cmd::Prepare { id } => {
            let inst = store.find_installation(&id)?;
            let o = ops::prepare(store, &inst)?;
            print_simple(
                json,
                &format!(
                    "準備完了: {} {} (世代={}, 検証={}, 版確認={:?})",
                    inst.name, o.version, o.generation_id, o.verification.status, o.probed_version
                ),
            )?;
        }
        Cmd::Apply { id } => {
            let inst = store.find_installation(&id)?;
            ops::apply(store, &inst)?;
            let st = store.load_state(&inst.id)?;
            print_simple(
                json,
                &format!(
                    "適用完了: {} → {}",
                    inst.name,
                    st.current_version.as_deref().unwrap_or("?")
                ),
            )?;
        }
        Cmd::Cancel { id } => {
            let inst = store.find_installation(&id)?;
            ops::cancel_prepared(store, &inst)?;
            print_simple(json, &format!("予約取消: {}", inst.name))?;
        }
        Cmd::Pin { id, version, clear } => {
            let inst = store.find_installation(&id)?;
            let v = if clear {
                None
            } else {
                version.or_else(|| {
                    store
                        .load_state(&inst.id)
                        .ok()
                        .and_then(|s| s.latest_version().map(String::from))
                })
            };
            ops::pin(store, &inst, v.clone())?;
            print_simple(
                json,
                &match v {
                    Some(v) => format!("版固定: {} → {v}", inst.name),
                    None => format!("版固定解除: {}", inst.name),
                },
            )?;
        }
        Cmd::Import { id, file, version } => {
            let inst = store.find_installation(&id)?;
            let o = ops::import(store, &inst, &file, &version)?;
            print_simple(
                json,
                &format!(
                    "取込完了: {} {} (世代={})",
                    inst.name, o.version, o.generation_id
                ),
            )?;
        }
        Cmd::Rollback { id, generation } => {
            let inst = store.find_installation(&id)?;
            let v = dtm_core::apply::rollback(store, &inst, generation.as_deref())?;
            print_simple(json, &format!("復元完了: {} → {v}", inst.name))?;
        }
        Cmd::PmSync { id } => {
            let desc = dtm_core::ops::pm_sync(store, &id)?;
            print_simple(json, &format!("外部更新完了: {desc}"))?;
        }
        Cmd::RestartDevin { exe } => {
            let exe = exe
                .or_else(default_devin_exe)
                .filter(|p| p.exists())
                .ok_or_else(|| {
                    dtm_core::Error::Config(
                        "Devin exe が見つかりません。--exe で指定してください".into(),
                    )
                })?;
            let image = exe
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "Devin - Next.exe".into());
            spawn_relaunch_helper(&image, &exe)?;
            print_simple(
                json,
                "再起動ヘルパー起動 — Devin 全プロセス終了後に再実行します",
            )?;
        }
        Cmd::RelaunchWait { image, exe } => {
            relaunch_wait(&image, &exe)?;
        }
        Cmd::Generations { id } => {
            let inst = store.find_installation(&id)?;
            let gens = dtm_core::apply::list_generations(store, &inst.id)?;
            let active = store.active_generation(&inst.id)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&gens)?);
            } else {
                for g in gens {
                    let mark = if Some(&g.generation_id) == active.as_ref() {
                        "*"
                    } else {
                        " "
                    };
                    println!(
                        "{mark} {:40} {} [{}]",
                        g.generation_id, g.version, g.verification.status
                    );
                }
            }
        }
        Cmd::History { id } => {
            let inst = store.find_installation(&id)?;
            let st = store.load_state(&inst.id)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&st.events)?);
            } else {
                for e in &st.events {
                    println!(
                        "{} {:16} {}",
                        e.at.format("%Y-%m-%d %H:%M:%S"),
                        e.kind,
                        e.detail
                    );
                }
            }
        }
        Cmd::Cleanup { id, keep, dry_run } => {
            let targets = pick_targets(store, id.clone(), id.is_none())?;
            let mut results = vec![];
            for inst in &targets {
                match ops::cleanup_generations(store, inst, keep, dry_run) {
                    Ok(removed) => {
                        let freed: u64 = removed.iter().map(|(_, b)| b).sum();
                        results.push(serde_json::json!({
                            "id": inst.id, "name": inst.name,
                            "removed": removed.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>(),
                            "freedBytes": freed,
                        }));
                    }
                    Err(e) => results.push(serde_json::json!({
                        "id": inst.id, "name": inst.name,
                        "skipped": e.to_string(),
                    })),
                }
            }
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "dryRun": dry_run, "results": results,
                    }))?
                );
            } else {
                for r in &results {
                    let removed = r["removed"].as_array().map(|a| a.len()).unwrap_or(0);
                    let mb = r["freedBytes"].as_u64().unwrap_or(0) as f64 / 1_048_576.0;
                    if let Some(s) = r["skipped"].as_str() {
                        println!("{}: スキップ ({s})", r["name"].as_str().unwrap_or(""));
                    } else {
                        println!(
                            "{}: {} 件{} ({:.1} MB)",
                            r["name"].as_str().unwrap_or(""),
                            removed,
                            if dry_run {
                                " が削除対象"
                            } else {
                                " 削除"
                            },
                            mb
                        );
                    }
                }
            }
        }
        Cmd::Diagnose { out } => {
            let zip = ops::diagnose(store, out.as_deref())?;
            if json {
                let bytes = std::fs::metadata(&zip).map(|m| m.len()).unwrap_or(0);
                println!(
                    "{}",
                    serde_json::json!({
                        "ok": true, "path": zip, "bytes": bytes,
                        "message": format!("診断バンドルを保存しました: {}", zip.display()),
                    })
                );
            } else {
                println!("診断バンドル: {}", zip.display());
            }
        }
        Cmd::Preview { id, source } => {
            let inst = store.find_installation(&id)?;
            let plan = if source.is_dir() {
                stage::plan_placement(&inst.recipe, &source, None)?
            } else if dtm_core::zipx::is_zip(&source) {
                let tmp = std::env::temp_dir().join(format!("dtm-preview-{}", std::process::id()));
                if tmp.exists() {
                    std::fs::remove_dir_all(&tmp)?;
                }
                let files =
                    dtm_core::zipx::extract(&source, &tmp, &dtm_core::zipx::ZipLimits::default())?;
                let p = stage::plan_placement(&inst.recipe, &tmp, None);
                let _ = std::fs::remove_dir_all(&tmp);
                let _ = files;
                p?
            } else {
                let dir = source.parent().unwrap_or_else(|| std::path::Path::new("."));
                let name = source
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                stage::plan_placement(&inst.recipe, dir, Some(&name))?
            };
            if json {
                let files: Vec<_> = plan.files.iter().map(|f| f.rel.clone()).collect();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "entrypoint": plan.entrypoint, "files": files,
                        "preserved": plan.preserved, "excluded": plan.excluded,
                    }))?
                );
            } else {
                println!("配置プレビュー (entrypoint: {}):", plan.entrypoint);
                for f in &plan.files {
                    println!("  + {} ({} bytes)", f.rel, f.size);
                }
                for p in &plan.preserved {
                    println!("  = {p} [保持]");
                }
                for e in &plan.excluded {
                    println!("  - {e} [除外]");
                }
            }
        }
        Cmd::SetSource {
            id,
            repository,
            asset,
        } => {
            let inst = store.find_installation(&id)?;
            let repo = dtm_core::github::GithubClient::normalize_repo(&repository);
            store.update_registry(|reg| {
                let i = reg
                    .installations
                    .iter_mut()
                    .find(|i| i.id == inst.id)
                    .ok_or_else(|| dtm_core::Error::NotFound(inst.id.clone()))?;
                i.recipe.update_provider =
                    Some(UpdateProvider::GithubRelease(GithubReleaseProvider {
                        repository: repo.clone(),
                        channel: ReleaseChannel::Stable,
                        asset: AssetSelector {
                            match_type: AssetMatchType::Glob,
                            pattern: asset.clone().unwrap_or_else(|| "*.zip".into()),
                            tag: None,
                            platform: Some("windows".into()),
                            architecture: Some("x64".into()),
                        },
                        version_scheme: VersionScheme::Semver,
                    }));
                Ok(())
            })?;
            dtm_core::dlog!(
                "set-source: {} → {} (asset: {})",
                inst.name,
                repo,
                asset.as_deref().unwrap_or("*.zip")
            );
            print_simple(json, &format!("更新元設定: {} → {repo}", inst.name))?;
        }
        Cmd::SetNpmSource {
            id,
            package,
            registry,
            install_commands,
        } => {
            let inst = store.find_installation(&id)?;
            store.update_registry(|reg| {
                let i = reg
                    .installations
                    .iter_mut()
                    .find(|i| i.id == inst.id)
                    .ok_or_else(|| dtm_core::Error::NotFound(inst.id.clone()))?;
                i.recipe.update_provider = Some(UpdateProvider::NpmRegistry(NpmRegistryProvider {
                    package: package.clone(),
                    registry: registry.clone(),
                    install_commands: install_commands.clone(),
                    version_scheme: VersionScheme::Semver,
                }));
                // npm tarball は tar.gz — artifact.kind が他形式のままだと
                // prepare で形式不一致になるため tar-gz に揃える
                if let Some(a) = i.recipe.artifact.as_mut() {
                    if a.kind != ArtifactKind::TarGz {
                        a.kind = ArtifactKind::TarGz;
                    }
                }
                Ok(())
            })?;
            dtm_core::dlog!(
                "set-npm-source: {} → {} (registry: {})",
                inst.name,
                package,
                registry.as_deref().unwrap_or("https://registry.npmjs.org")
            );
            print_simple(json, &format!("npm 取得元設定: {} → {package}", inst.name))?;
        }
        Cmd::SetGitSource {
            id,
            repo,
            branch,
            work_dir,
            build,
            out,
        } => {
            let inst = store.find_installation(&id)?;
            if !work_dir.join(".git").exists() {
                bail!("git 作業ディレクトリではありません: {}", work_dir.display());
            }
            // ブランチ省略時は作業ツリーの現在ブランチを採用
            let branch = branch
                .or_else(|| dtm_core::ops::git_current_branch(&work_dir))
                .unwrap_or_else(|| "main".into());
            // origin にブランチが存在するか検証 (大文字小文字の誤り等を早期発見)。
            // ls-remote が失敗するオフライン時は検証をスキップする。
            match dtm_core::ops::git_remote_branches(&work_dir) {
                Ok(branches) if !branches.iter().any(|b| b == &branch) => {
                    bail!(
                        "ブランチ '{}' が origin に見つかりません (大文字小文字は厳密)。候補: {}",
                        branch,
                        branches.join(", ")
                    );
                }
                Ok(_) => {}
                Err(e) => {
                    dtm_core::dlogw!("set-git-source: ブランチ検証をスキップ ({e})");
                }
            }
            store.update_registry(|reg| {
                let i = reg
                    .installations
                    .iter_mut()
                    .find(|i| i.id == inst.id)
                    .ok_or_else(|| dtm_core::Error::NotFound(inst.id.clone()))?;
                i.recipe.update_provider = Some(UpdateProvider::GitBuild(GitBuildProvider {
                    repository: repo.clone(),
                    branch: branch.clone(),
                    work_dir: work_dir.clone(),
                    build_commands: build.clone(),
                    output_dir: Some(out.clone()),
                }));
                Ok(())
            })?;
            dtm_core::dlog!(
                "set-git-source: {} → {} ({} / {} build, out: {})",
                inst.name,
                repo,
                work_dir.display(),
                build.len(),
                out
            );
            print_simple(
                json,
                &format!(
                    "ビルド元設定: {} → {} ({} @ {})",
                    inst.name,
                    repo,
                    work_dir.display(),
                    branch
                ),
            )?;
        }
        Cmd::Recipe { id, action } => {
            let inst = store.find_installation(&id)?;
            match action {
                RecipeCmd::Show => {
                    println!("{}", serde_json::to_string_pretty(&inst.recipe)?);
                }
                RecipeCmd::Set { file } => {
                    let recipe: Recipe = paths::read_json(&file)?;
                    if recipe.installation_id != inst.id {
                        bail!("レシピの installationId が一致しません");
                    }
                    store.update_registry(|reg| {
                        let i = reg
                            .installations
                            .iter_mut()
                            .find(|i| i.id == inst.id)
                            .ok_or_else(|| dtm_core::Error::NotFound(inst.id.clone()))?;
                        i.recipe = recipe;
                        Ok(())
                    })?;
                    print_simple(json, &format!("レシピ更新: {}", inst.name))?;
                }
                RecipeCmd::Export => {
                    let p = store.layout.recipe_file(&inst.recipe.id);
                    paths::atomic_write_json(&p, &inst.recipe)?;
                    println!("{}", p.display());
                }
            }
        }
        Cmd::Resolve { target } => {
            let p = dtm_core::launcher::resolve_path_only(store, &target)?;
            println!("{}", p.display());
        }
        Cmd::Logs { tail } => {
            let f = store.layout.log_file();
            let mut lines: Vec<String> = vec![];
            if f.exists() {
                if let Ok(content) = std::fs::read_to_string(&f) {
                    let all: Vec<String> = content.lines().map(String::from).collect();
                    lines = match tail {
                        Some(n) => all
                            .iter()
                            .skip(all.len().saturating_sub(n))
                            .cloned()
                            .collect(),
                        None => all,
                    };
                }
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&lines)?);
            } else {
                println!("{}", f.display());
                for l in &lines {
                    println!("{l}");
                }
            }
        }
        Cmd::Auth { action } => match action {
            AuthCmd::Github { token, show } => {
                if show {
                    let set = dtm_core::credentials::github_token().is_some();
                    println!("GitHub token: {}", if set { "設定済み" } else { "未設定" });
                } else {
                    let tok = token
                        .or_else(|| std::env::var("DTM_GITHUB_TOKEN").ok())
                        .or_else(|| std::env::var("GITHUB_TOKEN").ok())
                        .with_context(|| "トークン未指定 (--token または環境変数)")?;
                    dtm_core::credentials::store_github_token(&tok)?;
                    println!("GitHub トークンを資格情報ストアへ保存しました");
                }
            }
        },
        Cmd::Launch { .. } => unreachable!(),
    }
    Ok(())
}

fn pick_targets(store: &Store, id: Option<String>, all: bool) -> Result<Vec<Installation>> {
    let reg = store.load_registry()?;
    if all {
        return Ok(reg.installations);
    }
    match id {
        Some(id) => Ok(vec![store.find_installation(&id)?]),
        None => bail!("対象を指定してください (id または --all)"),
    }
}

fn print_simple(json: bool, msg: &str) -> Result<()> {
    if json {
        println!("{}", serde_json::json!({"ok": true, "message": msg}));
    } else {
        println!("{msg}")
    }
    Ok(())
}

// ---------- Devin 再起動ヘルパー ----------

/// dtm.exe の配置から Devin Next の exe を推定 (DevinToolManager\dtm.exe の1階層上)
fn default_devin_exe() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()?
        .parent()?
        .parent()
        .map(|d| d.join("Devin - Next.exe"))
}

/// Devin 終了検知→再起動を担う切り離しヘルパーを起動。
/// 拡張側はこの直後に workbench.action.quit を呼ぶ前提で、
/// 本関数はヘルパー起動を確認した時点で即復帰する。
#[cfg(windows)]
fn spawn_relaunch_helper(image: &str, exe: &std::path::Path) -> Result<()> {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x00000008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    let self_exe = std::env::current_exe()?;
    std::process::Command::new(self_exe)
        .args(["relaunch-wait", "--image", image, "--exe"])
        .arg(exe)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW)
        .spawn()
        .with_context(|| "再起動ヘルパー起動失敗")?;
    dtm_core::dlog!(
        "restart-devin: ヘルパー起動 (image={image}, exe={})",
        exe.display()
    );
    Ok(())
}

#[cfg(not(windows))]
fn spawn_relaunch_helper(_image: &str, _exe: &std::path::Path) -> Result<()> {
    anyhow::bail!("この環境では未対応です")
}

/// 指定イメージ名のプロセスが全て消えるまで待ち、exe を再実行する (最大300s)
fn relaunch_wait(image: &str, exe: &std::path::Path) -> Result<()> {
    dtm_core::dlog!(
        "relaunch-wait: '{image}' 終了待機開始 (exe={})",
        exe.display()
    );
    // 1回目は即判定。プロセス列挙は軽量なので 250ms 間隔で終了検知を早める
    for _ in 0..1200 {
        if !image_running(image) {
            dtm_core::dlog!("relaunch-wait: '{image}' 全プロセス終了を検知 → 再起動");
            match launch_detached(exe) {
                Ok(pid) => {
                    dtm_core::dlog!("relaunch-wait: 起動実行 pid={pid}");
                    // Electron アプリは起動に数秒かかる。生存確認して消滅は検知する
                    std::thread::sleep(std::time::Duration::from_secs(5));
                    if image_running(image) {
                        dtm_core::dlog!("relaunch-wait: '{image}' 生存確認 OK");
                    } else {
                        dtm_core::dlogw!(
                            "relaunch-wait: '{image}' が起動直後に消滅 — env継承等の起動失敗の可能性"
                        );
                    }
                }
                Err(e) => dtm_core::dloge!("relaunch-wait: 起動失敗: {e}"),
            }
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    dtm_core::dlogw!("relaunch-wait: タイムアウト — '{image}' が残留しているため再起動中止");
    Ok(())
}

// Windows では Toolhelp32 スナップショットで列挙する。
// tasklist.exe を起動する方法は、コンソールの無いヘルパーからの
// 子プロセス生成で都度コンソールウィンドウが点滅するため使わない。
#[cfg(windows)]
fn image_running(image: &str) -> bool {
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    let snapshot = match snapshot {
        Ok(h) => h,
        Err(_) => return true, // 取得失敗時は残存扱い (上限到達で諦める)
    };
    let needle = image.to_lowercase();
    let mut found = false;
    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    unsafe {
        if Process32FirstW(snapshot, &mut entry).is_ok() {
            loop {
                let len = entry
                    .szExeFile
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(entry.szExeFile.len());
                if String::from_utf16_lossy(&entry.szExeFile[..len]).to_lowercase() == needle {
                    found = true;
                    break;
                }
                if Process32NextW(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = windows::Win32::Foundation::CloseHandle(snapshot);
    }
    found
}

#[cfg(not(windows))]
fn image_running(image: &str) -> bool {
    match std::process::Command::new("tasklist")
        .args(["/FI", &format!("IMAGENAME eq {image}"), "/NH", "/FO", "CSV"])
        .output()
    {
        Ok(o) => String::from_utf8_lossy(&o.stdout)
            .to_lowercase()
            .contains(&image.to_lowercase()),
        Err(_) => true, // 取得失敗時は残存扱い (上限到達で諦める)
    }
}

/// Electron アプリを汚染された環境で起動しないよう ELECTRON_* を除去する。
/// (拡張ホスト由来の ELECTRON_RUN_AS_NODE=1 が伝播すると
///  Devin - Next.exe が Node.js として起動し即終了する)
fn sanitize_electron_env(cmd: &mut std::process::Command) {
    let keys: Vec<std::ffi::OsString> = std::env::vars_os()
        .map(|(k, _)| k)
        .filter(|k| k.to_string_lossy().to_uppercase().starts_with("ELECTRON"))
        .collect();
    for k in keys {
        cmd.env_remove(&k);
    }
}

#[cfg(windows)]
fn launch_detached(exe: &std::path::Path) -> Result<u32> {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x00000008;
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    let mut cmd = std::process::Command::new(exe);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW);
    sanitize_electron_env(&mut cmd);
    let child = cmd
        .spawn()
        .with_context(|| format!("Devin 再起動失敗: {}", exe.display()))?;
    Ok(child.id())
}

#[cfg(not(windows))]
fn launch_detached(exe: &std::path::Path) -> Result<u32> {
    let child = std::process::Command::new(exe).spawn()?;
    Ok(child.id())
}

#[cfg(test)]
mod tests {
    /// Toolhelp32 列挙が実行中プロセスを検出し、存在しない名を検出しないこと
    #[cfg(windows)]
    #[test]
    fn image_running_via_toolhelp() {
        let self_name = std::env::current_exe()
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        assert!(super::image_running(&self_name));
        assert!(!super::image_running("dtm-definitely-not-running-9f3e.exe"));
    }

    /// ELECTRON_RUN_AS_NODE が除去指定されること (Electron 即終了バグの防止)
    #[test]
    fn sanitize_strips_electron_env() {
        std::env::set_var("ELECTRON_RUN_AS_NODE", "1");
        std::env::set_var("ELECTRON_DTM_TEST", "x");
        let mut cmd = std::process::Command::new("cmd");
        super::sanitize_electron_env(&mut cmd);
        let removals: Vec<String> = cmd
            .get_envs()
            .filter(|(_, v)| v.is_none())
            .map(|(k, _)| k.to_string_lossy().to_uppercase())
            .collect();
        assert!(removals.iter().any(|k| k == "ELECTRON_RUN_AS_NODE"));
        assert!(removals.iter().any(|k| k == "ELECTRON_DTM_TEST"));
    }
}

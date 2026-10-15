//! インストール登録 (仕様 §4.2, §10)。
//! - MCP サーバー利用元から installation を登録/関連付け
//! - 一般 CLI ツールの手動登録 (EXE絶対パス / dir+rel / コマンド名)
//! - RTK 初期レシピ

use crate::discover::{self, McpServerEntry};
use crate::error::{Error, Result};
use crate::id;
use crate::models::*;
use crate::paths::normalize_path;
use crate::resolve;
use crate::store::Store;
use chrono::Utc;
use std::path::{Path, PathBuf};

/// MCP サーバーエントリから installation を登録する。
/// command が管理可能な実体 (絶対パスEXE / ローカルスクリプト) を指す場合のみ。
/// パッケージ管理下 (npx/uvx) は adapter 対象として PackageManager で登録。
pub fn register_mcp_server(
    store: &Store,
    entry: &McpServerEntry,
    source_repo: Option<String>,
) -> Result<(Installation, bool)> {
    let launch = entry
        .launch
        .clone()
        .ok_or_else(|| Error::Config("リモートサーバーはファイル管理対象外".into()))?;

    // パッケージ管理下 (npx/uvx 等) は実体が PM 管轄のため PATH 解決不要。
    // 複数候補や未インストールでも登録できるよう、先に PM 判定する。
    let resolved = match resolve::resolve_command(&launch) {
        Ok(r) => r,
        Err(e) => match resolve::detect_manager(&launch.command, &launch.args) {
            Some(pm) => resolve::ResolvedCommand {
                path: None,
                via: "unresolved".into(),
                candidates: vec![],
                package_manager: Some(pm),
            },
            None => return Err(e),
        },
    };

    // 実体の特定: 直接EXE / インタプリタ+スクリプト / パッケージ管理下
    let (install_root, update_provider, artifact, strategy, verify) =
        if let Some(pm) = &resolved.package_manager {
            // npx/uvx 経由 → パッケージ管理下。直接上書きしない (仕様 §9, §15-13)
            let pkg_dir = pm
                .package
                .as_deref()
                .map(|p| PathBuf::from(format!("pkg:{p}")))
                .unwrap_or_else(|| PathBuf::from(format!("cmd:{}", launch.command)));
            (
                pkg_dir,
                Some(UpdateProvider::PackageManager {
                    manager: pm.manager.clone(),
                    package: pm.package.clone().unwrap_or_default(),
                }),
                None,
                None,
                None,
            )
        } else if let Some(uvpkg) = resolved.path.as_deref().and_then(uv_shim_package) {
            // uv tool install のシム (serena 等): 実体は uv の tool 環境にあり、
            // シムを上書きすると uv の管理を壊すため委譲扱いにする (仕様 §9)
            (
                PathBuf::from(format!("uv-tool:{uvpkg}")),
                Some(UpdateProvider::PackageManager {
                    manager: "uv-tool".into(),
                    package: uvpkg,
                }),
                None,
                None,
                None,
            )
        } else if let Some(script) = resolve::interpret_local_script(&launch) {
            // node script.js 等 → スクリプトの親ディレクトリを管理単位に
            let dir = script
                .parent()
                .and_then(|p| p.parent())
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| script.parent().unwrap_or(&script).to_path_buf());
            // dist/index.js → ルートは dist ではなくプロジェクトルート想定だが、
            // 配布単位はレシピで調整。初期はスクリプトの親dirをルートとする。
            let root = script
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or(dir.clone());
            let rel_entry = script
                .strip_prefix(&root)
                .unwrap_or(&script)
                .to_string_lossy()
                .replace('\\', "/");
            (
                root,
                None, // ソースは利用者が設定 (§5: 推測しない)
                Some(ArtifactSpec {
                    kind: ArtifactKind::Zip,
                    root_pattern: None,
                    entrypoint: rel_entry,
                    include: vec!["**/*".into()],
                    exclude: vec![],
                    data_priority: DataPriority::DataWins,
                }),
                Some(InstallStrategy {
                    strategy: StrategyKind::FixedPath,
                    apply_when: ApplyWhen::NextLaunch,
                    fixed_path: Some(script.parent().unwrap().to_path_buf()),
                    keep_generations: 3,
                    max_generations_mib: 0,
                }),
                None,
            )
        } else if let Some(p) = &resolved.path {
            // 直接 EXE
            let dir = p.parent().map(|d| d.to_path_buf()).unwrap_or_default();
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "tool.exe".into());
            (
                dir.clone(),
                None,
                Some(ArtifactSpec {
                    kind: ArtifactKind::Exe,
                    root_pattern: None,
                    entrypoint: name.clone(),
                    include: vec!["**/*".into()],
                    exclude: vec![],
                    data_priority: DataPriority::DataWins,
                }),
                Some(InstallStrategy {
                    strategy: StrategyKind::FixedPath,
                    apply_when: ApplyWhen::NextLaunch,
                    fixed_path: Some(dir),
                    keep_generations: 3,
                    max_generations_mib: 0,
                }),
                Some(VerifySpec {
                    command: vec!["{entrypoint}".into(), "--version".into()],
                    version_regex: Some(r"(\d+\.\d+\.\d+)".into()),
                    allowed_exit_codes: vec![0],
                    timeout_secs: 15,
                }),
            )
        } else {
            return Err(Error::NeedUser(format!(
                "command '{}' を解決できません (PATH候補: {})",
                launch.command,
                resolved
                    .candidates
                    .iter()
                    .map(|c| c.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        };

    let env = id::current_environment();
    let iid = id::installation_id(&install_root, &env);
    let recipe = Recipe {
        schema_version: SCHEMA_VERSION,
        id: id::slugify(&entry.name),
        kind: ToolKind::McpStdio,
        installation_id: iid.clone(),
        source_repository: source_repo,
        update_provider,
        artifact,
        installation: strategy,
        policy: Some(Policy {
            prepare_automatically: false,
            pinned_version: None,
            check_interval_hours: 24,
            on_preparation_failure: OnFailure::KeepCurrent,
        }),
        verify,
        data_preserve: vec![],
        data_migration_required: false,
    };

    let inst = Installation {
        id: iid,
        name: entry.name.clone(),
        kind: ToolKind::McpStdio,
        environment: env,
        install_root: normalize_path(&install_root),
        fixed_path: recipe
            .installation
            .as_ref()
            .and_then(|s| s.fixed_path.clone()),
        recipe,
        usages: vec![UsageRef::McpServer {
            scope: entry.scope.as_str().into(),
            config_path: entry.config_path.clone(),
            server_name: entry.name.clone(),
        }],
        created_at: Some(Utc::now()),
        updated_at: Some(Utc::now()),
    };
    store.upsert_installation(inst)
}

/// 解決済み exe が uv tool シムなら対応パッケージ名を返す。
/// `uv tool list` が使えれば正式名、取れなければ exe stem を採用。
fn uv_shim_package(path: &Path) -> Option<String> {
    if !resolve::is_uv_tool_shim(path) {
        return None;
    }
    let stem = path.file_stem()?.to_string_lossy().to_lowercase();
    Some(
        resolve::uv_tool_shim_packages()
            .and_then(|m| m.get(&stem).cloned())
            .unwrap_or(stem),
    )
}

/// 管理化 (ランチャー経由への設定書き換え) が可能な起動形式か事前検査する。
/// 一覧登録 (register_mcp_server) は実体把握のみで起動経路を変えないが、
/// manage は `dtm launch` に差し替えるため、ランチャーが扱えない形式を拒否する:
/// - パッケージマネージャ経由 (npx/uvx 等): 実体が管理下で artifact なし
/// - インタプリタ+スクリプト (node/python 等): ランチャーはインタプリタ
///   経由起動を未対応 (スクリプトを直接実行してしまう)
/// - 解決実体が .exe 以外: 直接起動できない形式
///
/// いずれも第2段階のアダプター対象 (仕様 §9)。
pub fn ensure_manageable(entry: &McpServerEntry) -> Result<()> {
    let launch = entry.launch.as_ref().ok_or_else(|| {
        Error::Config(format!(
            "'{}' はリモートサーバーのため管理化できません (stdio のみ対象)",
            entry.name
        ))
    })?;
    let resolved = resolve::resolve_command(launch)?;
    if let Some(pm) = &resolved.package_manager {
        return Err(Error::NeedUser(format!(
            "'{}' は {} 管理下のため管理化できません (更新は各マネージャへ委譲。一覧登録のみ可)",
            entry.name, pm.manager
        )));
    }
    // インタプリタ系コマンド (node/python 等) は引数のスクリプト有無に
    // 関わらず拒否する。スクリプトが未存在でも node.exe 自体の管理化に
    // なってしまうため (ランチャーはインタプリタ経由起動を未対応)
    let cmd_name = Path::new(&launch.command)
        .file_name()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if resolve::is_interpreter_command(&cmd_name) {
        return Err(Error::NeedUser(format!(
            "'{}' はインタプリタ+スクリプト形式のため管理化できません (アダプター未実装。一覧登録のみ可)",
            entry.name
        )));
    }
    // uv tool のシムは .exe だが uv 管理下のため拒否
    if let Some(p) = &resolved.path {
        if resolve::is_uv_tool_shim(p) {
            return Err(Error::NeedUser(format!(
                "'{}' は uv tool 管理下のシムのため管理化できません (uv tool upgrade へ委譲。一覧登録のみ可)",
                entry.name
            )));
        }
    }
    match &resolved.path {
        Some(p) if p.extension().map(|e| e == "exe").unwrap_or(false) => Ok(()),
        Some(p) => Err(Error::NeedUser(format!(
            "'{}' の実体 {} は直接起動可能な .exe ではないため管理化できません",
            entry.name,
            p.display()
        ))),
        None => Err(Error::NeedUser(format!(
            "'{}' のコマンド '{}' を解決できません",
            entry.name, launch.command
        ))),
    }
}

/// CLI ツール手動登録 (仕様 §4.2)。
/// spec 形式: 絶対パス | "dir|relative" | コマンド名
pub fn register_cli_tool(
    store: &Store,
    name: &str,
    specifier: &str,
    source_repo: Option<String>,
) -> Result<(Installation, bool, PathBuf)> {
    let (exe, via): (PathBuf, String) = if specifier.contains('|') {
        let mut it = specifier.splitn(2, '|');
        let dir = PathBuf::from(it.next().unwrap_or_default());
        let rel = it.next().unwrap_or_default();
        (normalize_path(&dir.join(rel)), "dir+rel".into())
    } else {
        let p = Path::new(specifier);
        if p.is_absolute() || specifier.contains(['\\', '/']) {
            (normalize_path(p), "abs-path".into())
        } else {
            // コマンド名 → PATH 解決
            let launch = LaunchSpec {
                command: specifier.into(),
                args: vec![],
                env: Default::default(),
                cwd: None,
            };
            let r = resolve::resolve_command(&launch)?;
            let path = r.path.clone().ok_or_else(|| {
                Error::NeedUser(format!(
                    "コマンド '{specifier}' を解決できません (候補: {})",
                    r.candidates
                        .iter()
                        .map(|c| c.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            })?;
            (path, "path-resolve".into())
        }
    };
    if !exe.exists() {
        return Err(Error::NotFound(format!(
            "実行ファイルが存在しません: {}",
            exe.display()
        )));
    }

    let dir = exe.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    let env = id::current_environment();
    let iid = id::installation_id(&dir, &env);
    let entry_name = exe
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "tool.exe".into());

    let recipe = Recipe {
        schema_version: SCHEMA_VERSION,
        id: id::slugify(name),
        kind: ToolKind::CliTool,
        installation_id: iid.clone(),
        source_repository: source_repo,
        update_provider: None, // ソースは利用者が設定 (仕様 §5)
        artifact: Some(ArtifactSpec {
            kind: ArtifactKind::Exe,
            root_pattern: None,
            entrypoint: entry_name.clone(),
            include: vec!["**/*".into()],
            exclude: vec![],
            data_priority: DataPriority::DataWins,
        }),
        installation: Some(InstallStrategy {
            strategy: StrategyKind::FixedPath,
            apply_when: ApplyWhen::NextLaunch,
            fixed_path: Some(dir.clone()),
            keep_generations: 3,
            max_generations_mib: 0,
        }),
        policy: Some(Policy {
            prepare_automatically: false,
            pinned_version: None,
            check_interval_hours: 24,
            on_preparation_failure: OnFailure::KeepCurrent,
        }),
        verify: Some(VerifySpec {
            command: vec!["{entrypoint}".into(), "--version".into()],
            version_regex: Some(r"(\d+\.\d+\.\d+)".into()),
            allowed_exit_codes: vec![0],
            timeout_secs: 15,
        }),
        data_preserve: vec![],
        data_migration_required: false,
    };

    let inst = Installation {
        id: iid,
        name: name.into(),
        kind: ToolKind::CliTool,
        environment: env,
        install_root: dir.clone(),
        fixed_path: Some(dir.clone()),
        recipe,
        usages: vec![UsageRef::Cli {
            via,
            specifier: specifier.into(),
        }],
        created_at: Some(Utc::now()),
        updated_at: Some(Utc::now()),
    };
    let (inst, created) = store.upsert_installation(inst)?;
    Ok((inst, created, exe))
}

/// RTK 初期レシピ (仕様 §10)。
/// - source: https://github.com/rtk-ai/rtk
/// - asset: rtk-x86_64-pc-windows-msvc.zip
/// - entrypoint: rtk.exe, 版確認 rtk --version
/// - 固定パス参照 (C:\work\rtk\rtk.exe) → fixed-path 方式
/// - フック設定/統計データは更新対象外 (data_preserve で防御)
pub fn rtk_recipe(install_dir: &Path, installation_id: &str) -> Recipe {
    Recipe {
        schema_version: SCHEMA_VERSION,
        id: "rtk".into(),
        kind: ToolKind::CliTool,
        installation_id: installation_id.into(),
        source_repository: Some("https://github.com/rtk-ai/rtk".into()),
        update_provider: Some(UpdateProvider::GithubRelease(GithubReleaseProvider {
            repository: "rtk-ai/rtk".into(),
            channel: ReleaseChannel::Stable,
            asset: AssetSelector {
                match_type: AssetMatchType::Exact,
                pattern: "rtk-x86_64-pc-windows-msvc.zip".into(),
                tag: None,
                platform: Some("windows".into()),
                architecture: Some("x64".into()),
            },
            version_scheme: VersionScheme::Semver,
        })),
        artifact: Some(ArtifactSpec {
            kind: ArtifactKind::Zip,
            // rtk zip は内部に rtk-x86_64-pc-windows-msvc/ ルートを持つ可能性
            root_pattern: None,
            entrypoint: "rtk.exe".into(),
            include: vec!["**/*".into()],
            exclude: vec![],
            data_priority: DataPriority::DataWins,
        }),
        installation: Some(InstallStrategy {
            strategy: StrategyKind::FixedPath,
            apply_when: ApplyWhen::NextLaunch,
            fixed_path: Some(install_dir.to_path_buf()),
            keep_generations: 3,
            max_generations_mib: 0,
        }),
        policy: Some(Policy {
            prepare_automatically: false,
            pinned_version: None,
            check_interval_hours: 24,
            on_preparation_failure: OnFailure::KeepCurrent,
        }),
        verify: Some(VerifySpec {
            command: vec!["{entrypoint}".into(), "--version".into()],
            version_regex: Some(r"(\d+\.\d+\.\d+)".into()),
            allowed_exit_codes: vec![0],
            timeout_secs: 15,
        }),
        // 仕様 §10: フック設定・統計データ・backup を上書きしない
        data_preserve: vec![
            "backup/**".into(),
            ".rtk/**".into(),
            "hooks/**".into(),
            "*.toml".into(),
            "*.json".into(),
        ],
        data_migration_required: false,
    }
}

/// RTK を登録 (既存実体 C:\work\rtk\rtk.exe を対象にする)
pub fn register_rtk(store: &Store, install_dir: &Path) -> Result<(Installation, bool)> {
    let exe = install_dir.join("rtk.exe");
    if !exe.exists() {
        return Err(Error::NotFound(format!(
            "rtk.exe が見つかりません: {}",
            exe.display()
        )));
    }
    let env = id::current_environment();
    let dir = normalize_path(install_dir);
    let iid = id::installation_id(&dir, &env);
    let inst = Installation {
        id: iid.clone(),
        name: "rtk".into(),
        kind: ToolKind::CliTool,
        environment: env,
        install_root: dir.clone(),
        fixed_path: Some(dir.clone()),
        recipe: rtk_recipe(&dir, &iid),
        usages: vec![UsageRef::Cli {
            via: "abs-path".into(),
            specifier: exe.to_string_lossy().to_string(),
        }],
        created_at: Some(Utc::now()),
        updated_at: Some(Utc::now()),
    };
    store.upsert_installation(inst)
}

/// MCP 設定を読みスキャン結果を返す (管理化は行わない)
pub struct ScanResult {
    pub config_files: Vec<(String, PathBuf)>,
    pub servers: Vec<ScannedServer>,
    pub errors: Vec<(PathBuf, String)>,
}

pub struct ScannedServer {
    pub name: String,
    pub scope: String,
    pub config_path: PathBuf,
    pub kind: ToolKind,
    pub command_summary: String,
    pub resolved_path: Option<PathBuf>,
    pub package_manager: Option<String>,
    /// 管理化 (ランチャー経由化) が可能な起動形式か
    pub manageable: bool,
    /// 管理化不可の理由 (UI 表示用)
    pub manage_note: Option<String>,
    pub disabled: bool,
    pub registry: Option<String>,
    /// 既存 installation との一致 (同一実体検出)
    pub installation_id: Option<String>,
}

pub fn scan(project_roots: &[PathBuf]) -> Result<ScanResult> {
    let mut files = vec![];
    let user_path = discover::user_config_path()?;
    if user_path.exists() {
        files.push((discover::ConfigScope::User, user_path));
    }
    for root in project_roots {
        files.extend(discover::project_config_paths(root));
    }
    Ok(scan_files(&files))
}

fn scan_files(files: &[(discover::ConfigScope, PathBuf)]) -> ScanResult {
    let mut result = ScanResult {
        config_files: vec![],
        servers: vec![],
        errors: vec![],
    };
    let mut seen = std::collections::HashSet::new();
    for (scope, path) in files {
        if !seen.insert(crate::paths::path_key(path)) {
            continue;
        }
        result
            .config_files
            .push((scope.as_str().into(), path.clone()));
        let cfg = match discover::read_config(*scope, path) {
            Ok(cfg) => cfg,
            Err(error) => {
                result.errors.push((path.clone(), error.to_string()));
                continue;
            }
        };
        let Some(entries) = cfg
            .raw
            .get("mcpServers")
            .and_then(|value| value.as_object())
        else {
            result.errors.push((
                path.clone(),
                "mcpServersがオブジェクトではありません".into(),
            ));
            continue;
        };
        for entry in discover::list_servers(&cfg) {
            let mut scanned = scan_one(&entry);
            scanned.registry = entries[&entry.name]
                .get("registry")
                .and_then(|value| value.as_str())
                .filter(|value| !value.trim().is_empty())
                .map(String::from);
            result.servers.push(scanned);
        }
        for (name, _) in entries.iter().filter(|(_, value)| !value.is_object()) {
            result.servers.push(ScannedServer {
                name: name.clone(),
                scope: scope.as_str().into(),
                config_path: path.clone(),
                kind: ToolKind::McpStdio,
                command_summary: "設定形式が不正".into(),
                resolved_path: None,
                package_manager: None,
                manageable: false,
                manage_note: Some("MCP設定がオブジェクトではありません".into()),
                disabled: false,
                registry: None,
                installation_id: None,
            });
        }
    }
    result
}

fn scan_one(e: &McpServerEntry) -> ScannedServer {
    let kind = discover::classify(e);
    let mut resolved_path = None;
    let mut pm = None;
    let mut resolve_note = String::new();
    // 管理化可否判定用: 解決後の .exe 実体 (スクリプト解決と区別)
    let mut resolved_exe: Option<PathBuf> = None;
    let mut resolve_err: Option<String> = None;
    let summary = if let Some(l) = &e.launch {
        // node script.js 形式はスクリプト実体を優先表示
        if let Some(script) = resolve::interpret_local_script(l) {
            resolved_path = Some(script);
        }
        // パッケージ管理下判定はコマンド名+引数のみで行う。
        // resolve_command 失敗 (PATH複数候補等) でも識別できるよう分離する。
        pm = resolve::detect_manager(&l.command, &l.args).map(|p| p.manager);
        match resolve::resolve_command(l) {
            Ok(r) => {
                resolved_exe = r.path.clone();
                if resolved_path.is_none() {
                    resolved_path = r.path.clone();
                }
            }
            Err(Error::NeedUser(m)) => {
                resolve_err = Some(m.clone());
                resolve_note = format!(" [選択要: {m}]");
            }
            Err(_) => {
                resolve_err = Some("未解決".into());
                resolve_note = " [未解決]".into();
            }
        }
        format!("{} {}{}", l.command, l.args.join(" "), resolve_note)
            .trim()
            .to_string()
    } else {
        e.url.clone().unwrap_or_default()
    };
    let (manageable, manage_note) = manageability(
        e,
        &kind,
        pm.as_deref(),
        resolved_exe.as_ref(),
        resolve_err.as_deref(),
    );
    ScannedServer {
        name: e.name.clone(),
        scope: e.scope.as_str().into(),
        config_path: e.config_path.clone(),
        kind,
        command_summary: summary,
        resolved_path,
        package_manager: pm,
        manageable,
        manage_note,
        disabled: e.disabled,
        registry: None,
        installation_id: None,
    }
}

/// UI 向けの管理化可否判定 (ensure_manageable と同一基準、副作用なし)。
fn manageability(
    e: &McpServerEntry,
    kind: &ToolKind,
    pm: Option<&str>,
    resolved_exe: Option<&PathBuf>,
    resolve_err: Option<&str>,
) -> (bool, Option<String>) {
    if e.launch.is_none() || *kind == ToolKind::McpRemote {
        return (false, Some("リモート".into()));
    }
    if let Some(m) = pm {
        return (false, Some(format!("{m} 管理下 (更新は委譲)")));
    }
    let cmd_name = e
        .launch
        .as_ref()
        .map(|l| {
            Path::new(&l.command)
                .file_name()
                .map(|s| s.to_string_lossy().to_lowercase())
                .unwrap_or_default()
        })
        .unwrap_or_default();
    if resolve::is_interpreter_command(&cmd_name) {
        return (false, Some("インタプリタ+スクリプト形式".into()));
    }
    match resolved_exe {
        Some(p) if resolve::is_uv_tool_shim(p) => (
            false,
            Some("uv tool 管理下 (uv tool upgrade へ委譲)".into()),
        ),
        Some(p) if p.extension().map(|x| x == "exe").unwrap_or(false) => (true, None),
        Some(p) => (
            false,
            Some(format!("実体 {} は .exe ではない", p.display())),
        ),
        None => (
            false,
            Some(format!("実体未解決: {}", resolve_err.unwrap_or("-"))),
        ),
    }
}

#[cfg(test)]
mod scan_tests {
    use super::*;

    #[test]
    fn scan_keeps_other_configs_and_reports_invalid_file() {
        let temp = tempfile::tempdir().unwrap();
        let good = temp.path().join("good.json");
        let bad = temp.path().join("bad.json");
        std::fs::write(&good, r#"{"mcpServers":{"remote":{"serverUrl":"http://localhost:1234/mcp","disabled":true,"registry":"example/tool"},"broken":42}}"#).unwrap();
        std::fs::write(&bad, "{").unwrap();
        let result = scan_files(&[
            (discover::ConfigScope::User, bad.clone()),
            (discover::ConfigScope::Project, good.clone()),
            (discover::ConfigScope::Project, good),
        ]);
        assert_eq!(result.errors.len(), 1);
        assert_eq!(result.errors[0].0, bad);
        assert_eq!(result.servers.len(), 2);
        let remote = result
            .servers
            .iter()
            .find(|entry| entry.name == "remote")
            .unwrap();
        assert_eq!(remote.kind, ToolKind::McpRemote);
        assert_eq!(remote.command_summary, "http://localhost:1234/mcp");
        assert_eq!(remote.registry.as_deref(), Some("example/tool"));
        assert!(remote.disabled);
        let broken = result
            .servers
            .iter()
            .find(|entry| entry.name == "broken")
            .unwrap();
        assert!(!broken.manageable);
        assert!(broken.manage_note.is_some());
    }

    #[test]
    fn scan_reports_invalid_server_map() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("config.json");
        std::fs::write(&file, r#"{"mcpServers":[]}"#).unwrap();
        let result = scan_files(&[(discover::ConfigScope::User, file)]);
        assert_eq!(result.errors.len(), 1);
        assert!(result.servers.is_empty());
    }
}

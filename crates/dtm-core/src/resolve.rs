//! コマンド解決と実体照合 (仕様 §4.2, §10)。
//! - 絶対パス → 正規化
//! - 裸コマンド名 → PATH+PATHEXT 解決。複数候補は選択必須エラー
//! - npx/uvx 等のランチャー経由は「パッケージ管理下」として識別
//!   (仕様 §9: 管理下の実体を直接上書きしない)

use crate::error::{Error, Result};
use crate::models::LaunchSpec;
use crate::paths::normalize_path;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct ResolvedCommand {
    /// 解決後の実パス (存在する場合)
    pub path: Option<PathBuf>,
    /// "absolute" | "path-resolve" | "unresolved"
    pub via: String,
    /// PATH 上の全候補 (複数時は選択用)
    pub candidates: Vec<PathBuf>,
    /// パッケージマネージャ識別子 ("npm", "uv" 等) - 管理下実体なら Some
    pub package_manager: Option<PackageManagerRef>,
}

#[derive(Debug, Clone)]
pub struct PackageManagerRef {
    /// "npm" | "uv" | "cargo" | "bun" | "pnpm" | "yarn" | "winget" | "unknown"
    pub manager: String,
    /// 対象パッケージ指定子 (例 "@playwright/mcp@latest")
    pub package: Option<String>,
}

const KNOWN_MANAGERS: &[(&str, &str)] = &[
    ("npx", "npm"),
    ("npx.cmd", "npm"),
    ("npx.exe", "npm"),
    ("npm", "npm"),
    ("uvx", "uv"),
    ("uvx.exe", "uv"),
    ("uv", "uv"),
    ("bunx", "bun"),
    ("bun", "bun"),
    ("pnpm", "pnpm"),
    ("yarn", "yarn"),
    ("cargo", "cargo"),
    ("winget", "winget"),
];

/// LaunchSpec.command を実パスへ解決する。
/// cwd は解決基準ディレクトリ (相対パス対応)。
pub fn resolve_command(launch: &LaunchSpec) -> Result<ResolvedCommand> {
    let cmd = launch.command.trim();
    if cmd.is_empty() {
        return Err(Error::Config("空の command".into()));
    }
    let cmd_path = Path::new(cmd);

    // 1) 絶対パス or 区切りを含む相対パス
    if cmd_path.is_absolute() || cmd.contains(['\\', '/']) {
        let base = launch.cwd.clone().unwrap_or_default();
        let joined = if cmd_path.is_absolute() {
            cmd_path.to_path_buf()
        } else {
            base.join(cmd_path)
        };
        let p = normalize_path(&joined);
        return Ok(ResolvedCommand {
            path: if p.exists() { Some(p.clone()) } else { None },
            via: "absolute".into(),
            candidates: vec![p],
            package_manager: detect_manager(cmd, &launch.args),
        });
    }

    // 2) 裸コマンド名 → PATH 解決 (PATHEXT 考慮)
    let candidates = resolve_on_path(cmd)?;
    let pm = detect_manager(cmd, &launch.args);
    match candidates.len() {
        0 => Ok(ResolvedCommand {
            path: None,
            via: "unresolved".into(),
            candidates,
            package_manager: pm,
        }),
        1 => Ok(ResolvedCommand {
            path: Some(normalize_path(&candidates[0])),
            via: "path-resolve".into(),
            candidates,
            package_manager: pm,
        }),
        _ => {
            // 同一実体に正規化される重複は潰す
            let mut uniq: Vec<PathBuf> = vec![];
            for c in &candidates {
                let n = normalize_path(c);
                if !uniq.contains(&n) {
                    uniq.push(n);
                }
            }
            if uniq.len() == 1 {
                return Ok(ResolvedCommand {
                    path: Some(uniq[0].clone()),
                    via: "path-resolve".into(),
                    candidates,
                    package_manager: pm,
                });
            }
            Err(Error::NeedUser(format!(
                "コマンド '{cmd}' が PATH 上で複数候補: {}",
                uniq.iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join("; ")
            )))
        }
    }
}

fn resolve_on_path(cmd: &str) -> Result<Vec<PathBuf>> {
    // which::which_all は PATHEXT を考慮した全候補を返す
    let mut found: Vec<PathBuf> = vec![];
    if let Ok(iter) = which::which_all(cmd) {
        found.extend(iter);
    }
    Ok(found)
}

/// コマンド名がパッケージマネージャのランチャーか判定。
/// `npx -y pkg`, `uvx --from x y`, `node script.js` 等を識別する。
pub fn detect_manager(cmd: &str, args: &[String]) -> Option<PackageManagerRef> {
    let name = Path::new(cmd)
        .file_name()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    for (key, mgr) in KNOWN_MANAGERS {
        if &name == key {
            // パッケージ指定子らしき引数を抽出
            let package = extract_package_arg(mgr, args);
            return Some(PackageManagerRef {
                manager: (*mgr).to_string(),
                package,
            });
        }
    }
    None
}

fn extract_package_arg(manager: &str, args: &[String]) -> Option<String> {
    match manager {
        "npm" | "bun" | "pnpm" | "yarn" => {
            // -y, -q 等のフラグを飛ばして最初の非フラグ引数
            args.iter().find(|a| !a.starts_with('-')).cloned()
        }
        "uv" => {
            // uvx --from X Y / uvx pkg: 最初の非フラグ非 --from 値
            let mut skip_next = false;
            for a in args {
                if skip_next {
                    skip_next = false;
                    continue;
                }
                if a == "--from" || a == "--with" || a == "-w" {
                    skip_next = true;
                    continue;
                }
                if !a.starts_with('-') {
                    return Some(a.clone());
                }
            }
            None
        }
        _ => None,
    }
}

/// コマンド文字列を spawn 可能な実パスへ解決する。
/// 絶対パス/パス区切りを含む相対パスは存在確認のみ、裸コマンド名は
/// PATH 検索 (spawn は PATHEXT を適用しないため .exe>.cmd>.bat の優先順で選択)。
pub fn resolve_spawn_command(cmd: &str) -> Option<PathBuf> {
    let p = Path::new(cmd);
    if p.is_absolute() || cmd.contains(['\\', '/']) {
        return p.exists().then(|| p.to_path_buf());
    }
    let mut all: Vec<PathBuf> = match which::which_all(cmd) {
        Ok(iter) => iter.collect(),
        Err(_) => vec![],
    };
    if all.is_empty() {
        return None;
    }
    let rank = |p: &PathBuf| match p
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .as_deref()
    {
        Some("exe") => 0,
        Some("cmd") => 1,
        Some("bat") => 2,
        _ => 3,
    };
    all.sort_by_key(rank);
    all.into_iter().next()
}

/// `uv tool install` がシムを置くディレクトリ。
/// UV_TOOL_BIN_DIR 優先、既定は %USERPROFILE%\.local\bin。
pub fn uv_tool_bin_dir() -> Option<PathBuf> {
    if let Ok(d) = std::env::var("UV_TOOL_BIN_DIR") {
        if !d.trim().is_empty() {
            return Some(PathBuf::from(d));
        }
    }
    dirs::home_dir().map(|h| h.join(".local").join("bin"))
}

/// 指定ファイルパスが uv tool のシムか (bin dir 直下かどうか)。
/// シムを直接上書きすると uv の管理を壊すため、管理化/適用の拒否に使う。
pub fn is_uv_tool_shim(path: &Path) -> bool {
    match (path.parent(), uv_tool_bin_dir()) {
        (Some(parent), Some(bin)) => normalize_path(parent) == normalize_path(&bin),
        _ => false,
    }
}

/// 指定ディレクトリが uv tool のシム配置先そのものか (適用対象dirの判定用)
pub fn is_uv_tool_bin_dir(dir: &Path) -> bool {
    match uv_tool_bin_dir() {
        Some(bin) => normalize_path(dir) == normalize_path(&bin),
        None => false,
    }
}

/// `uv tool list` の結果から shim exe 名 → パッケージ名 のマップを得る。
/// uv 不在/失敗時は None (その場合でも bin dir 判定は有効)。
pub fn uv_tool_shim_packages() -> Option<std::collections::HashMap<String, String>> {
    let mut cmd = std::process::Command::new("uv");
    cmd.args(["tool", "list"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    crate::procutil::hide_window(&mut cmd);
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut map = std::collections::HashMap::new();
    let mut pkg = String::new();
    for line in text.lines() {
        let l = line.trim_end();
        if let Some(exe) = l.trim_start().strip_prefix("- ") {
            let name = exe.trim().trim_end_matches(".exe").to_lowercase();
            if !name.is_empty() {
                map.insert(name, pkg.clone());
            }
        } else if !l.trim().is_empty() {
            // "serena-agent v1.7.0" 形式の先頭トークンがパッケージ名
            pkg = l.split_whitespace().next().unwrap_or("").to_string();
        }
    }
    Some(map)
}

/// コマンド名がインタプリタか判定 (node/python/bun/deno)。
/// 管理化可否判定にも使うため名前判定を切り出す。
pub fn is_interpreter_command(cmd_name_lower: &str) -> bool {
    matches!(
        cmd_name_lower,
        "node"
            | "node.exe"
            | "python"
            | "python.exe"
            | "python3"
            | "bun"
            | "bun.exe"
            | "deno"
            | "deno.exe"
            | "ruby"
            | "ruby.exe"
            | "perl"
            | "perl.exe"
            | "powershell"
            | "powershell.exe"
            | "pwsh"
            | "pwsh.exe"
    )
}

/// `node <script.js>` / `python script.py` のように
/// インタプリタ+ローカルファイル形式かを判定し、成果物ルートを推定する。
/// 戻り値: スクリプト実パス (解決できた場合)
pub fn interpret_local_script(launch: &LaunchSpec) -> Option<PathBuf> {
    script_arg_index(launch).map(|(_, p)| p)
}

/// `interpret_local_script` と同じ判定で、スクリプト引数の
/// args 内インデックスも返す。管理化時にこの index を記録し、
/// ランチャーが世代内エントリポイントへの差し替え位置として使う。
/// 戻り値: (args 内インデックス, スクリプト実パス)
pub fn script_arg_index(launch: &LaunchSpec) -> Option<(usize, PathBuf)> {
    let name = Path::new(&launch.command)
        .file_name()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if !is_interpreter_command(&name) {
        return None;
    }
    // 最初のローカルファイルらしき引数 (区切り or .js/.mjs/.py 拡張子)
    let cwd = launch.cwd.clone().unwrap_or_default();
    for (i, a) in launch.args.iter().enumerate() {
        if a.starts_with('-') {
            continue;
        }
        let p = Path::new(a);
        let cand = if p.is_absolute() {
            p.to_path_buf()
        } else {
            cwd.join(p)
        };
        if cand.exists() {
            return Some((i, normalize_path(&cand)));
        }
    }
    None
}

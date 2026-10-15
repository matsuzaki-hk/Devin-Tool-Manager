//! 世代構築 (仕様 §5.3, §6-5,6,7)。
//! - 空のステージングに新版を作成し旧ファイル混在を防ぐ
//! - rootPattern で展開ルート選択 (一意一致必須)
//! - include/exclude glob、保持データ優先
//! - 起動ファイル存在確認、非破壊的版確認

use crate::error::{Error, Result};
use crate::models::{GenerationMeta, Recipe, VerifySpec};
use crate::paths::{self, Layout};
use chrono::Utc;
use globset::{Glob, GlobSet, GlobSetBuilder};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// 展開/配置結果のプレビュー (レシピ試行と実配置の一致に使用。受け入れ条件 §15-5)
#[derive(Debug, Clone)]
pub struct PlacementPlan {
    /// 配置される相対パス → 供給元
    pub files: Vec<PlacedFile>,
    /// 保持対象として除外された相対パス
    pub preserved: Vec<String>,
    /// exclude で除外された相対パス
    pub excluded: Vec<String>,
    /// 起動ファイルの配置相対パス
    pub entrypoint: String,
}

#[derive(Debug, Clone)]
pub struct PlacedFile {
    /// 世代内相対パス (`/` 区切り)
    pub rel: String,
    /// 供給元の絶対パス (ステージング内)
    pub source: PathBuf,
    pub size: u64,
}

/// glob 構築。レシピの区切りは `/`、`**` で再帰。大小文字は Windows 不区別。
pub fn build_globset(patterns: &[String]) -> Result<GlobSet> {
    let mut b = GlobSetBuilder::new();
    for p in patterns {
        let g = Glob::new(p).map_err(|e| Error::Config(format!("glob パターン不正 '{p}': {e}")))?;
        b.add(g);
    }
    b.build()
        .map_err(|e| Error::Config(format!("glob 構築失敗: {e}")))
}

fn match_ci(set: &GlobSet, rel: &str) -> bool {
    // Windows は大小文字不区別: 候補の大小文字両方で照合するのではなく
    // globset は大小文字区別するため、パターン側の運用に依存する。
    // 初期版はそのまま照合 (レシピ作成時に大小文字を正しく書く前提)。
    set.is_match(rel)
}

/// ソース (展開済みZIPルート or 単一EXE or フォルダ) から
/// 配置計画を構築する。実ファイルをまだ動かさずに済む (プレビュー用)。
///
/// - `source_root`: 展開済みツリーのルート (EXE直接配布時はファイルの親dir)
/// - `single_file`: EXE直接配布など、単一ファイルのみの場合のファイル名
pub fn plan_placement(
    recipe: &Recipe,
    source_root: &Path,
    single_file: Option<&str>,
) -> Result<PlacementPlan> {
    let art = recipe
        .artifact
        .as_ref()
        .ok_or_else(|| Error::Config("artifact 未定義のレシピ".into()))?;

    // 収集: source_root 配下の全ファイル (相対 `/` パス)
    let mut all: Vec<(String, PathBuf, u64)> = vec![];
    if let Some(name) = single_file {
        let p = source_root.join(name);
        let size = fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
        all.push((name.replace('\\', "/"), p, size));
    } else {
        collect_files(source_root, source_root, &mut all)?;
    }

    // rootPattern: 一意一致必須 (仕様 §12)
    let mut root_prefix = String::new();
    if let Some(rp) = &art.root_pattern {
        if !rp.trim().is_empty() {
            let set = build_globset(&[rp.trim_end_matches('/').to_string()])?;
            // ディレクトリ一致: 各ファイルの第1階層ディレクトリで判定
            let mut dirs: std::collections::BTreeSet<String> = Default::default();
            for (rel, _, _) in &all {
                if let Some(first) = rel.split('/').next() {
                    if rel.contains('/') {
                        dirs.insert(first.to_string());
                    }
                }
            }
            let matches: Vec<&String> = dirs.iter().filter(|d| set.is_match(d.as_str())).collect();
            match matches.len() {
                0 => {
                    return Err(Error::NeedUser(format!(
                        "rootPattern '{rp}' に一致するルートがありません (候補: {})",
                        dirs.iter().cloned().collect::<Vec<_>>().join(", ")
                    )))
                }
                1 => root_prefix = format!("{}/", matches[0]),
                _ => {
                    return Err(Error::NeedUser(format!(
                        "rootPattern '{rp}' が複数一致 ({}): 一意条件に修正してください",
                        matches
                            .iter()
                            .map(|s| s.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )))
                }
            }
        }
    }

    let include = build_globset(&art.include)?;
    let exclude = build_globset(&art.exclude)?;
    let preserve = build_globset(&recipe.data_preserve)?;

    let mut files = vec![];
    let mut preserved = vec![];
    let mut excluded = vec![];
    for (rel, src, size) in &all {
        // root_prefix 除去後が配置先相対パス
        let stripped = rel
            .strip_prefix(&root_prefix)
            .unwrap_or(rel.as_str())
            .to_string();
        if stripped.is_empty() {
            continue;
        }
        if !match_ci(&include, &stripped) && !match_ci(&include, rel) {
            excluded.push(stripped);
            continue;
        }
        if match_ci(&exclude, &stripped) || match_ci(&exclude, rel) {
            excluded.push(stripped);
            continue;
        }
        if match_ci(&preserve, &stripped) || match_ci(&preserve, rel) {
            preserved.push(stripped);
            continue;
        }
        files.push(PlacedFile {
            rel: stripped,
            source: src.clone(),
            size: *size,
        });
    }

    Ok(PlacementPlan {
        files,
        preserved,
        excluded,
        entrypoint: art.entrypoint.replace('\\', "/"),
    })
}

fn collect_files(root: &Path, dir: &Path, out: &mut Vec<(String, PathBuf, u64)>) -> Result<()> {
    for e in fs::read_dir(dir)? {
        let e = e?;
        let p = e.path();
        if p.is_dir() {
            collect_files(root, &p, out)?;
        } else if p.is_file() {
            let rel = p
                .strip_prefix(root)
                .unwrap_or(&p)
                .to_string_lossy()
                .replace('\\', "/");
            let size = e.metadata().map(|m| m.len()).unwrap_or(0);
            out.push((rel, p, size));
        }
    }
    Ok(())
}

/// 計画どおりに世代ディレクトリへコピー。中間は staging → 完成後 rename で
/// generations へ移し、不完全な世代が見えないようにする。
pub fn materialize_generation(
    layout: &Layout,
    recipe: &Recipe,
    plan: &PlacementPlan,
    version: &str,
    source: crate::models::GenerationSource,
    verification: crate::models::VerificationResult,
    staging_dir: &Path,
) -> Result<(String, PathBuf)> {
    // 起動ファイル存在確認 (仕様 §6-5)
    if !plan.files.iter().any(|f| f.rel == plan.entrypoint) {
        return Err(Error::Verify(format!(
            "起動ファイル '{}' が配置計画に存在しません (配置対象: {})",
            plan.entrypoint,
            plan.files
                .iter()
                .take(10)
                .map(|f| f.rel.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }

    // 世代ID: <版>-<時刻>-<短ハッシュ>。同名版の重複準備も別世代として識別
    let uniq = &paths::sha256_bytes(
        format!(
            "{}:{}",
            version,
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        )
        .as_bytes(),
    )[..6];
    let gen_id = format!(
        "{}-{}-{}",
        sanitize_version(version),
        Utc::now().format("%Y%m%d%H%M%S"),
        uniq
    );
    let target = layout.generation_dir(&recipe.installation_id, &gen_id);
    let payload_tmp = staging_dir.join("payload");
    if payload_tmp.exists() {
        fs::remove_dir_all(&payload_tmp)?;
    }
    fs::create_dir_all(&payload_tmp)?;

    for f in &plan.files {
        let dst = payload_tmp.join(&f.rel);
        if let Some(p) = dst.parent() {
            fs::create_dir_all(p)?;
        }
        fs::copy(&f.source, &dst)?;
    }

    // genmeta 書き込み (files=配布物マニフェスト。verified は版確認合格後に立つ)
    let meta = GenerationMeta {
        generation_id: gen_id.clone(),
        version: version.to_string(),
        created_at: Utc::now(),
        recipe_revision: recipe.schema_version as u64,
        source,
        verification,
        probed_version: None,
        verified: false,
        applied_at: None,
        files: plan.files.iter().map(|f| f.rel.clone()).collect(),
    };
    paths::atomic_write_json(&payload_tmp.join("genmeta.json"), &meta)?;

    // 完成後に世代ディレクトリへ移動 (staging と generations は同一ボリューム前提)
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    if target.exists() {
        fs::remove_dir_all(&target)?;
    }
    fs::rename(&payload_tmp, &target)?;
    Ok((gen_id, target))
}

fn sanitize_version(v: &str) -> String {
    v.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect::<String>()
        .chars()
        .take(48)
        .collect()
}

/// 非破壊的な版確認 (仕様 §6-6)。
/// 登録されたコマンドを timeout 付きで実行し、出力から版を抽出する。
/// 任意EXEの起動が無害とは仮定しないため、verify 未登録なら実行しない。
pub fn probe_version(entrypoint: &Path, verify: &VerifySpec) -> Result<Option<String>> {
    if verify.command.is_empty() {
        return Ok(None);
    }
    let mut argv: Vec<String> = verify
        .command
        .iter()
        .map(|a| {
            if a == "{entrypoint}" {
                entrypoint.to_string_lossy().to_string()
            } else {
                a.clone()
            }
        })
        .collect();
    if argv.is_empty() {
        return Ok(None);
    }
    // "{entrypoint}" を含まない場合は先頭を entrypoint で置換
    if !verify.command.iter().any(|a| a == "{entrypoint}") {
        argv[0] = entrypoint.to_string_lossy().to_string();
    }
    let prog = argv.remove(0);
    let work_dir = entrypoint.parent().unwrap_or_else(|| Path::new("."));
    let output = run_timeout(
        &prog,
        &argv,
        work_dir,
        Duration::from_secs(verify.timeout_secs),
    )?;
    if !verify.allowed_exit_codes.contains(&output.code) {
        return Err(Error::Verify(format!(
            "版確認コマンドが終了コード {} (許容: {:?})",
            output.code, verify.allowed_exit_codes
        )));
    }
    let text = format!("{}{}", output.stdout, output.stderr);
    let ver = if let Some(re) = &verify.version_regex {
        regex::Regex::new(re)
            .ok()
            .and_then(|r| r.captures(&text))
            .and_then(|c| c.get(1).or_else(|| c.get(0)))
            .map(|m| m.as_str().trim().to_string())
    } else {
        let t = text.trim();
        if t.is_empty() {
            None
        } else {
            Some(t.lines().next().unwrap_or("").trim().to_string())
        }
    };
    Ok(ver)
}

/// 版検出のフォールバックチェーン (ローカル実行ファイル向け)。
///
/// 1) レシピ verify コマンド → 2) 汎用フラグ (--version/-V/-v/version)
///    → 3) Windows PE ファイルバージョン (VersionInfo)
///
/// いずれも非破壊実行。全失敗時は None。
pub fn probe_version_auto(exe: &Path, verify: Option<&VerifySpec>) -> Option<String> {
    if let Some(v) = verify {
        if let Ok(Some(ver)) = probe_version(exe, v) {
            return Some(ver);
        }
    }
    let cwd = exe.parent().unwrap_or_else(|| Path::new("."));
    let prog = exe.to_string_lossy().to_string();
    for flag in ["--version", "-V", "version", "-v"] {
        if let Ok(o) = run_timeout(&prog, &[flag.to_string()], cwd, Duration::from_secs(5)) {
            if let Some(v) = extract_version(&format!("{}{}", o.stdout, o.stderr)) {
                return Some(v);
            }
        }
    }
    pe_file_version(exe)
}

/// テキストから版らしきトークンを抽出 (x.y または x.y.z、接尾辞許容)
fn extract_version(text: &str) -> Option<String> {
    regex::Regex::new(r"(\d+\.\d+(?:\.\d+){0,2}(?:[-.][0-9A-Za-z.+-]+)?)")
        .ok()?
        .captures(text)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().trim().to_string())
}

/// Windows PE の FileVersion を PowerShell 経由で取得 (.exe のみ)。
/// Rust から PE バージョンリソースを読む専用依存を増やさないため、
/// 登録/検出時のみ呼ぶ低頻度経路として PowerShell に委譲する。
#[cfg(windows)]
fn pe_file_version(exe: &Path) -> Option<String> {
    if exe
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase() != "exe")
        .unwrap_or(true)
    {
        return None;
    }
    let esc = exe.display().to_string().replace('\'', "''");
    let mut cmd = std::process::Command::new("powershell");
    cmd.args([
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        &format!("(Get-Item -LiteralPath '{esc}').VersionInfo.FileVersion"),
    ])
    .stdin(std::process::Stdio::null())
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::null());
    crate::procutil::hide_window(&mut cmd);
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    extract_version(&text)
}

#[cfg(not(windows))]
fn pe_file_version(_exe: &Path) -> Option<String> {
    None
}

pub struct RunOutput {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// タイムアウト付き実行 (stdio はパイプで回収。管理ログ混入なし)
pub fn run_timeout(
    prog: &str,
    args: &[String],
    cwd: &Path,
    timeout: Duration,
) -> Result<RunOutput> {
    let mut cmd = std::process::Command::new(prog);
    cmd.args(args)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    crate::procutil::hide_window(&mut cmd);
    let mut child = cmd.spawn()?;

    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait()? {
            Some(status) => {
                let mut stdout = String::new();
                let mut stderr = String::new();
                use std::io::Read;
                if let Some(mut s) = child.stdout.take() {
                    let _ = s.read_to_string(&mut stdout);
                }
                if let Some(mut s) = child.stderr.take() {
                    let _ = s.read_to_string(&mut stderr);
                }
                return Ok(RunOutput {
                    code: status.code().unwrap_or(-1),
                    stdout,
                    stderr,
                });
            }
            None => {
                if std::time::Instant::now() > deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(Error::Verify(format!(
                        "版確認コマンドがタイムアウト ({} 秒)",
                        timeout.as_secs()
                    )));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

/// 保持世代数の超過分を選定する (削除はしない。dry-run / 容量見積用)。
/// 現行世代・適用待ち世代は常に保護。genmeta 不在の残骸ディレクトリも
/// 候補になる (呼び出し側がインストールロックを保持している前提)。
/// 戻り値: (世代名, パス) の削除候補。
pub fn prune_candidates(
    layout: &Layout,
    installation_id: &str,
    keep: u32,
    active_gen: Option<&str>,
    prepared_gen: Option<&str>,
) -> Result<Vec<(String, PathBuf)>> {
    let base = layout.generations_base(installation_id);
    if !base.exists() {
        return Ok(vec![]);
    }
    let mut gens: Vec<(String, PathBuf, std::time::SystemTime)> = vec![];
    for e in fs::read_dir(&base)? {
        let e = e?;
        let p = e.path();
        if p.is_dir() {
            let modified = e
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            gens.push((e.file_name().to_string_lossy().to_string(), p, modified));
        }
    }
    // 新しい順
    gens.sort_by_key(|g| std::cmp::Reverse(g.2));
    let keep = keep.max(1) as usize;
    Ok(gens
        .into_iter()
        .enumerate()
        .filter(|(i, (name, _, _))| {
            *i >= keep && Some(name.as_str()) != active_gen && Some(name.as_str()) != prepared_gen
        })
        .map(|(_, (name, path, _))| (name, path))
        .collect())
}

/// 保持世代数・容量上限の適用 (仕様 §13: 最低1つの正常な旧版、使用中は削除しない)
pub fn prune_generations(
    layout: &Layout,
    installation_id: &str,
    keep: u32,
    active_gen: Option<&str>,
    prepared_gen: Option<&str>,
) -> Result<Vec<String>> {
    let mut removed = vec![];
    for (name, path) in prune_candidates(layout, installation_id, keep, active_gen, prepared_gen)? {
        fs::remove_dir_all(&path)?;
        removed.push(name);
    }
    Ok(removed)
}

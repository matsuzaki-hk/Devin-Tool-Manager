//! 有効化・復元 (仕様 §6.1, §13, §15-3,7)。
//! - versioned: active.json の原子的切替 (既存プロセスは旧版継続可能)
//! - fixed-path: 計画を先行記録したジャーナル+バックアップ+起動遮断で
//!   中間状態を防ぐ。中断復旧はジャーナル計画と実ファイル状態の照合で冪等。
//! - 外部プロセスが掴むファイルを強制終了しない。利用中は保留。
//! - 適用・復元の対象は「検証済みまたは過去に適用した世代」のみ (R8)

use crate::error::{Error, Result};
use crate::models::{GenerationMeta, InstallState, Installation, Status, StrategyKind};
use crate::paths::{self};
use crate::store::Store;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

// ---------- versioned 適用 ----------

/// 準備済み世代を有効世代へ切替える。
/// 呼び出し前に installation ロックを取得していること。
pub fn activate_versioned(store: &Store, inst: &Installation, st: &mut InstallState) -> Result<()> {
    let gen = st
        .prepared_generation
        .clone()
        .ok_or_else(|| Error::Config("適用待ちの世代がありません".into()))?;
    let dir = store.layout.generation_dir(&inst.id, &gen);
    let meta = load_launchable_genmeta(store, &inst.id, &gen)?;

    st.status = Some(Status::Applying);
    store.save_state(&inst.id, st)?;

    store.set_active_generation(&inst.id, &gen)?;
    mark_applied(&dir);

    st.current_version = Some(meta.version.clone());
    st.active_generation = Some(gen);
    st.prepared_generation = None;
    st.prepared_version = None;
    st.status = Some(Status::UpToDate);
    st.last_error = None;
    store.save_state(&inst.id, st)?;
    store.push_event(&inst.id, "apply", &format!("世代適用: {}", meta.version));
    Ok(())
}

/// 世代メタを読み、起動可能 (検証済みまたは過去適用済み) な場合だけ返す。
/// 検証失敗・準備途中の世代を適用しないための共通ゲート (仕様 §15-3)。
fn load_launchable_genmeta(
    store: &Store,
    installation_id: &str,
    gen: &str,
) -> Result<GenerationMeta> {
    let dir = store.layout.generation_dir(installation_id, gen);
    if !dir.join("genmeta.json").exists() {
        return Err(Error::Verify(format!(
            "世代 {gen} が不完全です (genmeta.json なし)。再準備してください"
        )));
    }
    let meta = store.load_genmeta(installation_id, gen)?;
    if !meta.verified && meta.applied_at.is_none() {
        return Err(Error::Verify(format!(
            "世代 {gen} は起動検証未合格のため適用対象外です。再準備してください"
        )));
    }
    Ok(meta)
}

/// 適用済み世代へ applied_at を刻む (復元候補の選定材料)。
fn mark_applied(gen_dir: &Path) {
    if let Ok(mut m) = paths::read_json::<GenerationMeta>(&gen_dir.join("genmeta.json")) {
        m.applied_at = Some(Utc::now());
        let _ = paths::atomic_write_json(&gen_dir.join("genmeta.json"), &m);
    }
}

// ---------- fixed-path 差し替え ----------

/// 差し替えジャーナル。計画全体を変更前に記録し、復旧は実ファイル状態との
/// 照合で行う (操作単位の事後記録ではクラッシュ区間が残るため)。
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SwapJournal {
    started_at: chrono::DateTime<Utc>,
    install_dir: PathBuf,
    backup_dir: PathBuf,
    /// 退避対象 (新版にも存在し、適用前に配置先に実在した相対パス)
    backup_files: Vec<String>,
    /// 配置対象 (新版の配布物相対パス)
    place_files: Vec<String>,
    /// 新版に無い旧配布物 (バックアップへ退避して除去)
    #[serde(default)]
    stale_files: Vec<String>,
    /// "swapping" | "done" | "rolled-back" | "rollback-failed"
    phase: String,
}

fn journal_path(backup_dir: &Path) -> PathBuf {
    backup_dir.join("journal.json")
}

/// 対象ファイルが外部プロセスに掴まれているか検査 (仕様 §15-7)。
/// Windows では実行中EXE/ロード中DLLは書き込みオープンに失敗する。
fn file_in_use(path: &Path) -> bool {
    if !path.exists() {
        return false;
    }
    // append モードでのオープンは共有違反で失敗する
    fs::OpenOptions::new().append(true).open(path).is_err()
}

/// fixed-path 方式の適用:
/// 1) 未完了差し替えの復旧 (失敗したら中止)
/// 2) 全対象ファイルの in-use 検査 → 利用中なら waiting-in-use
/// 3) 計画 (退避/配置/旧配布物除去) をジャーナルへ先行記録
/// 4) 現行ファイルを <install>/.dtm-backup/<ts>/ へ rename
/// 5) 世代ペイロードを配置、旧配布物を退避
/// 6) 失敗/中断時はジャーナルから復旧 (復旧失敗は完了扱いにしない)
pub fn apply_fixed_path(store: &Store, inst: &Installation, st: &mut InstallState) -> Result<()> {
    let gen = st
        .prepared_generation
        .clone()
        .ok_or_else(|| Error::Config("適用待ちの世代がありません".into()))?;
    let gen_dir = store.layout.generation_dir(&inst.id, &gen);
    let meta = load_launchable_genmeta(store, &inst.id, &gen)?;
    let install_dir = inst
        .fixed_path
        .clone()
        .or_else(|| Some(inst.install_root.clone()))
        .ok_or_else(|| Error::Config("fixed_path 未設定".into()))?;

    // uv tool のシム配置先は外部管理 (uv tool upgrade へ委譲)。
    // 登録状態に関わらず上書きを拒否する安全ガード (仕様 §9, serena 等)。
    if crate::resolve::is_uv_tool_bin_dir(&install_dir) {
        return Err(Error::NeedUser(format!(
            "適用先 {} は uv tool のシム配置先のため DTM では適用しません (uv tool upgrade へ委譲)",
            install_dir.display()
        )));
    }

    // 1) 中断ジャーナルの回復 (失敗したら適用しない。復旧失敗は起動も止める)
    recover_pending_swap(&install_dir)?;

    // 2) 新旧の対象ファイル集合。新版は世代マニフェストを正とする
    //    (genmeta.files が無い旧世代はディレクトリ走査へフォールバック)
    let new_files: Vec<String> = if meta.files.is_empty() {
        collect_rel_files(&gen_dir)?
    } else {
        meta.files.clone()
    };
    // 旧配布物マニフェスト: 直前に有効だった世代のファイル一覧。
    // 新版に存在しない旧配布ファイルは退避して取り除く (R10)。
    // ユーザーデータや保持対象はマニフェスト外なので対象にしない。
    let prev_files = st
        .active_generation
        .as_deref()
        .and_then(|g| store.load_genmeta(&inst.id, g).ok())
        .map(|m| m.files)
        .unwrap_or_default();
    let new_set: BTreeSet<&String> = new_files.iter().collect();
    let stale: Vec<String> = prev_files
        .iter()
        .filter(|r| !new_set.contains(r))
        .filter(|r| install_dir.join(r).exists())
        .cloned()
        .collect();

    // 3) in-use 検査 (配置対象と退避対象の両方)
    let busy: Vec<String> = new_files
        .iter()
        .chain(stale.iter())
        .map(|r| install_dir.join(r))
        .filter(|p| file_in_use(p))
        .map(|p| p.display().to_string())
        .collect();
    if !busy.is_empty() {
        st.status = Some(Status::WaitingInUse);
        st.last_error = Some(format!("使用中ファイル: {}", busy.join(", ")));
        store.save_state(&inst.id, st)?;
        return Err(Error::Busy(format!(
            "ファイルが使用中のため保留: {}",
            busy.join(", ")
        )));
    }

    st.status = Some(Status::Applying);
    store.save_state(&inst.id, st)?;

    // 4) ジャーナルを計画込みで先行記録してから変更開始
    let ts = Utc::now().format("%Y%m%d%H%M%S").to_string();
    let backup_dir = install_dir.join(".dtm-backup").join(&ts);
    fs::create_dir_all(&backup_dir)?;
    let backup_files: Vec<String> = new_files
        .iter()
        .filter(|r| install_dir.join(r).exists())
        .cloned()
        .collect();
    let mut journal = SwapJournal {
        started_at: Utc::now(),
        install_dir: install_dir.clone(),
        backup_dir: backup_dir.clone(),
        backup_files: backup_files.clone(),
        place_files: new_files.clone(),
        stale_files: stale.clone(),
        phase: "swapping".into(),
    };
    paths::atomic_write_json(&journal_path(&backup_dir), &journal)?;

    let mut result: Result<()> = (|| {
        // 既存側の対象ファイルをバックアップへ退避
        for rel in &backup_files {
            let cur = install_dir.join(rel);
            let bk = backup_dir.join(rel);
            if let Some(p) = bk.parent() {
                fs::create_dir_all(p)?;
            }
            fs::rename(&cur, &bk)?;
        }
        // 新版配置 (新規ファイルへの部分コピーは復旧時に実体照合で除去)
        for rel in &new_files {
            let src = gen_dir.join(rel);
            let dst = install_dir.join(rel);
            if let Some(p) = dst.parent() {
                fs::create_dir_all(p)?;
            }
            fs::copy(&src, &dst)?;
        }
        // 新版に無い旧配布物を退避
        for rel in &stale {
            let cur = install_dir.join(rel);
            let bk = backup_dir.join(rel);
            if let Some(p) = bk.parent() {
                fs::create_dir_all(p)?;
            }
            if cur.exists() {
                fs::rename(&cur, &bk)?;
            }
        }
        journal.phase = "done".into();
        paths::atomic_write_json(&journal_path(&backup_dir), &journal)?;
        Ok(())
    })();

    if let Err(e) = &mut result {
        crate::dloge!("fixed-path 適用失敗、復旧を試行: {e}");
        let recovered = restore_journal(&journal);
        match recovered {
            Ok(()) => {
                journal.phase = "rolled-back".into();
                let _ = paths::atomic_write_json(&journal_path(&backup_dir), &journal);
                st.status = Some(Status::Failed);
                st.last_error = Some(format!("適用失敗 (旧版復旧済み): {e}"));
                store.save_state(&inst.id, st)?;
                return Err(Error::Other(format!("適用失敗・旧版へ復旧済み: {e}")));
            }
            Err(re) => {
                // 復旧失敗は完了扱いにしない。中間状態のまま起動を止める
                journal.phase = "rollback-failed".into();
                let _ = paths::atomic_write_json(&journal_path(&backup_dir), &journal);
                st.status = Some(Status::Failed);
                st.last_error = Some(format!("適用失敗・復旧失敗: {e} / {re}"));
                store.save_state(&inst.id, st)?;
                return Err(Error::Other(format!(
                    "適用失敗・復旧も失敗 (手動確認が必要): {e} / {re}"
                )));
            }
        }
    }

    // 診断一貫性のためポインタも記録 (fixed-path の起動解決は実ファイル側)
    let _ = store.set_active_generation(&inst.id, &gen);
    mark_applied(&gen_dir);
    st.current_version = Some(meta.version.clone());
    st.active_generation = Some(gen);
    st.prepared_generation = None;
    st.prepared_version = None;
    st.status = Some(Status::UpToDate);
    st.last_error = None;
    store.save_state(&inst.id, st)?;
    store.push_event(
        &inst.id,
        "apply",
        &format!(
            "固定パス適用: {} (バックアップ: {})",
            meta.version,
            backup_dir.display()
        ),
    );
    Ok(())
}

/// install_dir 配下の未完了ジャーナル(phase != done/rolled-back)を検出し復旧。
/// ランチャー起動前にも呼ばれるため公開。復旧失敗は Err で伝播する
/// (中間状態のまま起動しないため。呼び出し側は起動を中止すること)。
pub fn recover_pending_swap(install_dir: &Path) -> Result<()> {
    let root = install_dir.join(".dtm-backup");
    if !root.exists() {
        return Ok(());
    }
    for e in fs::read_dir(&root)? {
        let dir = e?.path();
        if !dir.is_dir() {
            continue;
        }
        let jp = journal_path(&dir);
        if !jp.exists() {
            // ジャーナル未作成のバックアップ dir = 計画記録前に中断
            // (まだファイルを動かしていないので安全)。空なら除去。
            let _ = fs::remove_dir(&dir);
            continue;
        }
        if let Ok(j) = paths::read_json::<SwapJournal>(&jp) {
            if j.phase != "done" && j.phase != "rolled-back" {
                crate::dlogw!("中断された差し替えを検出、復旧: {}", dir.display());
                restore_journal(&j).map_err(|e| {
                    Error::Other(format!(
                        "中断した差し替えの復旧に失敗しました ({}): {e}",
                        dir.display()
                    ))
                })?;
                let mut j2 = j;
                j2.phase = "rolled-back".into();
                paths::atomic_write_json(&jp, &j2)?;
            }
        }
    }
    Ok(())
}

/// ジャーナルの計画と実ファイル状態を照合して旧版へ復旧する。
/// 各操作の完了マーカーに依存せず、存在するものだけを処理するため冪等。
/// 失敗は Err で伝播する (呼び出し側は完了扱いしない)。
fn restore_journal(j: &SwapJournal) -> Result<()> {
    let mut errs: Vec<String> = vec![];

    // 1) 配置済み(または部分コピー)の新版ファイルを除去。
    //    backup_files に含まれるものは後段で旧版を戻す。
    for rel in &j.place_files {
        let dst = j.install_dir.join(rel);
        if dst.exists() {
            if let Err(e) = fs::remove_file(&dst) {
                errs.push(format!("{rel}: 新版除去失敗 {e}"));
            }
        }
    }
    // 2) 退避済みの旧ファイルと旧配布物を復元
    for rel in j.backup_files.iter().chain(j.stale_files.iter()) {
        let bk = j.backup_dir.join(rel);
        let cur = j.install_dir.join(rel);
        if bk.exists() {
            if let Some(p) = cur.parent() {
                let _ = fs::create_dir_all(p);
            }
            if cur.exists() {
                let _ = fs::remove_file(&cur);
            }
            if let Err(e) = fs::rename(&bk, &cur) {
                errs.push(format!("{rel}: 旧版復元失敗 {e}"));
            }
        }
    }
    if errs.is_empty() {
        Ok(())
    } else {
        Err(Error::Other(errs.join("; ")))
    }
}

fn collect_rel_files(gen_dir: &Path) -> Result<Vec<String>> {
    let mut out = vec![];
    collect_inner(gen_dir, gen_dir, &mut out)?;
    // genmeta.json は管理ファイルなので配置対象外
    out.retain(|r| r != "genmeta.json");
    Ok(out)
}

fn collect_inner(root: &Path, dir: &Path, out: &mut Vec<String>) -> Result<()> {
    for e in fs::read_dir(dir)? {
        let p = e?.path();
        if p.is_dir() {
            collect_inner(root, &p, out)?;
        } else {
            out.push(
                p.strip_prefix(root)
                    .unwrap_or(&p)
                    .to_string_lossy()
                    .replace('\\', "/"),
            );
        }
    }
    Ok(())
}

// ---------- ロールバック (旧版復元) ----------

/// 指定世代または「最後に実際に適用された旧世代」へ戻す。
/// 候補は検証合格済みまたは過去適用済みの世代に限定し、
/// 検証失敗・準備途中の世代は復元先にしない (R8)。
pub fn rollback(store: &Store, inst: &Installation, target_gen: Option<&str>) -> Result<String> {
    let base = store.layout.generations_base(&inst.id);
    // 起動可能な世代だけを候補化
    let mut candidates: Vec<GenerationMeta> = vec![];
    if base.exists() {
        for e in fs::read_dir(&base)? {
            let e = e?;
            let p = e.path();
            if p.is_dir() && p.join("genmeta.json").exists() {
                if let Ok(m) = store.load_genmeta(&inst.id, &e.file_name().to_string_lossy()) {
                    if m.verified || m.applied_at.is_some() {
                        candidates.push(m);
                    }
                }
            }
        }
    }
    if candidates.is_empty() {
        return Err(Error::NotFound(
            "復元可能な世代 (検証済/適用済) がありません".into(),
        ));
    }
    // 現行世代: state.active_generation を正本とする
    let st_prev = store.load_state(&inst.id)?;
    let current = st_prev
        .active_generation
        .clone()
        .or(store.active_generation(&inst.id)?);

    let pick = if let Some(t) = target_gen {
        candidates
            .iter()
            .find(|c| c.generation_id == t)
            .cloned()
            .ok_or_else(|| Error::NotFound(format!("世代 {t} が見つからないか検証未合格です")))?
            .generation_id
    } else {
        // 「過去に実際に適用した世代」の履歴から最新を選ぶ。
        // 未適用の候補世代は対象外。applied_at が無い旧世代は作成時刻で近似。
        let mut best: Option<(&GenerationMeta, chrono::DateTime<Utc>)> = None;
        for m in candidates
            .iter()
            .filter(|c| Some(c.generation_id.as_str()) != current.as_deref())
        {
            let key = m.applied_at.unwrap_or(m.created_at);
            if best.as_ref().map(|(_, t)| key > *t).unwrap_or(true) {
                best = Some((m, key));
            }
        }
        best.map(|(m, _)| m.generation_id.clone())
            .ok_or_else(|| Error::NotFound("現行以外の適用済み世代がありません".into()))?
    };
    let meta = store.load_genmeta(&inst.id, &pick)?;

    match inst
        .recipe
        .installation
        .as_ref()
        .map(|s| s.strategy)
        .unwrap_or_default()
    {
        StrategyKind::Versioned => {
            store.set_active_generation(&inst.id, &pick)?;
            mark_applied(&store.layout.generation_dir(&inst.id, &pick));
        }
        StrategyKind::FixedPath => {
            // prepared を一時的に差し替えて fixed-path 適用を再利用
            let mut st = store.load_state(&inst.id)?;
            st.prepared_generation = Some(pick.clone());
            apply_fixed_path(store, inst, &mut st)?;
        }
    }

    let mut st = store.load_state(&inst.id)?;
    st.current_version = Some(meta.version.clone());
    st.active_generation = Some(pick.clone());
    st.status = Some(Status::UpToDate);
    store.save_state(&inst.id, &st)?;
    store.push_event(&inst.id, "rollback", &format!("復元: {}", meta.version));
    crate::dlog!("rollback: {} → v{} (gen {})", inst.name, meta.version, pick);
    Ok(meta.version)
}

/// 世代一覧 (メタ付き、新しい順)
pub fn list_generations(store: &Store, installation_id: &str) -> Result<Vec<GenerationMeta>> {
    let base = store.layout.generations_base(installation_id);
    let mut metas = vec![];
    if base.exists() {
        for e in fs::read_dir(&base)? {
            let p = e?.path();
            if p.is_dir() {
                if let Ok(m) = paths::read_json::<GenerationMeta>(&p.join("genmeta.json")) {
                    metas.push(m);
                }
            }
        }
    }
    metas.sort_by(|a, b| b.generation_id.cmp(&a.generation_id));
    Ok(metas)
}

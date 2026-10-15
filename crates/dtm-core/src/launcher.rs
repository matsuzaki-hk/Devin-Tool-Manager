//! ランチャー (仕様 §3-3,4, §9)。
//! - `dtm launch <ref> -- <args>`: 確定した世代のツールへ引数・stdin/out/err・
//!   終了コードをそのまま引き継ぐ
//! - 起動直前は「適用」のみ (取得/検証は事前実行済み)
//! - stdout に管理ログを混入させない (受け入れ条件 §15-9)
//! - 起動引数の本文はログへ記録しない (秘密値混入防止)
//! - 復旧確認・世代解決・spawn は同一ロックの内側で行い、
//!   ロックを取れない間に適用が進行する競合を排除する

use crate::apply;
use crate::error::{Error, Result};
use crate::lock::InstallationLock;
use crate::models::{ApplyWhen, Installation, ManagedEntry, StrategyKind};
use crate::stage;
use crate::store::Store;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::Duration;

/// 起動対象を解決し子プロセスを spawn する。
/// 戻り値は子プロセスの ExitStatus。呼び出し側はそのコードで終了する。
///
/// ref 形式:
///   - installationId / 名前  (直接指定)
///   - "mcp:<serverName>"    (後方互換: 管理マップの一意一致のみ)
///   - "mcpkey:<usageKey>"   (管理マップの一意キー。複数設定の同名衝突を防止)
pub fn launch(store: &Store, target_ref: &str, tool_args: &[String]) -> Result<ExitStatus> {
    let (inst, managed) = resolve_target(store, target_ref)?;
    crate::dlog!("launch: {} ({})", inst.name, inst.id);

    let strategy = inst
        .recipe
        .installation
        .as_ref()
        .map(|s| s.strategy)
        .unwrap_or_default();

    // 適用ロックを先に取得し、復旧確認→保留適用→世代解決→spawn まで
    // 同一ロック内で完結させる (ロック解放〜spawn の間に他プロセスの apply が
    // 挟まる競合を防ぐ。spawn はプロセス生成のみで子の終了待ちは含まない)。
    //
    // ロックが取れない場合:
    //   - fixed-path: 配置ファイルが差し替え中かもしれず旧版が安定して存在
    //     しない保証がないため、起動せずエラー (タイムアウトを起動根拠にしない)
    //   - versioned: 世代は immutable でポインタ切替は原子的なので
    //     有効世代のまま起動してよい (仕様 §6-8 初回適用時間制限)
    let lock =
        match InstallationLock::acquire_timeout(&store.layout, &inst.id, Duration::from_secs(10)) {
            Ok(l) => Some(l),
            Err(Error::Busy(m)) => {
                if strategy == StrategyKind::FixedPath {
                    return Err(Error::Busy(format!(
                        "{m} — 更新処理が進行中のため起動できません。完了後に再試行してください"
                    )));
                }
                crate::dlogw!("適用ロック取得失敗、現行有効世代で起動: {m}");
                None
            }
            Err(e) => return Err(e),
        };
    let _lock = lock.as_ref(); // Drop で解放

    // ロック内: 中断した差し替えの復旧確認。復旧失敗は起動させない。
    if lock.is_some() && strategy == StrategyKind::FixedPath {
        let install_dir = inst
            .fixed_path
            .clone()
            .unwrap_or_else(|| inst.install_root.clone());
        apply::recover_pending_swap(&install_dir).map_err(|e| {
            Error::Other(format!("中断した更新の復旧に失敗したため起動を中止: {e}"))
        })?;
    }

    // ロック内: prepared 世代があれば起動境界で適用
    if lock.is_some() {
        apply_pending_at_launch(store, &inst)?;
        // 実行中ロックで予約された委譲更新も起動境界で実行
        // (この時点では対象は未起動のためファイルロックは無い。
        //  失敗しても現行版で起動継続するため伝播しない)
        crate::ops::run_pending_pm_sync(store, &inst)?;
    }

    // ロック内: 起動ファイル解決 + spawn。
    // spawn 完了後にロックを解放しても、子プロセスは EXE の実行イメージを
    // OS が保持するため以後の差し替えは Windows のファイルロックで保護される。
    let exe = resolve_entrypoint(store, &inst)?;
    crate::dlog!("spawn: {}", exe.display());
    let mut cmd = Command::new(&exe);
    cmd.args(tool_args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    // MCP 等、コンソールの無い呼び出し元からの起動では子の新規
    // コンソールウィンドウが点滅するため抑制する。対話的なCLI利用
    // (親がコンソール保持) では子にそのコンソールを継承させる。
    #[cfg(windows)]
    if !crate::procutil::parent_has_console() {
        crate::procutil::hide_window(&mut cmd);
    }
    // 利用元 (MCP 設定) が明示する env / cwd のみ適用。
    // cwd 未指定は呼び出し元の作業ディレクトリを継承する
    // (EXE の親ディレクトリへの暗黙変更はしない。RTK 経由の git 等が
    //  プロジェクト基準で動くための既定動作)
    if let Some(m) = &managed {
        for (k, v) in &m.original.env {
            cmd.env(k, v);
        }
        if let Some(cwd) = &m.original.cwd {
            cmd.current_dir(cwd);
        }
    }
    let mut child: Child = cmd
        .spawn()
        .map_err(|e| Error::Other(format!("起動失敗 {}: {e}", exe.display())))?;
    drop(lock); // spawn 完了でロック解放
    let status = child
        .wait()
        .map_err(|e| Error::Other(format!("待機失敗 {}: {e}", exe.display())))?;
    crate::dlog!("exit: {} code={:?}", exe.display(), status.code());
    Ok(status)
}

/// 起動対象を解決する。利用元 (MCP 管理エントリ) があれば同時に返す。
fn resolve_target(store: &Store, target_ref: &str) -> Result<(Installation, Option<ManagedEntry>)> {
    if let Some(key) = target_ref.strip_prefix("mcpkey:") {
        let map = store.load_managed_map()?;
        let entry = map
            .entries
            .iter()
            .find(|e| e.usage_key == key)
            .ok_or_else(|| {
                Error::NotFound(format!("管理対象 MCP 利用元 '{key}' が見つかりません"))
            })?;
        return Ok((
            store.find_installation(&entry.installation_id)?,
            Some(entry.clone()),
        ));
    }
    if let Some(name) = target_ref.strip_prefix("mcp:") {
        let map = store.load_managed_map()?;
        let matches: Vec<&ManagedEntry> = map
            .entries
            .iter()
            .filter(|e| e.server_name == name)
            .collect();
        match matches.len() {
            0 => {
                return Err(Error::NotFound(format!(
                    "管理対象 MCP サーバー '{name}' が見つかりません"
                )))
            }
            1 => {
                let entry = matches[0];
                return Ok((
                    store.find_installation(&entry.installation_id)?,
                    Some(entry.clone()),
                ));
            }
            _ => {
                return Err(Error::NeedUser(format!(
                "管理対象 '{name}' が複数あります。mcpkey:<key> で一意指定してください (キー: {})",
                matches
                    .iter()
                    .map(|e| e.usage_key.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )))
            }
        }
    }
    if let Some(rest) = target_ref.strip_prefix("cli:") {
        return Ok((store.find_installation(rest)?, None));
    }
    Ok((store.find_installation(target_ref)?, None))
}

/// prepared 世代があれば起動境界で適用する (呼び出し側がロック取得済み)。
/// - apply_when=next-launch のみ。manual/all-windows-closed はここでは適用しない。
/// - 適用失敗時は現行版で継続 (旧版が保持されていることを前提)。
fn apply_pending_at_launch(store: &Store, inst: &Installation) -> Result<()> {
    let st = store.load_state(&inst.id)?;
    if st.prepared_generation.is_none() {
        return Ok(());
    }
    let apply_when = inst
        .recipe
        .installation
        .as_ref()
        .map(|s| s.apply_when)
        .unwrap_or_default();
    if apply_when != ApplyWhen::NextLaunch {
        return Ok(());
    }
    let mut st = store.load_state(&inst.id)?;
    if st.prepared_generation.is_none() {
        return Ok(()); // 他プロセスが適用済み
    }
    let r = match inst
        .recipe
        .installation
        .as_ref()
        .map(|s| s.strategy)
        .unwrap_or_default()
    {
        StrategyKind::Versioned => apply::activate_versioned(store, inst, &mut st),
        StrategyKind::FixedPath => apply::apply_fixed_path(store, inst, &mut st),
    };
    if let Err(e) = r {
        // 旧版を保持して起動継続 (仕様 §6-8)。
        // ただし復旧自体に失敗した場合は起動を中止する (呼び出し側へ伝播)。
        if e.to_string().contains("復旧も失敗") {
            return Err(e);
        }
        crate::dloge!("起動時適用失敗、現行版で継続: {e}");
    }
    Ok(())
}

/// 有効世代の起動ファイルを返す。
/// versioned: generations/<id>/<gen>/<entrypoint>
/// fixed-path: <fixed_path 親dir>/<entrypoint>  (差し替え後の実ファイル)
fn resolve_entrypoint(store: &Store, inst: &Installation) -> Result<PathBuf> {
    let art = inst
        .recipe
        .artifact
        .as_ref()
        .ok_or_else(|| Error::Config("artifact 未定義".into()))?;
    let entry_rel = art.entrypoint.replace('\\', "/");

    let strategy = inst
        .recipe
        .installation
        .as_ref()
        .map(|s| s.strategy)
        .unwrap_or_default();

    let exe = match strategy {
        StrategyKind::Versioned => {
            let gen = store.active_generation(&inst.id)?.ok_or_else(|| {
                Error::Config("有効世代がありません (初回 prepare/apply 前)".into())
            })?;
            store.layout.generation_dir(&inst.id, &gen).join(&entry_rel)
        }
        StrategyKind::FixedPath => {
            // fixed_path は「配置先ディレクトリまたはEXE実パス」のどちらも許す
            let fp = inst
                .fixed_path
                .clone()
                .unwrap_or_else(|| inst.install_root.clone());
            if fp.extension().map(|e| e == "exe").unwrap_or(false) && fp.is_file() {
                fp
            } else {
                fp.join(&entry_rel)
            }
        }
    };

    if !exe.exists() {
        return Err(Error::NotFound(format!(
            "起動ファイルが存在しません: {}",
            exe.display()
        )));
    }
    Ok(exe)
}

/// ツール実パスだけを解決 (spawn しない。診断用)
pub fn resolve_path_only(store: &Store, target_ref: &str) -> Result<PathBuf> {
    let (inst, _m) = resolve_target(store, target_ref)?;
    resolve_entrypoint(store, &inst)
}

/// 版確認などに使う簡易実行 (非破壊的のみ)。
/// 版確認の cwd は entrypoint 親に固定 (通常実行の cwd とは分離)。
pub fn probe(store: &Store, inst: &Installation) -> Result<Option<String>> {
    let verify = match &inst.recipe.verify {
        Some(v) => v,
        None => return Ok(None),
    };
    let exe = resolve_entrypoint(store, inst)?;
    stage::probe_version(&exe, verify)
}

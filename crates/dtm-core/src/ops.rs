//! 更新オペレーション (仕様 §6)。
//! check → prepare → apply の各段階をロック+ジョブ状態で集約し、
//! 途中失敗しても現行版を変更しない。

use crate::apply;
use crate::error::{Error, Result};
use crate::fetch;
use crate::github::{GithubClient, Release};
use crate::lock::{self, InstallationLock, JobGuard};
use crate::models::*;
use crate::paths::{self, Layout};
use crate::stage;
use crate::store::Store;
use crate::version::{self, CompareResult};
use crate::zipx;
use chrono::Utc;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// 版表記の正規化 (先頭の v/V を除去して "1.2.3" と "v1.2.3" を同一視)
fn norm_ver(s: &str) -> &str {
    s.trim().trim_start_matches(['v', 'V'])
}

/// ポリシー考慮のリリース選択 (R7: 版固定を check/prepare 双方で強制)。
/// pin 指定時はタグ/版が一致するリリースのみ。見つからなければ NeedUser。
fn select_release_for_policy<'a>(
    releases: &'a [Release],
    provider: &GithubReleaseProvider,
    pin: Option<&str>,
) -> Result<&'a Release> {
    if let Some(pin) = pin {
        let want = norm_ver(pin);
        return releases
            .iter()
            .find(|r| !r.draft && norm_ver(&r.tag_name) == want)
            .ok_or_else(|| {
                Error::NeedUser(format!(
                    "版固定 '{pin}' に一致するリリースが見つかりません。固定を解除するか版を見直してください"
                ))
            });
    }
    GithubClient::select_release(releases, provider).ok_or_else(|| {
        Error::NotFound(format!(
            "条件に合うリリースがありません: {}",
            provider.repository
        ))
    })
}

/// apply 時の版固定ゲート (R7): pin 版と準備済み世代の版が一致しない場合は拒否。
fn enforce_pin_on_apply(inst: &Installation, meta_version: &str) -> Result<()> {
    if let Some(pin) = inst
        .recipe
        .policy
        .as_ref()
        .and_then(|p| p.pinned_version.as_deref())
    {
        if norm_ver(pin) != norm_ver(meta_version) {
            return Err(Error::Conflict(format!(
                "版固定 {pin} のため版 {meta_version} は適用できません。pin を解除するか {pin} を準備してください"
            )));
        }
    }
    Ok(())
}

pub struct CheckOutcome {
    pub status: Status,
    pub candidate: Option<Candidate>,
    /// 共有ジョブを再利用した場合 true (仕様 §7: 重複照会防止)
    pub reused: bool,
}

/// 更新確認 (仕様 §6-1)。同一installationIdの確認は単一処理に集約。
pub fn check(store: &Store, inst: &Installation) -> Result<CheckOutcome> {
    let _lock = match InstallationLock::try_acquire(&store.layout, &inst.id) {
        Ok(l) => l,
        Err(Error::Busy(m)) => {
            // 進行中ジョブの完了を待ち結果を再利用 (仕様 §7)
            crate::dlog!("check 要求: {m} — 完了待機");
            let _j = crate::lock::wait_job(&store.layout, &inst.id, Duration::from_secs(120))?;
            let st = store.load_state(&inst.id)?;
            return Ok(CheckOutcome {
                status: st.status.unwrap_or(Status::Checking),
                candidate: st.latest_candidate,
                reused: true,
            });
            // wait_job の失敗は呼び出し側へ伝播
        }
        Err(e) => return Err(e),
    };
    let job = JobGuard::begin(&store.layout, &inst.id, "check")?;
    match check_inner(store, inst) {
        Ok(o) => {
            let _ = job.finish(&format!("{:?}", o.status));
            crate::dlog!(
                "check: {} → {:?} (候補: {})",
                inst.name,
                o.status,
                o.candidate
                    .as_ref()
                    .map(|c| c.version.as_str())
                    .unwrap_or("-")
            );
            Ok(o)
        }
        Err(e) => {
            let _ = job.fail(&e.to_string());
            // Checking のまま残さない (失敗理由は last_error へ)
            let mut st = store.load_state(&inst.id).unwrap_or_default();
            st.status = Some(Status::Failed);
            st.last_error = Some(e.to_string());
            let _ = store.save_state(&inst.id, &st);
            crate::dloge!("check: {} 失敗: {}", inst.name, e);
            Err(e)
        }
    }
}

fn check_inner(store: &Store, inst: &Installation) -> Result<CheckOutcome> {
    let mut st = store.load_state(&inst.id)?;
    // 現行版未取得なら確認操作を契機に検出を試行 (登録時点で失敗した場合の救済)
    if st.current_version.is_none() {
        let _ = detect_current_version(store, inst);
        st = store.load_state(&inst.id)?;
    }
    let recipe = &inst.recipe;

    let provider = match &recipe.update_provider {
        None => {
            st.status = Some(Status::SourceUnset);
            st.last_checked = Some(Utc::now());
            store.save_state(&inst.id, &st)?;
            return Ok(CheckOutcome {
                status: Status::SourceUnset,
                candidate: None,
                reused: false,
            });
        }
        Some(UpdateProvider::Manual) => {
            st.status = Some(Status::Unmanaged);
            st.last_checked = Some(Utc::now());
            store.save_state(&inst.id, &st)?;
            return Ok(CheckOutcome {
                status: Status::Unmanaged,
                candidate: None,
                reused: false,
            });
        }
        Some(UpdateProvider::PackageManager { manager, package }) => {
            // 外部委譲エントリ: 実体更新はしないが、外部最新かどうかは判定する。
            // インストール版 (uv env / npx キャッシュ / エントリプローブ) と
            // レジストリ最新 (PyPI/npm) を照合。更新実行は「外部更新」(pm-sync)。
            let installed = match manager.as_str() {
                "uv" => uvx_installed_version(package),
                "npm" => npm_installed_version(package),
                "uv-tool" => {
                    // shim は Python スタブでファイルプローブでは版を返さない。
                    // uv tool run で env 内版をプローブする (実行中でも可)
                    st.current_version = None;
                    let v = uvx_installed_version(package);
                    if let Some(v) = &v {
                        st.current_version = Some(v.clone());
                    }
                    v
                }
                _ => None,
            };
            let latest = registry_latest(manager, package);
            st.last_checked = Some(Utc::now());
            st.last_error = None;
            if let Some(v) = &installed {
                st.current_version = Some(v.clone());
            }
            if let Some(lv) = &latest {
                st.latest_candidate = Some(Candidate {
                    version: lv.clone(),
                    tag: lv.clone(),
                    release_url: String::new(),
                    asset_name: None,
                    asset_url: None,
                    incomparable: false,
                });
            }
            let status = match (&installed, &latest) {
                (Some(cur), Some(lv)) => match version::compare(VersionScheme::Semver, cur, lv) {
                    CompareResult::Newer => Status::Available,
                    _ => Status::UpToDate,
                },
                (None, Some(_)) => {
                    // インストール版は不明だが最新版は判明 → 候補表示のみ
                    if let Some(c) = st.latest_candidate.as_mut() {
                        c.incomparable = true;
                    }
                    Status::Available
                }
                _ => Status::Unmanaged,
            };
            st.status = Some(status);
            store.save_state(&inst.id, &st)?;
            return Ok(CheckOutcome {
                status,
                candidate: st.latest_candidate.clone(),
                reused: false,
            });
        }
        Some(UpdateProvider::GitBuild(p)) => {
            // git ソースビルド型: fetch した origin/<branch> の版表現
            // (<commit日付>.<短縮sha>) と現行版を照合する。
            // 現行は適用済み世代の版。未適用なら作業ツリー HEAD を近似値に使う
            // (dist/ はそのツリー由来と仮定)。
            let remote = git_remote_version(p).inspect_err(|e| {
                st.last_error = Some(e.to_string());
                let _ = store.save_state(&inst.id, &st);
            })?;
            let installed = st
                .current_version
                .clone()
                .or_else(|| git_rev_version(&p.work_dir, "HEAD").ok());
            st.last_checked = Some(Utc::now());
            st.last_error = None;
            if let Some(v) = &installed {
                st.current_version = Some(v.clone());
            }
            st.latest_candidate = Some(Candidate {
                version: remote.clone(),
                tag: remote.clone(),
                release_url: p.repository.clone(),
                asset_name: None,
                asset_url: None,
                incomparable: installed.is_none(),
            });
            let status = match &installed {
                Some(cur) if *cur == remote => Status::UpToDate,
                _ => Status::Available,
            };
            st.status = Some(status);
            store.save_state(&inst.id, &st)?;
            return Ok(CheckOutcome {
                status,
                candidate: st.latest_candidate.clone(),
                reused: false,
            });
        }
        Some(UpdateProvider::NpmRegistry(p)) => {
            // npm レジストリ取得型: packument から dist-tags.latest (または版固定)
            // の tarball を候補とする。PM 委譲とは別経路 (仕様 §5.2)。
            let pin = recipe
                .policy
                .as_ref()
                .and_then(|pp| pp.pinned_version.clone());
            // ボディ読み取りのソケット停滞は deadline で遮断 (releases() と同型)
            let base = npm_registry_base(p);
            let package = p.package.clone();
            let pack = fetch::bounded(
                "npm packument 取得",
                Instant::now() + Duration::from_secs(120),
                move || npm_packument(&base, &package, Duration::from_secs(30)),
            )?;
            let sel = npm_select_version(p, &pack, pin.as_deref())?;
            let candidate = Candidate {
                version: sel.version.clone(),
                tag: sel.version.clone(),
                release_url: sel.tarball.clone(),
                asset_name: sel.tarball.rsplit('/').next().map(String::from),
                asset_url: Some(sel.tarball.clone()),
                incomparable: false,
            };
            st.last_checked = Some(Utc::now());
            st.latest_candidate = Some(candidate.clone());
            match &st.current_version {
                None => {
                    st.status = Some(Status::Available);
                }
                Some(cur) => match version::compare(p.version_scheme, cur, &candidate.version) {
                    CompareResult::Newer => st.status = Some(Status::Available),
                    CompareResult::Equal | CompareResult::Older => {
                        st.status = Some(Status::UpToDate)
                    }
                    CompareResult::Incomparable => {
                        st.status = Some(Status::Available);
                        st.latest_candidate.as_mut().unwrap().incomparable = true;
                    }
                },
            }
            st.last_error = None;
            store.save_state(&inst.id, &st)?;
            return Ok(CheckOutcome {
                status: st.status.unwrap_or(Status::Available),
                candidate: Some(candidate),
                reused: false,
            });
        }
        Some(UpdateProvider::GithubRelease(p)) => p.clone(),
    };

    st.status = Some(Status::Checking);
    store.save_state(&inst.id, &st)?;

    let client = GithubClient::new()?;
    let releases = client.releases(&provider.repository, 30)?;
    // R7: 版固定があれば候補を固定版へ制約 (見つからなければ NeedUser)
    let pin = recipe
        .policy
        .as_ref()
        .and_then(|p| p.pinned_version.clone());
    let release = select_release_for_policy(&releases, &provider, pin.as_deref())?;
    let asset = GithubClient::select_asset(release, &provider)?;

    let candidate = Candidate {
        version: release.tag_name.trim_start_matches(['v', 'V']).to_string(),
        tag: release.tag_name.clone(),
        release_url: release.html_url.clone(),
        asset_name: Some(asset.name.clone()),
        asset_url: Some(asset.browser_download_url.clone()),
        incomparable: false,
    };

    st.last_checked = Some(Utc::now());
    st.latest_candidate = Some(candidate.clone());

    // 版比較 (仕様 §5.1: 辞書順比較禁止、不能なら候補表示のみ)
    match &st.current_version {
        None => {
            st.status = Some(Status::Available);
        }
        Some(cur) => {
            let scheme = provider.version_scheme;
            match version::compare(scheme, cur, &candidate.version) {
                CompareResult::Newer => st.status = Some(Status::Available),
                CompareResult::Equal | CompareResult::Older => st.status = Some(Status::UpToDate),
                CompareResult::Incomparable => {
                    st.status = Some(Status::Available);
                    st.latest_candidate.as_mut().unwrap().incomparable = true;
                }
            }
        }
    }
    st.last_error = None;
    store.save_state(&inst.id, &st)?;
    Ok(CheckOutcome {
        status: st.status.unwrap_or(Status::Available),
        candidate: Some(candidate),
        reused: false,
    })
}

pub struct PrepareOutcome {
    pub generation_id: String,
    pub version: String,
    pub verification: VerificationResult,
    pub probed_version: Option<String>,
}

/// 更新準備 (仕様 §6-2〜7)。検証に合格した世代のみ「適用待ち」にする。
/// 逐次ネットワーク呼び出し (取得+DL+検証探索) の合計に内部上限を設け、
/// 呼び出し側の execFile 300s kill より先に自分で失敗終了する
/// (kill されると進行中ステータス/ジョブが残留するため)。
pub fn prepare(store: &Store, inst: &Installation) -> Result<PrepareOutcome> {
    let _lock =
        InstallationLock::acquire_timeout(&store.layout, &inst.id, Duration::from_secs(300))?;
    let job = JobGuard::begin(&store.layout, &inst.id, "prepare")?;
    // 内部上限は呼び出し側の execFile kill より必ず先に切れる範囲
    // (拡張 prepare タイムアウト 900s に対し余裕を持って先に失敗終了)。
    // 大容量アセット×低速回線でも「中断残留」にならないことが目的。
    let deadline = Instant::now() + Duration::from_secs(780);
    let res = prepare_inner(store, inst, None, deadline);
    // 進捗ファイルは read 側が job=running でゲートするが、正常終了後も消しておく
    crate::lock::clear_progress(&store.layout, &inst.id);
    match res {
        Ok(o) => {
            let _ = job.finish(&format!("{} {}", o.version, o.generation_id));
            crate::dlog!(
                "prepare: {} v{} → 適用待ち (gen {}, 検証: {})",
                inst.name,
                o.version,
                o.generation_id,
                o.verification.status
            );
            Ok(o)
        }
        Err(e) => {
            let _ = job.fail(&e.to_string());
            crate::dloge!("prepare: {} 失敗: {}", inst.name, e);
            let mut st = store.load_state(&inst.id).unwrap_or_default();
            st.status = Some(Status::Failed);
            st.last_error = Some(e.to_string());
            let _ = store.save_state(&inst.id, &st);
            Err(e)
        }
    }
}

/// 逐次呼び出し境界でのデッドライン検査。超過なら Timeout で失敗終了する。
fn check_deadline(deadline: Instant) -> Result<()> {
    if Instant::now() > deadline {
        return Err(Error::Timeout(
            "準備処理が内部制限時間を超過しました（通信状態を確認して再試行してください）".into(),
        ));
    }
    Ok(())
}

fn prepare_inner(
    store: &Store,
    inst: &Installation,
    local_import: Option<(&Path, &str)>, // (file, version)
    deadline: Instant,
) -> Result<PrepareOutcome> {
    let mut st = store.load_state(&inst.id)?;
    let recipe = &inst.recipe;

    // 結果再利用 (仕様 §7): 同版が既に「適用待ち」で世代も完全なら二重取得しない。
    // 最新候補が準備済み版より新しい場合は再利用しない。
    if local_import.is_none() && st.status == Some(Status::Ready) {
        let current_ready = match (&st.prepared_version, st.latest_version()) {
            (Some(pv), Some(lv)) => pv == lv,
            (Some(_), None) => true,
            _ => false,
        };
        if current_ready {
            if let Some(pg) = &st.prepared_generation {
                let dir = store.layout.generation_dir(&inst.id, pg);
                if dir.join("genmeta.json").exists() {
                    if let Ok(m) = store.load_genmeta(&inst.id, pg) {
                        crate::dlog!("prepare 再利用: {} {}", inst.id, pg);
                        return Ok(PrepareOutcome {
                            generation_id: pg.clone(),
                            version: m.version,
                            verification: m.verification,
                            probed_version: m.probed_version,
                        });
                    }
                }
            }
        }
    }

    st.status = Some(Status::Preparing);
    store.save_state(&inst.id, &st)?;

    // 進捗レポーター: DL ワーカースレッドからも書けるよう Arc 共有。
    // 表示側は job=running の間だけこのファイルを有効とみなす。
    let progress = std::sync::Arc::new(lock::Progress::new(&store.layout, &inst.id));

    // 過去に強制終了した prepare/import が残した同 installation の
    // staging 残骸を回収 (ロック保持中のため他の作業者は存在しない)。
    if let Ok(rd) = std::fs::read_dir(store.layout.staging_dir()) {
        let prefix = format!("{}-", inst.id);
        for e in rd.flatten() {
            if e.file_name().to_string_lossy().starts_with(&prefix) {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
    }
    let staging = store
        .layout
        .staging_dir()
        .join(format!("{}-{}", inst.id, std::process::id()));
    if staging.exists() {
        std::fs::remove_dir_all(&staging)?;
    }
    std::fs::create_dir_all(&staging)?;
    let cleanup = |staging: &Path| {
        let _ = std::fs::remove_dir_all(staging);
    };

    // 取得 + 配布元検証
    let (outcome, version, source_kind, release_url, asset_name) = match local_import {
        Some((file, ver)) => {
            // R7: 手動取込も版固定の制約を受ける (固定版と異なる取込は拒否)
            if let Some(pin) = recipe
                .policy
                .as_ref()
                .and_then(|p| p.pinned_version.as_deref())
            {
                if norm_ver(pin) != norm_ver(ver) {
                    cleanup(&staging);
                    return Err(Error::Conflict(format!(
                        "版固定 {pin} のため版 {ver} の取り込みはできません"
                    )));
                }
            }
            let o = fetch::import_local_file(file, &staging)?;
            (
                o,
                ver.to_string(),
                "manual-import".to_string(),
                None,
                file.file_name().map(|n| n.to_string_lossy().to_string()),
            )
        }
        None => match &recipe.update_provider {
            Some(UpdateProvider::GithubRelease(provider)) => {
                progress.phase("release");
                let client = GithubClient::new()?;
                let releases = client.releases_bounded(&provider.repository, 30, deadline)?;
                check_deadline(deadline)?;
                let pin = recipe
                    .policy
                    .as_ref()
                    .and_then(|p| p.pinned_version.clone());
                let release = select_release_for_policy(&releases, provider, pin.as_deref())?;
                let asset = GithubClient::select_asset(release, provider)?;
                let mut o = fetch::download(
                    &client,
                    &asset.browser_download_url,
                    &staging,
                    &asset.name,
                    deadline,
                    Some(progress.clone()),
                )?;
                check_deadline(deadline)?;
                progress.phase("verify");
                fetch::verify_checksum(&client, release, asset, &mut o, &staging, deadline)?;
                (
                    o,
                    release.tag_name.trim_start_matches(['v', 'V']).to_string(),
                    "github-release".to_string(),
                    Some(release.html_url.clone()),
                    Some(asset.name.clone()),
                )
            }
            Some(UpdateProvider::GitBuild(p)) => match git_build_fetch(p, &staging) {
                Ok((o, ver)) => (
                    o,
                    ver,
                    "build".to_string(),
                    Some(p.repository.clone()),
                    None,
                ),
                Err(e) => {
                    cleanup(&staging);
                    return Err(e);
                }
            },
            Some(UpdateProvider::NpmRegistry(p)) => {
                progress.phase("release");
                let base = npm_registry_base(p);
                let package = p.package.clone();
                let pack = match fetch::bounded("npm packument 取得", deadline, move || {
                    npm_packument(&base, &package, Duration::from_secs(30))
                }) {
                    Ok(v) => v,
                    Err(e) => {
                        cleanup(&staging);
                        return Err(e);
                    }
                };
                check_deadline(deadline)?;
                let pin = recipe
                    .policy
                    .as_ref()
                    .and_then(|pp| pp.pinned_version.clone());
                let sel = match npm_select_version(p, &pack, pin.as_deref()) {
                    Ok(s) => s,
                    Err(e) => {
                        cleanup(&staging);
                        return Err(e);
                    }
                };
                let name = sel
                    .tarball
                    .rsplit('/')
                    .next()
                    .unwrap_or("package.tgz")
                    .to_string();
                let http = npm_http_client()?;
                let mut o = fetch::download_http(
                    &http,
                    &sel.tarball,
                    &staging,
                    &name,
                    deadline,
                    Some(progress.clone()),
                )?;
                check_deadline(deadline)?;
                progress.phase("verify");
                if let Err(e) =
                    verify_npm_integrity(&mut o, sel.integrity.as_deref(), sel.shasum.as_deref())
                {
                    cleanup(&staging);
                    return Err(e);
                }
                (
                    o,
                    sel.version,
                    "npm-registry".to_string(),
                    Some(sel.tarball),
                    Some(name),
                )
            }
            _ => {
                cleanup(&staging);
                return Err(Error::Config(
                    "github-release / git-build / npm-registry プロバイダー未設定 (import を使用)"
                        .into(),
                ));
            }
        },
    };

    // 展開 (仕様 §6-4 安全検査込み)
    progress.phase("extract");
    let art = recipe
        .artifact
        .clone()
        .ok_or_else(|| Error::Config("artifact 未定義".into()))?;
    let extract_root = staging.join("extract");
    let single_file = match art.kind {
        ArtifactKind::Exe => {
            // 実体が ZIP なら ZIP 扱い (形式推測は内容で判定)
            if zipx::is_zip(&outcome.file) {
                zipx::extract(&outcome.file, &extract_root, &zipx::ZipLimits::default())?;
                None
            } else {
                let name = outcome
                    .file
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "tool.exe".into());
                let dest = extract_root.join(&name);
                std::fs::create_dir_all(&extract_root)?;
                std::fs::copy(&outcome.file, &dest)?;
                Some(name)
            }
        }
        ArtifactKind::Zip => {
            if !zipx::is_zip(&outcome.file) {
                cleanup(&staging);
                return Err(Error::Unsafe(
                    "ZIP として取得したが内容が ZIP 形式ではありません".into(),
                ));
            }
            zipx::extract(&outcome.file, &extract_root, &zipx::ZipLimits::default())?;
            None
        }
        ArtifactKind::TarGz => {
            if !crate::tarx::is_targz(&outcome.file) {
                cleanup(&staging);
                return Err(Error::Unsafe(
                    "tar.gz として取得したが内容が tar.gz 形式ではありません (artifact.kind を確認してください)".into(),
                ));
            }
            crate::tarx::extract(
                &outcome.file,
                &extract_root,
                &crate::tarx::TarLimits::default(),
            )?;
            None
        }
        ArtifactKind::Folder => {
            // ローカルフォルダ取込: outcome.file はフォルダ内ファイルではなく
            // import_local_file が返すコピー済みファイル。フォルダ指定は CLI が
            // 直接 extract_root へコピーする経路にする
            cleanup(&staging);
            return Err(Error::Config(
                "Folder 取込は import コマンドのフォルダ指定を使用してください".into(),
            ));
        }
    };

    // 配置計画 → 世代化
    let plan = stage::plan_placement(recipe, &extract_root, single_file.as_deref())?;
    let source = GenerationSource {
        kind: source_kind,
        release_url,
        asset_name,
        sha256: outcome.sha256.clone(),
        imported_from: local_import.map(|(p, _)| p.to_path_buf()),
    };
    let (gen_id, gen_dir) = stage::materialize_generation(
        &store.layout,
        recipe,
        &plan,
        &version,
        source,
        outcome.verification.clone(),
        &staging,
    )?;

    // 世代内インストール手順 (npm tarball 等、実行時依存が成果物に
    // 同梱されない場合の補助。git-build の buildCommands と同型で
    // 世代ルートを cwd に順に実行する)
    if let Some(UpdateProvider::NpmRegistry(p)) = &recipe.update_provider {
        for cmdline in &p.install_commands {
            crate::dlog!("npm-install: 実行 `{cmdline}` in {}", gen_dir.display());
            let mut c = std::process::Command::new(if cfg!(windows) { "cmd" } else { "sh" });
            if cfg!(windows) {
                c.args(["/c", cmdline]);
            } else {
                c.args(["-c", cmdline]);
            }
            c.current_dir(&gen_dir);
            let remaining = deadline.saturating_duration_since(Instant::now());
            let out = match run_with_timeout(c, remaining.min(Duration::from_secs(600)), cmdline) {
                Ok(o) => o,
                Err(e) => {
                    store.push_event(&inst.id, "install-failed", &e.to_string());
                    cleanup(&staging);
                    return Err(e);
                }
            };
            if !out.status.success() {
                let tail = String::from_utf8_lossy(&out.stderr)
                    .lines()
                    .rev()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or("")
                    .to_string();
                let e = Error::Config(format!("インストールコマンド失敗 `{cmdline}`: {tail}"));
                store.push_event(&inst.id, "install-failed", &e.to_string());
                cleanup(&staging);
                return Err(e);
            }
        }
    }

    // 版確認 (仕様 §6-6: 登録された非破壊的確認のみ)
    progress.phase("probe");
    let entry = gen_dir.join(&plan.entrypoint);
    let probed = if let Some(v) = &recipe.verify {
        match stage::probe_version(&entry, v) {
            // 登録コマンドは正常終了したが版出力が無い exe 向けに
            // 汎用フォールバック (他フラグ→PE FileVersion) へ継続
            Ok(x) => x.or_else(|| stage::probe_version_auto(&entry, None)),
            Err(e) => {
                // 版確認失敗 → 世代は残すが verified=false のまま
                // (ready にせず適用・復元の対象外。R8)
                store.push_event(&inst.id, "verify-failed", &e.to_string());
                cleanup(&staging);
                return Err(e);
            }
        }
    } else {
        None
    };
    // 検証結果を genmeta へ反映:
    // - プローブ合格、または verify 未登録 (検査不能のため合格扱い) → verified
    // - 未合格のまま残る世代は apply/rollback の対象外
    if let Ok(mut m) = store.load_genmeta(&inst.id, &gen_id) {
        m.probed_version = probed.clone();
        m.verified = recipe.verify.is_none() || probed.is_some();
        let _ = paths::atomic_write_json(&gen_dir.join("genmeta.json"), &m);
    }

    // 「適用待ち」保存 (仕様 §6-7)
    st.prepared_generation = Some(gen_id.clone());
    st.prepared_version = Some(version.clone());
    st.status = Some(Status::Ready);
    st.last_error = None;
    store.save_state(&inst.id, &st)?;
    store.push_event(
        &inst.id,
        "prepare",
        &format!(
            "準備完了: {version} ({gen_id}) 検証={}",
            outcome.verification.status
        ),
    );

    // 保持数整理 (現行+準備済みは保護)
    if let Some(s) = &recipe.installation {
        let cur = st.active_generation.clone();
        let _ = stage::prune_generations(
            &store.layout,
            &inst.id,
            s.keep_generations,
            cur.as_deref(),
            Some(gen_id.as_str()),
        );
    }

    cleanup(&staging);
    Ok(PrepareOutcome {
        generation_id: gen_id,
        version,
        verification: outcome.verification,
        probed_version: probed,
    })
}

/// 適用 (仕様 §6-8)。方式に応じた安全な起動境界でのみ呼ぶ。
pub fn apply(store: &Store, inst: &Installation) -> Result<()> {
    let _lock =
        InstallationLock::acquire_timeout(&store.layout, &inst.id, Duration::from_secs(60))?;
    let job = JobGuard::begin(&store.layout, &inst.id, "apply")?;
    let r = (|| {
        let mut st = store.load_state(&inst.id)?;
        // R7: 版固定があれば準備済み世代の版と一致するか事前確認
        if let Some(pg) = &st.prepared_generation {
            if let Ok(m) = store.load_genmeta(&inst.id, pg) {
                enforce_pin_on_apply(inst, &m.version)?;
            }
        }
        match inst
            .recipe
            .installation
            .as_ref()
            .map(|s| s.strategy)
            .unwrap_or_default()
        {
            StrategyKind::Versioned => apply::activate_versioned(store, inst, &mut st),
            StrategyKind::FixedPath => apply::apply_fixed_path(store, inst, &mut st),
        }
    })();
    match &r {
        Ok(()) => {
            let _ = job.finish("applied");
            let v = store
                .load_state(&inst.id)
                .ok()
                .and_then(|s| s.current_version)
                .unwrap_or_else(|| "?".into());
            crate::dlog!("apply: {} → v{}", inst.name, v);
        }
        Err(e) => {
            let _ = job.fail(&e.to_string());
            crate::dloge!("apply: {} 失敗: {}", inst.name, e);
        }
    }
    r
}

/// 手動成果物取込 (仕様 §5.2)。
pub fn import(
    store: &Store,
    inst: &Installation,
    file: &Path,
    version: &str,
) -> Result<PrepareOutcome> {
    let _lock =
        InstallationLock::acquire_timeout(&store.layout, &inst.id, Duration::from_secs(120))?;
    let job = JobGuard::begin(&store.layout, &inst.id, "import")?;
    // ローカル取込はネットワークを使わないが、共通処理の上限として同じ枠を渡す
    let deadline = Instant::now() + Duration::from_secs(180);
    match prepare_inner(store, inst, Some((file, version)), deadline) {
        Ok(o) => {
            let _ = job.finish(&format!("import {} {}", o.version, o.generation_id));
            crate::dlog!(
                "import: {} v{} (gen {}, 検証: {})",
                inst.name,
                o.version,
                o.generation_id,
                o.verification.status
            );
            Ok(o)
        }
        Err(e) => {
            let _ = job.fail(&e.to_string());
            Err(e)
        }
    }
}

/// 予約取消 (仕様 §8: 現在版を変更しない)
pub fn cancel_prepared(store: &Store, inst: &Installation) -> Result<()> {
    let _lock =
        InstallationLock::acquire_timeout(&store.layout, &inst.id, Duration::from_secs(30))?;
    let mut st = store.load_state(&inst.id)?;
    let had = st.prepared_generation.take();
    st.prepared_version = None;
    if st.status == Some(Status::Ready) {
        // 現行版と最新候補を比較して復帰先を決定 (同版なら UpToDate)
        let newer = match (&st.current_version, &st.latest_candidate) {
            (Some(cur), Some(cand)) => {
                let scheme = match &inst.recipe.update_provider {
                    Some(UpdateProvider::GithubRelease(p)) => p.version_scheme,
                    _ => VersionScheme::Semver,
                };
                matches!(
                    crate::version::compare(scheme, cur, &cand.version),
                    CompareResult::Newer
                )
            }
            (None, Some(_)) => true,
            _ => false,
        };
        st.status = Some(if newer {
            Status::Available
        } else {
            Status::UpToDate
        });
    }
    store.save_state(&inst.id, &st)?;
    if let Some(g) = had {
        store.push_event(&inst.id, "cancel", &format!("適用予約取消: {g}"));
        crate::dlog!("cancel: {} 適用予約取消 ({})", inst.name, g);
    }
    Ok(())
}

/// 版固定/解除
pub fn pin(store: &Store, inst: &Installation, version: Option<String>) -> Result<()> {
    store.update_registry(|reg| {
        let i = reg
            .installations
            .iter_mut()
            .find(|i| i.id == inst.id)
            .ok_or_else(|| Error::NotFound(inst.id.clone()))?;
        let p = i.recipe.policy.get_or_insert(Policy {
            prepare_automatically: false,
            pinned_version: None,
            check_interval_hours: 24,
            on_preparation_failure: OnFailure::KeepCurrent,
        });
        p.pinned_version = version.clone();
        i.updated_at = Some(Utc::now());
        Ok(())
    })?;
    store.push_event(
        &inst.id,
        "pin",
        &version
            .as_ref()
            .map(|v| format!("版固定: {v}"))
            .unwrap_or_else(|| "版固定解除".into()),
    );
    crate::dlog!(
        "pin: {} → {}",
        inst.name,
        version
            .as_ref()
            .map(|v| format!("固定 {v}"))
            .unwrap_or_else(|| "解除".into())
    );
    Ok(())
}

// Layout re-export for convenience of CLI
pub fn root_layout(cli_root: Option<&Path>) -> Result<Layout> {
    Layout::discover(cli_root)
}

/// 現行版を登録 verify コマンドで検出し state に記録 (登録直後の初回のみ)。
/// fixed-path では実ファイル、versioned では有効世代のファイルを対象とする。
pub fn detect_current_version(store: &Store, inst: &Installation) -> Result<Option<String>> {
    let mut st = store.load_state(&inst.id)?;
    if st.current_version.is_some() {
        return Ok(st.current_version);
    }
    let art = match &inst.recipe.artifact {
        Some(a) => a,
        None => return Ok(None),
    };
    // 実行ファイルパス: fixed-path → install_root+entrypoint、versioned → 有効世代
    let exe = match inst
        .recipe
        .installation
        .as_ref()
        .map(|s| s.strategy)
        .unwrap_or_default()
    {
        StrategyKind::FixedPath => inst.install_root.join(&art.entrypoint),
        StrategyKind::Versioned => match store.active_generation(&inst.id)? {
            Some(g) => store
                .layout
                .generation_dir(&inst.id, &g)
                .join(&art.entrypoint),
            None => inst.install_root.join(&art.entrypoint),
        },
    };
    if !exe.exists() {
        return Ok(None);
    }
    // verify コマンド失敗時も汎用フラグ/PEバージョンへフォールバック
    if let Some(v) = stage::probe_version_auto(&exe, inst.recipe.verify.as_ref()) {
        st.current_version = Some(v.clone());
        store.save_state(&inst.id, &st)?;
        store.push_event(&inst.id, "detect", &format!("現行版検出: {v}"));
        crate::dlog!("detect: {} → 現行版 {}", inst.name, v);
        return Ok(Some(v));
    }
    Ok(None)
}

/// 初期世代シード (versioned 方式のみ)。
/// 管理開始時点の既存ファイルを「世代0」として世代領域へ取り込み、
/// 有効ポインタを設定する。これにより初回適用前からランチャー経由で起動可能。
/// 版は verify プローブで検出、失敗時は "unknown"。
pub fn seed_initial_generation(store: &Store, inst: &Installation) -> Result<Option<String>> {
    let strategy = inst
        .recipe
        .installation
        .as_ref()
        .map(|s| s.strategy)
        .unwrap_or_default();
    if strategy != StrategyKind::Versioned {
        return Ok(None);
    }
    if store.active_generation(&inst.id)?.is_some() {
        return Ok(None); // 既に有効世代あり
    }
    let art = inst
        .recipe
        .artifact
        .as_ref()
        .ok_or_else(|| Error::Config("artifact 未定義".into()))?;
    let install_dir = inst.install_root.clone();
    let entry = install_dir.join(&art.entrypoint);
    if !entry.exists() {
        crate::dlogw!("初期世代スキップ: 起動ファイルなし {}", entry.display());
        return Ok(None);
    }
    // 版検出 (失敗しても seed は継続)
    let version = stage::probe_version_auto(&entry, inst.recipe.verify.as_ref())
        .unwrap_or_else(|| "unknown".into());

    // 現在の配置物をそのまま計画化。管理領域・バックアップは除外
    let mut recipe = inst.recipe.clone();
    if let Some(a) = recipe.artifact.as_mut() {
        a.exclude
            .extend(vec![".dtm-backup/**".into(), "genmeta.json".into()]);
        a.root_pattern = None; // 既存配置はルート直下
    }
    let staging =
        store
            .layout
            .staging_dir()
            .join(format!("seed-{}-{}", inst.id, std::process::id()));
    std::fs::create_dir_all(&staging)?;
    let plan = stage::plan_placement(&recipe, &install_dir, None)?;
    let (gen_id, gen_dir) = stage::materialize_generation(
        &store.layout,
        &recipe,
        &plan,
        &version,
        GenerationSource {
            kind: "seed".into(),
            release_url: None,
            asset_name: None,
            sha256: paths::sha256_file(&entry)?,
            imported_from: Some(install_dir.clone()),
        },
        VerificationResult {
            status: "unverified".into(),
            method: Some("initial-seed".into()),
            detail: Some("管理開始時の既存配置物".into()),
        },
        &staging,
    )?;
    let _ = std::fs::remove_dir_all(&staging);
    // seed 世代は「現在動作している実体」の写しなので適用済みとして刻む。
    // 版確認に成功していれば verified も立てる。
    if let Ok(mut m) = store.load_genmeta(&inst.id, &gen_id) {
        m.applied_at = Some(Utc::now());
        m.verified = version != "unknown";
        let _ = paths::atomic_write_json(&gen_dir.join("genmeta.json"), &m);
    }
    store.set_active_generation(&inst.id, &gen_id)?;
    let mut st = store.load_state(&inst.id)?;
    st.active_generation = Some(gen_id.clone());
    st.current_version = Some(version.clone());
    if st.status.is_none() {
        st.status = Some(Status::SourceUnset);
    }
    store.save_state(&inst.id, &st)?;
    store.push_event(&inst.id, "seed", &format!("初期世代: {version} ({gen_id})"));
    Ok(Some(gen_id))
}

/// 外部パッケージマネージャへの更新委譲 (委譲エントリ専用、仕様 §9)。
/// DTM は実体を直接更新しない。マネージャの正規コマンドを実行するだけ。
///
/// - uv-tool (uv tool install の shim): `uv tool upgrade <pkg>`
/// - uv (uvx): `uv tool run --refresh-package <pkg> --from <pkg> python -c pass`
///   → uvx キャッシュを再解決 (ツール本体は起動しない。`uv cache clean` は
///   稼働中 uvx がキャッシュロックを保持していると無限待機するため使わない)
/// - npm (npx): `npm exec --package <spec> -- node -e ...` → npx キャッシュを更新
///   (MCP サーバー本体は起動しない。spec に @latest/タグがあれば最新を再解決)
pub fn pm_sync(store: &Store, id_or_name: &str) -> Result<String> {
    let inst = store.find_installation(id_or_name)?;
    let (manager, package) = match &inst.recipe.update_provider {
        Some(UpdateProvider::PackageManager { manager, package }) => {
            (manager.clone(), package.clone())
        }
        _ => {
            return Err(Error::NeedUser(
                "外部パッケージ管理対象ではないため委譲更新できません".into(),
            ))
        }
    };
    if package.trim().is_empty() {
        return Err(Error::NeedUser("委譲先パッケージが不明です".into()));
    }
    // 実行前の検出版とレジストリ最新版を記録 (結果表示用)
    // uv-tool (インストール済みツール) も uv tool run で env 内版を
    // プローブできるため uv と同じ扱い (shim のファイルプローブは
    // Python スタブのため版を返さない)
    let before = match manager.as_str() {
        "uv" | "uv-tool" => uvx_installed_version(&package),
        "npm" => npm_installed_version(&package),
        _ => store
            .load_state(&inst.id)
            .ok()
            .and_then(|s| s.current_version),
    };
    let latest = registry_latest(&manager, &package); // ベストエフォート参照情報
    let (cmd, desc) = pm_command(&manager, &package)?;
    crate::dlog!("pm-sync: {} → {}", inst.name, desc);
    let out = run_with_timeout(cmd, Duration::from_secs(240), &desc)?;
    let stderr_tail = String::from_utf8_lossy(&out.stderr)
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .to_string();
    let mut st = store.load_state(&inst.id)?;
    if !out.status.success() {
        // 実行中ツールのファイルロック (os error 32) なら即失敗にせず
        // 「次回起動時適用」を予約する。launch 境界では対象が未起動のため
        // ロック無しで完了する。(os error 32 はロケール非依存の安定文字列)
        let combined = format!("{stderr_tail}\n{}", String::from_utf8_lossy(&out.stdout));
        if combined.contains("os error 32") {
            st.pending_pm_sync = Some(PendingPmSync {
                manager: manager.clone(),
                package: package.clone(),
                scheduled_at: Some(Utc::now()),
            });
            let msg = format!("対象ツール実行中のため次回起動時適用を予約 ({desc})");
            st.last_error = Some(msg.clone());
            st.last_checked = Some(Utc::now());
            let _ = store.save_state(&inst.id, &st);
            store.push_event(&inst.id, "pm-sync", "次回起動時適用を予約");
            crate::dlog!("pm-sync: {} {msg}", inst.name);
            return Ok(msg);
        }
        let msg = format!(
            "外部更新失敗 ({desc}): 終了コード {} {}",
            out.status.code().unwrap_or(-1),
            stderr_tail
        );
        st.last_error = Some(msg.clone());
        st.last_checked = Some(Utc::now());
        let _ = store.save_state(&inst.id, &st);
        store.push_event(&inst.id, "pm-sync", &format!("失敗: {desc}"));
        crate::dloge!("pm-sync: {} 失敗: {}", inst.name, msg);
        return Err(Error::NeedUser(msg));
    }
    st.pending_pm_sync = None;
    // 成功: 外部で版が変わった可能性があるため再検出
    st.last_error = None;
    st.last_checked = Some(Utc::now());
    st.current_version = None; // 強制再検出
    let _ = store.save_state(&inst.id, &st);
    store.push_event(&inst.id, "pm-sync", &format!("外部更新実行: {desc}"));
    crate::dlog!("pm-sync: {} 完了 ({})", inst.name, desc);
    // 実行後の版: uvx/npx はキャッシュ環境をプローブ、それ以外はエントリプローブ
    let after = match manager.as_str() {
        "uv" | "uv-tool" => uvx_installed_version(&package),
        "npm" => npm_installed_version(&package),
        _ => {
            let _ = detect_current_version(store, &inst);
            store
                .load_state(&inst.id)
                .ok()
                .and_then(|s| s.current_version)
        }
    };
    // 実行後の状態を反映 (旧 check 結果の候補が残ると実態と乖離するため
    // レジストリ最新・status もここで更新する)
    {
        let mut st2 = store.load_state(&inst.id)?;
        if let Some(v) = &after {
            st2.current_version = Some(v.clone());
        }
        if let Some(v) = &latest {
            st2.latest_candidate = Some(Candidate {
                version: v.clone(),
                tag: v.clone(),
                release_url: String::new(),
                asset_name: None,
                asset_url: None,
                incomparable: after.is_none(),
            });
        }
        st2.status = Some(match (&after, &latest) {
            (Some(cur), Some(lv)) => match version::compare(VersionScheme::Semver, cur, lv) {
                CompareResult::Newer => Status::Available,
                _ => Status::UpToDate,
            },
            (None, Some(_)) => Status::Available,
            _ => Status::Unmanaged,
        });
        let _ = store.save_state(&inst.id, &st2);
    }
    // 結果表示: 実行前版 → 実行後版 (+ レジストリ最新版)
    Ok(format!(
        "{desc} — 版: {} → {}{}",
        before.as_deref().unwrap_or("不明"),
        after.as_deref().unwrap_or("不明"),
        latest
            .map(|v| format!(" / レジストリ最新: {v}"))
            .unwrap_or_default()
    ))
}

/// 起動境界で予約された委譲更新を実行する (launcher から呼ぶ。ロック保持中前提)。
/// 対象ツールはまだ起動していないため shim 等のファイルロックは無い。
/// 成功時は予約を解除し版を再検出する。失敗時は予約を残したまま Ok を返し、
/// 現行版での起動を妨げない (起動側が Err を起動中止として扱わないよう
/// この関数自体は常に Ok を返す。失敗はイベントとログに残す)。
pub fn run_pending_pm_sync(store: &Store, inst: &Installation) -> Result<()> {
    let st = store.load_state(&inst.id)?;
    let Some(p) = st.pending_pm_sync.clone() else {
        return Ok(());
    };
    let (cmd, desc) = match pm_command(&p.manager, &p.package) {
        Ok(v) => v,
        Err(e) => {
            crate::dlogw!("pm-sync(launch): {} コマンド解決失敗: {e}", inst.name);
            return Ok(());
        }
    };
    crate::dlog!("pm-sync(launch): {} → {}", inst.name, desc);
    let applied = match run_with_timeout(cmd, Duration::from_secs(240), &desc) {
        Ok(out) if out.status.success() => true,
        Ok(out) => {
            let tail = String::from_utf8_lossy(&out.stderr)
                .lines()
                .rev()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("")
                .to_string();
            store.push_event(
                &inst.id,
                "pm-sync",
                &format!("起動時適用失敗 (次回再試行): {desc}"),
            );
            crate::dlogw!("pm-sync(launch): {} 失敗: {tail}", inst.name);
            false
        }
        Err(e) => {
            crate::dlogw!("pm-sync(launch): {} 実行失敗 (次回再試行): {e}", inst.name);
            false
        }
    };
    if applied {
        let mut st2 = store.load_state(&inst.id)?;
        st2.pending_pm_sync = None;
        st2.last_error = None;
        st2.last_checked = Some(Utc::now());
        st2.current_version = None; // 版は再検出させる
        let _ = store.save_state(&inst.id, &st2);
        store.push_event(&inst.id, "pm-sync", &format!("起動時適用完了: {desc}"));
        crate::dlog!("pm-sync(launch): {} 完了", inst.name);
    }
    Ok(())
}

/// 世代整理 (仕様 §13 の保持ポリシーを手動適用)。
/// 現行世代・適用待ち世代は保護。dry_run=true では削除せず候補のみ返す。
/// 戻り値: (世代名, 占有バイト数)。
pub fn cleanup_generations(
    store: &Store,
    inst: &Installation,
    keep: Option<u32>,
    dry_run: bool,
) -> Result<Vec<(String, u64)>> {
    // 実行中ジョブと競合しないよう非ブロッキングでロック (ビジーなら Busy エラー)
    let _lock = InstallationLock::try_acquire(&store.layout, &inst.id)?;
    let keep = keep
        .or_else(|| {
            inst.recipe
                .installation
                .as_ref()
                .map(|s| s.keep_generations)
        })
        .unwrap_or(3);
    let st = store.load_state(&inst.id).unwrap_or_default();
    let active = store
        .active_generation(&inst.id)?
        .or_else(|| st.active_generation.clone());
    let cands = stage::prune_candidates(
        &store.layout,
        &inst.id,
        keep,
        active.as_deref(),
        st.prepared_generation.as_deref(),
    )?;
    let mut out = vec![];
    for (name, path) in cands {
        let size = dir_size(&path);
        if !dry_run {
            std::fs::remove_dir_all(&path)?;
        }
        out.push((name, size));
    }
    if !dry_run && !out.is_empty() {
        store.push_event(
            &inst.id,
            "cleanup",
            &format!("世代整理: {} 件削除", out.len()),
        );
        crate::dlog!("cleanup: {} → {} 世代削除", inst.name, out.len());
    }
    Ok(out)
}

// ---------- 登録解除 ----------

pub struct RemoveOutcome {
    pub name: String,
    pub warnings: Vec<String>,
    /// 消去した managed-mcp エントリ数 (force 時のみ >0)
    pub removed_managed_entries: usize,
}

/// 登録解除: registry・状態・世代・管理マッピング・一時ファイルを削除する。
/// 実ファイル (fixed_path 元実体・install_root 配下のユーザーファイル) は削除しない。
/// `manage` で設定書き換え済みの MCP は Devin が `dtm launch mcpkey:` を呼び続けるため
/// `unmanage` 先行を要求する (--force で登録のみ強制削除 + 警告)。
pub fn remove(store: &Store, inst: &Installation, force: bool) -> Result<RemoveOutcome> {
    let lock = InstallationLock::try_acquire(&store.layout, &inst.id)?;

    let mut warnings: Vec<String> = Vec::new();
    let mut map = store.load_managed_map()?;
    let managed_names: Vec<String> = map
        .entries
        .iter()
        .filter(|e| e.installation_id == inst.id)
        .map(|e| e.server_name.clone())
        .collect();
    if !managed_names.is_empty() && !force {
        return Err(Error::Config(format!(
            "'{}' は管理化済み MCP ({}) です。先に `dtm unmanage` で設定を元に戻してください (登録だけ強制削除する場合は --force)",
            inst.name,
            managed_names.join(", ")
        )));
    }
    let mut removed_managed_entries = 0usize;
    if !managed_names.is_empty() {
        warnings.push(format!(
            "管理化済みの MCP 設定 ({}) は `dtm launch mcpkey:` を指したまま残ります。Devin 側の起動は失敗するため、設定の復元または `dtm unmanage` が別途必要です",
            managed_names.join(", ")
        ));
        removed_managed_entries = managed_names.len();
        map.entries.retain(|e| e.installation_id != inst.id);
        store.save_managed_map(&map)?;
    }
    // 登録のみ (manage 未実行) の MCP 利用元: 設定は無変更なので直接起動は継続するが
    // 更新追跡は外れることを明示する
    let registered: Vec<String> = inst
        .usages
        .iter()
        .filter_map(|u| match u {
            UsageRef::McpServer { server_name, .. } => Some(server_name.clone()),
            _ => None,
        })
        .filter(|n| !managed_names.contains(n))
        .collect();
    if !registered.is_empty() {
        warnings.push(format!(
            "MCP ({}) の更新追跡を解除しました (設定は無変更のため Devin 側の起動は継続します)",
            registered.join(", ")
        ));
    }

    let id = &inst.id;
    for p in [
        store.layout.install_state_file(id),
        store.layout.job_file(id),
        store.layout.progress_file(id),
    ] {
        if p.exists() {
            std::fs::remove_file(&p)?;
        }
    }
    if store.layout.generations_base(id).exists() {
        std::fs::remove_dir_all(store.layout.generations_base(id))?;
    }
    // staging 残骸 <id>-*
    if let Ok(rd) = std::fs::read_dir(store.layout.staging_dir()) {
        let prefix = format!("{id}-");
        for e in rd.flatten() {
            if e.file_name().to_string_lossy().starts_with(&prefix) {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
    }
    store.update_registry(|reg| {
        reg.installations.retain(|i| i.id != inst.id);
        Ok(())
    })?;
    // ガード解放後にロックファイルを消す (自プロセス保持中は削除できない)
    drop(lock);
    let _ = std::fs::remove_file(store.layout.lock_file(id));
    crate::dlog!("remove: {} ({}) 登録解除", inst.name, inst.id);
    Ok(RemoveOutcome {
        name: inst.name.clone(),
        warnings,
        removed_managed_entries,
    })
}

/// ディレクトリ配下の合計バイト数 (ベストエフォート)
fn dir_size(p: &Path) -> u64 {
    let mut total = 0;
    if let Ok(rd) = std::fs::read_dir(p) {
        for e in rd.flatten() {
            if let Ok(m) = e.metadata() {
                total += if m.is_dir() {
                    dir_size(&e.path())
                } else {
                    m.len()
                };
            }
        }
    }
    total
}

// ---------- 診断バンドル ----------

/// 診断情報を zip にまとめる (問題報告用)。
/// MCP 設定・managed 記録に含まれる env/headers/token 等の値は "***" にマスクする。
pub fn diagnose(store: &Store, out: Option<&Path>) -> Result<PathBuf> {
    use std::io::Write;
    let zip_path = match out {
        Some(p) => p.to_path_buf(),
        None => store.layout.state_dir().join("diagnostics").join(format!(
            "dtm-diagnose-{}.zip",
            Utc::now().format("%Y%m%d-%H%M%S")
        )),
    };
    if let Some(parent) = zip_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::File::create(&zip_path)?;
    let mut zw = zip::ZipWriter::new(file);
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);

    let mut add = |name: &str, bytes: &[u8]| -> Result<()> {
        zw.start_file(name, opts)?;
        zw.write_all(bytes)?;
        Ok(())
    };

    // 概要
    let inst_count = store
        .load_registry()
        .map(|r| r.installations.len())
        .unwrap_or(0);
    let summary = serde_json::json!({
        "generatedAt": Utc::now(),
        "dtmVersion": env!("CARGO_PKG_VERSION"),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "root": store.layout.root,
        "installations": inst_count,
    });
    add(
        "summary.json",
        serde_json::to_string_pretty(&summary)?.as_bytes(),
    )?;

    // 状態ファイル一式
    for (name, path) in [
        ("registry.json", store.layout.registry_file()),
        ("managed-mcp.json", store.layout.managed_map_file()),
    ] {
        if let Ok(bytes) = std::fs::read(&path) {
            // managed-mcp の original.env は起動設定を含み得るためマスク
            if let Ok(mut v) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                redact_json(&mut v);
                add(name, serde_json::to_string_pretty(&v)?.as_bytes())?;
            } else {
                add(name, &bytes)?;
            }
        }
    }
    for (dir_name, dir) in [
        (
            "installations",
            store.layout.state_dir().join("installations"),
        ),
        ("jobs", store.layout.jobs_dir()),
    ] {
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_file() {
                    if let Ok(bytes) = std::fs::read(&p) {
                        add(
                            &format!("{dir_name}/{}", e.file_name().to_string_lossy()),
                            &bytes,
                        )?;
                    }
                }
            }
        }
    }

    // ユーザースコープ MCP 設定 (マスク済み)
    if let Ok(cfg) = crate::discover::user_config_path() {
        if let Ok(bytes) = std::fs::read(&cfg) {
            match serde_json::from_slice::<serde_json::Value>(&bytes) {
                Ok(mut v) => {
                    redact_json(&mut v);
                    add(
                        "mcp_config.redacted.json",
                        serde_json::to_string_pretty(&v)?.as_bytes(),
                    )?;
                }
                Err(_) => add("mcp_config.parse-error.txt", &bytes)?,
            }
        }
    }

    // ログ末尾 (最大 1MB)
    let log = store.layout.log_file();
    if let Ok(bytes) = std::fs::read(&log) {
        let tail = if bytes.len() > 1024 * 1024 {
            &bytes[bytes.len() - 1024 * 1024..]
        } else {
            &bytes[..]
        };
        add("dtm.log.tail.txt", tail)?;
    }

    zw.finish()?;
    crate::dlog!("diagnose: {} 作成", zip_path.display());
    Ok(zip_path)
}

/// JSON 内の認証情報らしきキーの値を "***" にマスクする。
/// オブジェクト値 (env/headers 等) は各要素を、スカラー値は直接置き換える。
fn redact_json(v: &mut serde_json::Value) {
    const SENSITIVE: &[&str] = &[
        "env",
        "headers",
        "authorization",
        "token",
        "secret",
        "password",
        "apikey",
        "api_key",
        "credential",
    ];
    match v {
        serde_json::Value::Object(m) => {
            for (k, val) in m.iter_mut() {
                let lk = k.to_lowercase();
                if SENSITIVE.iter().any(|s| lk.contains(s)) {
                    match val {
                        serde_json::Value::Object(inner) => {
                            for iv in inner.values_mut() {
                                *iv = serde_json::Value::String("***".into());
                            }
                        }
                        serde_json::Value::String(_) | serde_json::Value::Array(_) => {
                            *val = serde_json::Value::String("***".into());
                        }
                        _ => {}
                    }
                } else {
                    redact_json(val);
                }
            }
        }
        serde_json::Value::Array(a) => {
            for x in a.iter_mut() {
                redact_json(x);
            }
        }
        _ => {}
    }
}

// ---------- git ビルド型ソース (UpdateProvider::GitBuild) ----------

/// `git -C <dir> <args>` を実行し stdout (trim) を返す
fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = run_with_timeout(
        {
            let mut c = std::process::Command::new("git");
            c.arg("-C").arg(dir).args(args);
            c
        },
        Duration::from_secs(120),
        &format!("git {}", args.join(" ")),
    )?;
    if !out.status.success() {
        let tail = String::from_utf8_lossy(&out.stderr)
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("")
            .to_string();
        return Err(Error::Config(format!(
            "git {} 失敗: {}",
            args.join(" "),
            tail
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// "<commit日付>.<短縮sha>" 形式の版表現 (例: 2026-10-07.cf687d7)。
/// リリース版のないソース型ツール向けの人間可読な版識別子。
fn git_rev_version(dir: &Path, rev: &str) -> Result<String> {
    git(dir, &["log", "-1", "--format=%cs.%h", rev])
}

/// 作業ツリーの現在ブランチ名 (detached HEAD・非 git では None)。
/// set-git-source でブランチ省略時の既定値解決に使う。
pub fn git_current_branch(dir: &Path) -> Option<String> {
    git(dir, &["rev-parse", "--abbrev-ref", "HEAD"])
        .ok()
        .filter(|b| !b.is_empty() && b != "HEAD")
}

/// origin に存在するブランチ名の一覧 (ls-remote)。
/// 登録時のブランチ名検証用。ネットワーク不通時は Err。
pub fn git_remote_branches(dir: &Path) -> Result<Vec<String>> {
    let out = git(dir, &["ls-remote", "--heads", "origin"])?;
    Ok(out
        .lines()
        .filter_map(|l| l.rsplit("refs/heads/").next().map(String::from))
        .collect())
}

/// fetch して FETCH_HEAD の版表現を返す (ネットワーク経由)
fn git_remote_version(p: &GitBuildProvider) -> Result<String> {
    let wd = &p.work_dir;
    if !wd.join(".git").exists() {
        return Err(Error::NeedUser(format!(
            "git 作業ディレクトリではありません: {}",
            wd.display()
        )));
    }
    git(wd, &["fetch", "--quiet", "origin", &p.branch])?;
    git_rev_version(wd, "FETCH_HEAD")
}

/// git 作業ツリーの最新化 → ビルドコマンド実行 → 成果物ディレクトリを zip 化。
/// pull は --ff-only のみ許可 (ローカル変更の巻き込み・強制書換を防ぐ)。
fn git_build_fetch(p: &GitBuildProvider, staging: &Path) -> Result<(fetch::FetchOutcome, String)> {
    let wd = &p.work_dir;
    if !wd.join(".git").exists() {
        return Err(Error::NeedUser(format!(
            "git 作業ディレクトリではありません: {}",
            wd.display()
        )));
    }
    crate::dlog!(
        "git-build: pull --ff-only origin {} in {}",
        p.branch,
        wd.display()
    );
    git(wd, &["pull", "--ff-only", "origin", &p.branch]).map_err(|e| {
        Error::NeedUser(format!(
            "git pull 失敗 (ローカル変更が残っている可能性): {e}"
        ))
    })?;
    // 版はビルド前の HEAD で確定
    let ver = git_rev_version(wd, "HEAD")?;
    for cmdline in &p.build_commands {
        crate::dlog!("git-build: 実行 `{cmdline}` in {}", wd.display());
        let mut c = std::process::Command::new(if cfg!(windows) { "cmd" } else { "sh" });
        if cfg!(windows) {
            c.args(["/c", cmdline]);
        } else {
            c.args(["-c", cmdline]);
        }
        c.current_dir(wd);
        let out = run_with_timeout(c, Duration::from_secs(600), cmdline)?;
        if !out.status.success() {
            let tail = String::from_utf8_lossy(&out.stderr)
                .lines()
                .rev()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("")
                .to_string();
            return Err(Error::Config(format!(
                "ビルドコマンド失敗 `{cmdline}`: {tail}"
            )));
        }
    }
    let rel = p
        .output_dir
        .as_deref()
        .ok_or_else(|| Error::Config("outputDir 未設定".into()))?;
    let out_dir = wd.join(rel);
    if !out_dir.is_dir() {
        return Err(Error::Config(format!(
            "成果物ディレクトリが見つかりません: {}",
            out_dir.display()
        )));
    }
    let zip_path = staging.join("build-output.zip");
    zip_dir(&out_dir, &zip_path)?;
    let sha = file_sha256(&zip_path)?;
    Ok((
        fetch::FetchOutcome {
            file: zip_path,
            sha256: sha,
            verification: VerificationResult {
                status: "unverified".into(),
                method: Some("local-build".into()),
                detail: Some(format!("git {} @ {}", wd.display(), ver)),
            },
        },
        ver,
    ))
}

/// ディレクトリを再帰的に zip 化 (ビルド成果物のパッケージング用)
fn zip_dir(src: &Path, dest: &Path) -> Result<()> {
    use std::io::Write;
    let f = std::fs::File::create(dest)?;
    let mut zw = zip::ZipWriter::new(f);
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    let base = src.to_path_buf();
    let mut stack = vec![base.clone()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d)? {
            let p = e?.path();
            let rel = p
                .strip_prefix(&base)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            if p.is_dir() {
                stack.push(p);
            } else {
                zw.start_file(rel, opts)?;
                zw.write_all(&std::fs::read(&p)?)?;
            }
        }
    }
    zw.finish()?;
    Ok(())
}

fn file_sha256(p: &Path) -> Result<String> {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    std::io::copy(&mut std::fs::File::open(p)?, &mut hasher)?;
    Ok(format!("{:x}", hasher.finalize()))
}

/// 子プロセスをタイムアウト付きで実行。キャッシュロック等の無限待機を防ぐ。
fn run_with_timeout(
    mut cmd: std::process::Command,
    timeout: Duration,
    desc: &str,
) -> Result<std::process::Output> {
    use std::io::Read;
    // パイプ回収の子プロセスに新規コンソールウィンドウは不要 (窓点滅抑制)
    crate::procutil::hide_window(&mut cmd);
    let mut child = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| Error::Config(format!("{desc} の起動失敗: {e}")))?;

    // stdout/stderr は別スレッドで逐次回収する — 子がパイプ容量 (64KB)
    // を超えて書き込むと、親が終了後に読む方式では子が書き込み
    // ブロックして永久に終了せず、タイムアウトで強制終了されてしまう
    // (install/build 系は出力が大きくなり得る)。
    fn drain<R: Read + Send + 'static>(mut s: R) -> std::sync::Arc<std::sync::Mutex<Vec<u8>>> {
        let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        let out = buf.clone();
        std::thread::spawn(move || {
            let mut tmp = [0u8; 8192];
            loop {
                match s.read(&mut tmp) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => out.lock().unwrap().extend_from_slice(&tmp[..n]),
                }
            }
        });
        buf
    }
    let stdout_buf = child
        .stdout
        .take()
        .map(drain)
        .unwrap_or_else(|| std::sync::Arc::new(std::sync::Mutex::new(Vec::new())));
    let stderr_buf = child
        .stderr
        .take()
        .map(drain)
        .unwrap_or_else(|| std::sync::Arc::new(std::sync::Mutex::new(Vec::new())));

    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                // ドレインスレッドの最終転送を拾うための短い猶予
                std::thread::sleep(Duration::from_millis(50));
                let status = child.wait()?;
                return Ok(std::process::Output {
                    status,
                    stdout: std::mem::take(&mut *stdout_buf.lock().unwrap()),
                    stderr: std::mem::take(&mut *stderr_buf.lock().unwrap()),
                });
            }
            Ok(None) => {
                if std::time::Instant::now() > deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(Error::NeedUser(format!(
                        "{desc} がタイムアウト ({}s)。対象マネージャのキャッシュ/ロック使用中の可能性があります",
                        timeout.as_secs()
                    )));
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(e) => return Err(Error::Config(format!("{desc} 待機失敗: {e}"))),
        }
    }
}

/// npm spec → レジストリ問い合わせ用の名前部分を抽出。
/// `github:`/`git+`/`http`/ローカル spec はレジストリ外のため None。
fn npm_spec_name(spec: &str) -> Option<String> {
    if spec.contains(':') {
        return None;
    }
    if let Some(rest) = spec.strip_prefix('@') {
        // スコープ付き: @scope/name[@tag]
        let (scope, name_and_tag) = rest.split_once('/')?;
        let name = name_and_tag.split('@').next()?;
        Some(format!("@{scope}/{name}"))
    } else {
        Some(spec.split('@').next()?.to_string())
    }
}

/// マネージャのレジストリ最新版を取得 (参照用・失敗しても pm-sync 自体は続行)。
fn registry_latest(manager: &str, package: &str) -> Option<String> {
    let url = match manager {
        "npm" => {
            let name = npm_spec_name(package)?;
            format!(
                "https://registry.npmjs.org/{}/latest",
                name.replace('/', "%2F")
            )
        }
        "uv" | "uv-tool" => {
            let name = package.split(&['@', ';'][..]).next()?;
            format!("https://pypi.org/pypi/{name}/json")
        }
        _ => return None,
    };
    let client = reqwest::blocking::Client::builder()
        .user_agent("dtm")
        .timeout(Duration::from_secs(10))
        .build()
        .ok()?;
    let v: serde_json::Value = client.get(&url).send().ok()?.json().ok()?;
    v.get("version")
        .and_then(|x| x.as_str())
        .map(String::from)
        .or_else(|| {
            v.get("info")
                .and_then(|i| i.get("version"))
                .and_then(|x| x.as_str())
                .map(String::from)
        })
}

/// 単純なパッケージ名か (シェル/python への埋め込み可否判定)
fn simple_pkg_name(pkg: &str) -> bool {
    !pkg.is_empty()
        && pkg
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// uvx/uv tool env のインストール済み版をプローブ (ツール本体は起動しない)。
/// refresh フラグなしで実行 → キャッシュの現行版を返す。
fn uvx_installed_version(package: &str) -> Option<String> {
    if !simple_pkg_name(package) {
        return None;
    }
    let code = format!("import importlib.metadata;print(importlib.metadata.version('{package}'))");
    let out = run_with_timeout(
        {
            let mut c = std::process::Command::new("uv");
            c.args(["tool", "run", "--from", package, "python", "-c", &code]);
            c
        },
        Duration::from_secs(60),
        "uvx 版プローブ",
    )
    .ok()?;
    if !out.status.success() {
        return None;
    }
    let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!v.is_empty()).then_some(v)
}

/// npx キャッシュ (`<cache>/_npx/*/node_modules/<pkg>`) の実装版を
/// ファイルスキャンで取得する。npm exec は PATH に bin を通すだけで
/// node の require 解決には繋がらないため、以前の -p プローブは
/// 構造的に失敗していた。複数キャッシュがある場合は最新更新日時を採用。
fn npm_installed_version(spec: &str) -> Option<String> {
    let name = npm_spec_name(spec)?;
    let cache = npm_cache_dir()?;
    let rel = Path::new("node_modules").join(name);
    let mut best: Option<(std::time::SystemTime, String)> = None;
    for e in std::fs::read_dir(cache.join("_npx")).ok()? {
        let e = e.ok()?;
        let pj = e.path().join(&rel).join("package.json");
        let Ok(txt) = std::fs::read_to_string(&pj) else {
            continue;
        };
        let Some(ver) = serde_json::from_str::<serde_json::Value>(&txt)
            .ok()
            .and_then(|j| j.get("version").and_then(|v| v.as_str()).map(String::from))
        else {
            continue;
        };
        let mt = pj
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        if best.as_ref().map(|(t, _)| mt > *t).unwrap_or(true) {
            best = Some((mt, ver));
        }
    }
    best.map(|(_, v)| v)
}

/// `npm config get cache` → 失敗時は %LOCALAPPDATA%\npm-cache
fn npm_cache_dir() -> Option<PathBuf> {
    let mut c = std::process::Command::new(if cfg!(windows) { "cmd" } else { "npm" });
    if cfg!(windows) {
        c.args(["/c", "npm"]);
    }
    c.args(["config", "get", "cache"]);
    let dir = run_with_timeout(c, Duration::from_secs(15), "npm cache dir")
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("LOCALAPPDATA").map(|l| PathBuf::from(l).join("npm-cache")))?;
    dir.exists().then_some(dir)
}

// ---------- npm レジストリ取得プロバイダー (仕様 §5.2) ----------
// PM ランチャー経由起動の委譲 (PackageManager) とは別経路。
// `node script.mjs` 等ローカルファイル起動の実体を DTM 自身が取得・世代管理する。

struct NpmSelected {
    version: String,
    tarball: String,
    integrity: Option<String>,
    shasum: Option<String>,
}

fn npm_registry_base(p: &NpmRegistryProvider) -> String {
    p.registry
        .clone()
        .unwrap_or_else(|| "https://registry.npmjs.org".into())
        .trim_end_matches('/')
        .to_string()
}

fn npm_http_client() -> Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder()
        .user_agent("dtm")
        .build()?)
}

/// パッケージメタデータ (packument) を取得。
/// `GET {registry}/{pkg}` — スコープ付きは `@scope%2Fname` 形式。
fn npm_packument(base: &str, package: &str, timeout: Duration) -> Result<serde_json::Value> {
    let url = format!("{}/{}", base, package.replace('/', "%2F"));
    let client = reqwest::blocking::Client::builder()
        .user_agent("dtm")
        .timeout(timeout)
        .build()?;
    let resp = client
        .get(&url)
        .header("Accept", "application/json")
        .send()?;
    if !resp.status().is_success() {
        return Err(Error::Config(format!(
            "npm packument 取得失敗 {url}: HTTP {}",
            resp.status()
        )));
    }
    Ok(resp.json()?)
}

/// packument から取得対象版を選択する。
/// pin 指定時はその版の存在を要求、未指定時は dist-tags.latest。
fn npm_select_version(
    p: &NpmRegistryProvider,
    pack: &serde_json::Value,
    pin: Option<&str>,
) -> Result<NpmSelected> {
    let versions = pack
        .get("versions")
        .and_then(|v| v.as_object())
        .ok_or_else(|| Error::Config("npm packument に versions がありません".into()))?;
    let ver = if let Some(pin) = pin {
        if !versions.contains_key(pin) {
            return Err(Error::NeedUser(format!(
                "版固定 {pin} が {} のレジストリに存在しません",
                p.package
            )));
        }
        pin.to_string()
    } else {
        pack.pointer("/dist-tags/latest")
            .and_then(|v| v.as_str())
            .ok_or_else(|| Error::Config("npm packument に dist-tags.latest がありません".into()))?
            .to_string()
    };
    let v = versions
        .get(&ver)
        .ok_or_else(|| Error::Config(format!("npm {ver} が versions にありません")))?;
    let tarball = v
        .pointer("/dist/tarball")
        .and_then(|x| x.as_str())
        .ok_or_else(|| Error::Config(format!("npm {ver} に dist.tarball がありません")))?
        .to_string();
    Ok(NpmSelected {
        version: ver,
        tarball,
        integrity: v
            .pointer("/dist/integrity")
            .and_then(|x| x.as_str())
            .map(String::from),
        shasum: v
            .pointer("/dist/shasum")
            .and_then(|x| x.as_str())
            .map(String::from),
    })
}

/// npm dist の integrity (SSRI 形式 "sha512-<base64>") / shasum (sha1 hex) を検証。
/// sha512 一致なら verified、sha1/shasum のみは sha1 実装で検証、
/// どちらも無い・対応形式でない場合は unverified 記録 (成功扱いにしない)。
fn verify_npm_integrity(
    outcome: &mut fetch::FetchOutcome,
    integrity: Option<&str>,
    shasum: Option<&str>,
) -> Result<()> {
    if let Some(integ) = integrity {
        for tok in integ.split_whitespace() {
            if let Some(b64) = tok.strip_prefix("sha512-") {
                use base64::Engine;
                use sha2::Digest;
                let digest = sha2::Sha512::digest(std::fs::read(&outcome.file)?);
                let actual = base64::engine::general_purpose::STANDARD.encode(digest);
                if actual == b64 {
                    outcome.verification = VerificationResult {
                        status: "verified".into(),
                        method: Some("npm-integrity".into()),
                        detail: Some(format!("sha512 integrity 一致 ({})", &b64[..16])),
                    };
                    return Ok(());
                }
                return Err(Error::Verify(format!(
                    "npm integrity 不一致: 期待 sha512-{b64} / 実際 sha512-{actual}"
                )));
            }
        }
    }
    if let Some(s1) = shasum {
        let s1 = s1.trim().to_lowercase();
        use sha1::Digest;
        let digest = sha1::Sha1::digest(std::fs::read(&outcome.file)?);
        let actual = digest
            .iter()
            .map(|x| format!("{x:02x}"))
            .collect::<String>();
        if actual == s1 {
            outcome.verification = VerificationResult {
                status: "verified".into(),
                method: Some("npm-shasum".into()),
                detail: Some(format!("sha1 shasum 一致 ({s1})")),
            };
            return Ok(());
        }
        return Err(Error::Verify(format!(
            "npm shasum 不一致: 期待 {s1} / 実際 {actual}"
        )));
    }
    outcome.verification = VerificationResult {
        status: "unverified".into(),
        method: None,
        detail: Some("npm dist が integrity/shasum を提供していません".into()),
    };
    Ok(())
}

/// マネージャ別の委譲更新コマンドを構築。
/// 未対応マネージャ (pnpm/yarn/bun/cargo/winget 等) は NeedUser で明示拒否。
fn pm_command(manager: &str, package: &str) -> Result<(std::process::Command, String)> {
    match manager {
        "uv-tool" => {
            let mut c = std::process::Command::new("uv");
            c.args(["tool", "upgrade", package]);
            Ok((c, format!("uv tool upgrade {package}")))
        }
        "uv" => {
            // uvx キャッシュを再解決。ツール本体は起動せず python の即終了だけ実行。
            // (uv cache clean は稼働中 uvx がロック保持で無限待機するため不使用)
            let mut c = std::process::Command::new("uv");
            c.args([
                "tool",
                "run",
                "--refresh-package",
                package,
                "--from",
                package,
                "python",
                "-c",
                "pass",
            ]);
            Ok((
                c,
                format!("uv tool run --refresh-package {package} (uvx キャッシュ更新)"),
            ))
        }
        "npm" => {
            // npx キャッシュへ最新を取り込む。MCP 本体は起動せず node の即終了だけ実行。
            // Windows の npm は npm.cmd のため cmd 経由が必要。
            let mut c = std::process::Command::new(if cfg!(windows) { "cmd" } else { "npm" });
            if cfg!(windows) {
                c.args(["/c", "npm"]);
            }
            c.args([
                "exec",
                "--yes",
                "--package",
                package,
                "--",
                "node",
                "-e",
                "process.exit(0)",
            ]);
            Ok((
                c,
                format!("npm exec --package {package} (npx キャッシュ更新)"),
            ))
        }
        other => Err(Error::NeedUser(format!(
            "'{other}' の外部更新は未対応です (対応: npm / uv / uv-tool)"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::Release;

    fn rel(tag: &str) -> Release {
        Release {
            tag_name: tag.into(),
            name: None,
            prerelease: false,
            draft: false,
            html_url: "u".into(),
            published_at: None,
            assets: vec![],
            body: None,
        }
    }

    fn prov() -> GithubReleaseProvider {
        GithubReleaseProvider {
            repository: "o/r".into(),
            channel: ReleaseChannel::Stable,
            asset: AssetSelector {
                match_type: AssetMatchType::Glob,
                pattern: "*.zip".into(),
                tag: None,
                platform: None,
                architecture: None,
            },
            version_scheme: VersionScheme::Semver,
        }
    }

    #[test]
    fn pin_selects_matching_release_only() {
        let releases = vec![rel("v2.0.0"), rel("v1.0.0")];
        let p = prov();
        // 固定版 1.0.0 → v1.0.0 を選ぶ (最新の 2.0.0 ではない)
        let r = select_release_for_policy(&releases, &p, Some("1.0.0")).unwrap();
        assert_eq!(r.tag_name, "v1.0.0");
        // 無い固定版 → NeedUser (自動更新しない)
        assert!(matches!(
            select_release_for_policy(&releases, &p, Some("9.9.9")),
            Err(Error::NeedUser(_))
        ));
        // pin なし → 最新
        let r = select_release_for_policy(&releases, &p, None).unwrap();
        assert_eq!(r.tag_name, "v2.0.0");
    }

    #[test]
    fn pin_gate_blocks_mismatched_apply() {
        let mut inst_recipe = Recipe {
            schema_version: 1,
            id: "t".into(),
            kind: ToolKind::CliTool,
            installation_id: "iid".into(),
            source_repository: None,
            update_provider: None,
            artifact: None,
            installation: None,
            policy: Some(Policy {
                prepare_automatically: false,
                pinned_version: Some("1.0.0".into()),
                check_interval_hours: 24,
                on_preparation_failure: OnFailure::KeepCurrent,
            }),
            verify: None,
            data_preserve: vec![],
            data_migration_required: false,
        };
        let inst = Installation {
            id: "iid".into(),
            name: "t".into(),
            kind: ToolKind::CliTool,
            environment: "windows-x64".into(),
            install_root: Path::new("C:/x").to_path_buf(),
            fixed_path: None,
            recipe: inst_recipe.clone(),
            usages: vec![],
            created_at: None,
            updated_at: None,
        };
        assert!(enforce_pin_on_apply(&inst, "v1.0.0").is_ok());
        assert!(matches!(
            enforce_pin_on_apply(&inst, "2.0.0"),
            Err(Error::Conflict(_))
        ));
        inst_recipe.policy.as_mut().unwrap().pinned_version = None;
        let inst2 = Installation {
            recipe: inst_recipe,
            ..inst
        };
        assert!(enforce_pin_on_apply(&inst2, "2.0.0").is_ok());
    }

    // ---------- npm レジストリプロバイダー (仕様 §5.2) ----------

    fn npm_packument() -> serde_json::Value {
        serde_json::json!({
            "name": "pkg-x",
            "dist-tags": {"latest": "2.0.0", "beta": "2.1.0-beta.1"},
            "versions": {
                "1.0.0": {"dist": {"tarball": "https://r/x-1.0.0.tgz", "integrity": "sha512-a", "shasum": "s1"}},
                "2.0.0": {"dist": {"tarball": "https://r/x-2.0.0.tgz", "integrity": "sha512-b", "shasum": "s2"}},
                "2.1.0-beta.1": {"dist": {"tarball": "https://r/x-2.1.0.tgz"}}
            }
        })
    }

    #[test]
    fn npm_select_latest_and_pin() {
        let p = NpmRegistryProvider {
            package: "pkg-x".into(),
            registry: None,
            install_commands: vec![],
            version_scheme: VersionScheme::Semver,
        };
        let pack = npm_packument();
        // 最新版選択 (dist-tags.latest)
        let sel = npm_select_version(&p, &pack, None).unwrap();
        assert_eq!(sel.version, "2.0.0");
        assert_eq!(sel.tarball, "https://r/x-2.0.0.tgz");
        assert_eq!(sel.integrity.as_deref(), Some("sha512-b"));
        // 版固定選択
        let sel = npm_select_version(&p, &pack, Some("1.0.0")).unwrap();
        assert_eq!(sel.version, "1.0.0");
        // 存在しない固定版 → 拒否
        assert!(matches!(
            npm_select_version(&p, &pack, Some("9.9.9")),
            Err(Error::NeedUser(_))
        ));
    }

    #[test]
    fn npm_integrity_sha512_and_shasum() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("pkg.tgz");
        let content = b"tarball payload";
        std::fs::write(&file, content).unwrap();
        use sha2::Digest;
        let b64 = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            sha2::Sha512::digest(content),
        );
        let mut outcome = fetch::FetchOutcome {
            file: file.clone(),
            sha256: "x".into(),
            verification: VerificationResult {
                status: "pending".into(),
                method: None,
                detail: None,
            },
        };
        // sha512 integrity 一致 → verified
        verify_npm_integrity(&mut outcome, Some(&format!("sha512-{b64}")), None).unwrap();
        assert_eq!(outcome.verification.status, "verified");
        // sha512 不一致 → Verify エラー
        let bad = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            sha2::Sha512::digest(b"other"),
        );
        assert!(matches!(
            verify_npm_integrity(&mut outcome, Some(&format!("sha512-{bad}")), None),
            Err(Error::Verify(_))
        ));
        // shasum (sha1 hex) 一致 → verified
        let s1 = sha1::Sha1::digest(content)
            .iter()
            .map(|x| format!("{x:02x}"))
            .collect::<String>();
        verify_npm_integrity(&mut outcome, None, Some(&s1)).unwrap();
        assert_eq!(outcome.verification.status, "verified");
        // shasum 不一致 → Verify エラー
        assert!(matches!(
            verify_npm_integrity(&mut outcome, None, Some("deadbeef")),
            Err(Error::Verify(_))
        ));
        // 両方なし → unverified 記録 (成功扱いにしない)
        verify_npm_integrity(&mut outcome, None, None).unwrap();
        assert_eq!(outcome.verification.status, "unverified");
    }
}

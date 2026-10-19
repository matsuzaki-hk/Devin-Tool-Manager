//! dtm-core ユニットテスト: 版比較・パス正規化・ZIP安全性・
//! チェックサム解析・アセット選択 (受け入れ条件 §15-4,5,7 関連)

use dtm_core::github::{Asset, GithubClient, Release};
use dtm_core::models::*;
use dtm_core::paths;
use dtm_core::version::{self, CompareResult};
use dtm_core::zipx;
use std::io::Write;
use std::path::Path;

// ---------- 版比較 (仕様 §5.1) ----------

#[test]
fn semver_compare() {
    assert_eq!(
        version::compare(VersionScheme::Semver, "0.37.2", "v0.51.0"),
        CompareResult::Newer
    );
    assert_eq!(
        version::compare(VersionScheme::Semver, "1.2.3", "1.2.3"),
        CompareResult::Equal
    );
    assert_eq!(
        version::compare(VersionScheme::Semver, "2.0.0", "1.9.9"),
        CompareResult::Older // 自動ダウングレードしない
    );
}

#[test]
fn date_compare() {
    assert_eq!(
        version::compare(VersionScheme::Date, "2024-01-15", "2024.02.01"),
        CompareResult::Newer
    );
    assert_eq!(
        version::compare(VersionScheme::Date, "20240115", "2024-01-15"),
        CompareResult::Equal
    );
}

#[test]
fn incomparable_is_not_newer() {
    // 独自タグ同士は比較不能 → 自動更新しない (仕様 §5.1)
    assert_eq!(
        version::compare(VersionScheme::Semver, "nightly-x", "nightly-y"),
        CompareResult::Incomparable
    );
    assert_eq!(
        version::compare(VersionScheme::Custom, "a", "b"),
        CompareResult::Incomparable
    );
}

#[test]
fn fileversion_fourpart_compare() {
    // Windows FileVersion (4部) と semver タグの比較: 0.2.0.0 == v0.2.0
    assert_eq!(
        version::compare(VersionScheme::Semver, "0.2.0.0", "v0.2.0"),
        CompareResult::Equal
    );
    // 4部の末尾が非ゼロなら大小関係を保持
    assert_eq!(
        version::compare(VersionScheme::Semver, "0.2.0.5", "0.2.0"),
        CompareResult::Older
    );
    assert_eq!(
        version::compare(VersionScheme::Semver, "0.2.0", "0.2.1.0"),
        CompareResult::Newer
    );
    // 部数違いの0埋め: 1.2 == 1.2.0.0
    assert_eq!(
        version::compare(VersionScheme::Semver, "1.2", "1.2.0.0"),
        CompareResult::Equal
    );
}

// ---------- パス正規化 (仕様 §4.2) ----------

#[test]
fn path_key_normalizes_case_and_separators() {
    let a = Path::new("C:\\Work\\RTK\\rtk.exe");
    let b = Path::new("c:/work/rtk/rtk.exe");
    // 存在しないパスでも構文正規化+小文字化で一致する
    assert_eq!(paths::path_key(a), paths::path_key(b));
}

#[test]
fn path_key_resolves_dotdot() {
    let a = Path::new("C:\\work\\a\\..\\rtk\\x.exe");
    let b = Path::new("C:\\work\\rtk\\x.exe");
    assert_eq!(paths::path_key(a), paths::path_key(b));
}

#[test]
fn installation_id_same_for_equivalent_paths() {
    let a = dtm_core::id::installation_id(Path::new("C:/work/rtk"), "windows-x64");
    let b = dtm_core::id::installation_id(Path::new("c:\\work\\rtk\\"), "windows-x64");
    assert_eq!(a, b);
    // 別環境・別配置は別ID
    let c = dtm_core::id::installation_id(Path::new("c:\\work\\rtk2"), "windows-x64");
    assert_ne!(a, c);
    let d = dtm_core::id::installation_id(Path::new("c:\\work\\rtk"), "linux-x64");
    assert_ne!(a, d);
}

// ---------- ZIP 安全性 (仕様 §6-4) ----------

fn make_zip(entries: &[(&str, &[u8])]) -> tempfile::NamedTempFile {
    let f = tempfile::NamedTempFile::new().unwrap();
    {
        let mut w = zip::ZipWriter::new(std::fs::File::create(f.path()).unwrap());
        for (name, data) in entries {
            w.start_file::<_, ()>(*name, zip::write::FileOptions::default())
                .unwrap();
            w.write_all(data).unwrap();
        }
        w.finish().unwrap();
    }
    f
}

#[test]
fn zip_rejects_parent_escape() {
    let z = make_zip(&[("../evil.exe", b"x")]);
    let r = zipx::inspect(z.path(), &zipx::ZipLimits::default());
    assert!(matches!(r, Err(dtm_core::Error::Unsafe(_))));
}

#[test]
fn zip_rejects_absolute_path() {
    let z = make_zip(&[("C:/Windows/evil.exe", b"x"), ("/abs/evil", b"y")]);
    let r = zipx::inspect(z.path(), &zipx::ZipLimits::default());
    assert!(matches!(r, Err(dtm_core::Error::Unsafe(_))));
}

#[test]
fn zip_rejects_size_bomb() {
    let big = vec![0u8; 1024];
    let z = make_zip(&[("a.bin", &big), ("b.bin", &big)]);
    let limits = zipx::ZipLimits {
        max_total_bytes: 1000,
        max_files: 100,
    };
    assert!(matches!(
        zipx::inspect(z.path(), &limits),
        Err(dtm_core::Error::Unsafe(_))
    ));
}

#[test]
fn zip_extract_normal() {
    let z = make_zip(&[
        ("tool.exe", b"MZ"),
        ("sub/lib.dll", b"DLL"),
        ("README.md", b"readme"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let files = zipx::extract(z.path(), dir.path(), &zipx::ZipLimits::default()).unwrap();
    assert_eq!(files.len(), 3);
    assert!(dir.path().join("tool.exe").exists());
    assert!(dir.path().join("sub").join("lib.dll").exists());
}

// ---------- アセット選択 (仕様 §5.1) ----------

fn rel_with_assets(names: &[&str]) -> Release {
    Release {
        tag_name: "v1.0.0".into(),
        name: None,
        prerelease: false,
        draft: false,
        html_url: "u".into(),
        published_at: None,
        assets: names
            .iter()
            .map(|n| Asset {
                name: n.to_string(),
                browser_download_url: format!("https://x/{n}"),
                size: 0,
                digest: None,
                content_type: None,
            })
            .collect(),
        body: None,
    }
}

fn provider(pattern: &str, mt: AssetMatchType) -> GithubReleaseProvider {
    GithubReleaseProvider {
        repository: "o/r".into(),
        channel: ReleaseChannel::Stable,
        asset: AssetSelector {
            match_type: mt,
            pattern: pattern.into(),
            tag: None,
            platform: Some("windows".into()),
            architecture: Some("x64".into()),
        },
        version_scheme: VersionScheme::Semver,
    }
}

#[test]
fn asset_exact_match() {
    let r = rel_with_assets(&["rtk-x86_64-pc-windows-msvc.zip", "rtk-linux.zip"]);
    let p = provider("rtk-x86_64-pc-windows-msvc.zip", AssetMatchType::Exact);
    let a = GithubClient::select_asset(&r, &p).unwrap();
    assert_eq!(a.name, "rtk-x86_64-pc-windows-msvc.zip");
}

#[test]
fn asset_glob_match() {
    let r = rel_with_assets(&["tool-1.0-windows-x64.zip", "tool-1.0-linux.zip"]);
    let p = provider("tool-*-windows-*.zip", AssetMatchType::Glob);
    assert_eq!(
        GithubClient::select_asset(&r, &p).unwrap().name,
        "tool-1.0-windows-x64.zip"
    );
}

#[test]
fn asset_multiple_match_requires_narrowing() {
    let r = rel_with_assets(&["a-win-x64.zip", "a-win-arm64.zip"]);
    let p = provider("a-win-*.zip", AssetMatchType::Glob);
    assert!(matches!(
        GithubClient::select_asset(&r, &p),
        Err(dtm_core::Error::NeedUser(_))
    ));
}

#[test]
fn asset_zero_match_holds() {
    let r = rel_with_assets(&["b.zip"]);
    let p = provider("a-*.zip", AssetMatchType::Glob);
    assert!(matches!(
        GithubClient::select_asset(&r, &p),
        Err(dtm_core::Error::NeedUser(_))
    ));
}

#[test]
fn release_channel_stable_skips_prerelease() {
    let mut r1 = rel_with_assets(&[]);
    r1.prerelease = true;
    r1.tag_name = "v9.9.9".into();
    let r2 = rel_with_assets(&[]);
    let releases = vec![r1, r2];
    let p = provider("*.zip", AssetMatchType::Glob);
    let sel = GithubClient::select_release(&releases, &p).unwrap();
    assert_eq!(sel.tag_name, "v1.0.0");
}

// ---------- パッケージマネージャ検出 (仕様 §9, §15-13) ----------

#[test]
fn detects_npx_uvx_as_managed() {
    let l = LaunchSpec {
        command: "npx.cmd".into(),
        args: vec!["-y".into(), "@playwright/mcp@latest".into()],
        env: Default::default(),
        cwd: None,
    };
    let pm = dtm_core::resolve::detect_manager(&l.command, &l.args).unwrap();
    assert_eq!(pm.manager, "npm");
    assert_eq!(pm.package.as_deref(), Some("@playwright/mcp@latest"));

    let l2 = LaunchSpec {
        command: "uvx".into(),
        args: vec!["--from".into(), "semble[mcp]".into(), "semble".into()],
        env: Default::default(),
        cwd: None,
    };
    let pm2 = dtm_core::resolve::detect_manager(&l2.command, &l2.args).unwrap();
    assert_eq!(pm2.manager, "uv");
    assert_eq!(pm2.package.as_deref(), Some("semble"));
}

// ---------- 原子的書き込み (R2) ----------

#[test]
fn atomic_write_replaces_existing_without_gap() {
    let d = tempfile::tempdir().unwrap();
    let f = d.path().join("state.json");
    paths::atomic_write(&f, b"old").unwrap();
    paths::atomic_write(&f, b"new").unwrap();
    assert_eq!(std::fs::read(&f).unwrap(), b"new");
    // 退避/temp ファイルが残らない (旧実装は .old-* を残し得た)
    let leftovers: Vec<_> = std::fs::read_dir(d.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n != "state.json")
        .collect();
    assert!(leftovers.is_empty(), "残存ファイル: {leftovers:?}");
}

// ---------- fixed-path ジャーナル復旧 (R3) ----------

#[test]
fn swap_journal_recovers_interrupted_state() {
    let d = tempfile::tempdir().unwrap();
    let install = d.path().join("inst");
    std::fs::create_dir_all(&install).unwrap();
    std::fs::write(install.join("old.exe"), b"OLD").unwrap();

    // 中断状態を再現: 旧版は退避済み、新版の一部が部分配置済み
    let backup = install.join(".dtm-backup").join("20260101000000");
    std::fs::create_dir_all(&backup).unwrap();
    std::fs::rename(install.join("old.exe"), backup.join("old.exe")).unwrap();
    std::fs::write(install.join("new.dll"), b"PARTIAL").unwrap();
    std::fs::write(
        backup.join("journal.json"),
        serde_json::json!({
            "startedAt": "2026-01-01T00:00:00Z",
            "installDir": install,
            "backupDir": backup,
            "backupFiles": ["old.exe"],
            "placeFiles": ["old.exe", "new.dll"],
            "staleFiles": [],
            "phase": "swapping"
        })
        .to_string(),
    )
    .unwrap();

    dtm_core::apply::recover_pending_swap(&install).unwrap();
    assert_eq!(std::fs::read(install.join("old.exe")).unwrap(), b"OLD");
    assert!(!install.join("new.dll").exists());
}

#[test]
fn swap_journal_done_is_not_touched() {
    let d = tempfile::tempdir().unwrap();
    let install = d.path().join("inst");
    std::fs::create_dir_all(&install).unwrap();
    std::fs::write(install.join("tool.exe"), b"NEW").unwrap();
    let backup = install.join(".dtm-backup").join("20260101000000");
    std::fs::create_dir_all(&backup).unwrap();
    std::fs::write(backup.join("tool.exe"), b"OLD").unwrap();
    std::fs::write(
        backup.join("journal.json"),
        serde_json::json!({
            "startedAt": "2026-01-01T00:00:00Z",
            "installDir": install,
            "backupDir": backup,
            "backupFiles": ["tool.exe"],
            "placeFiles": ["tool.exe"],
            "staleFiles": [],
            "phase": "done"
        })
        .to_string(),
    )
    .unwrap();
    dtm_core::apply::recover_pending_swap(&install).unwrap();
    // 完了済みジャーナルは復元しない (新版が残る)
    assert_eq!(std::fs::read(install.join("tool.exe")).unwrap(), b"NEW");
}

// ---------- rollback の世代選択 (R8) ----------

fn fixture_inst(iid: &str, root: &Path) -> Installation {
    Installation {
        id: iid.into(),
        name: "t".into(),
        kind: ToolKind::CliTool,
        environment: "windows-x64".into(),
        install_root: root.join("inst"),
        fixed_path: None,
        recipe: Recipe {
            schema_version: 1,
            id: "t".into(),
            kind: ToolKind::CliTool,
            installation_id: iid.into(),
            source_repository: None,
            update_provider: None,
            artifact: Some(ArtifactSpec {
                kind: ArtifactKind::Exe,
                root_pattern: None,
                entrypoint: "tool.exe".into(),
                include: vec!["**/*".into()],
                exclude: vec![],
                data_priority: DataPriority::DataWins,
            }),
            installation: Some(InstallStrategy {
                strategy: StrategyKind::Versioned,
                apply_when: ApplyWhen::NextLaunch,
                fixed_path: None,
                keep_generations: 5,
                max_generations_mib: 0,
            }),
            policy: None,
            verify: None,
            data_preserve: vec![],
            data_migration_required: false,
        },
        usages: vec![],
        created_at: None,
        updated_at: None,
    }
}

fn mk_gen(root: &Path, iid: &str, gen: &str, ver: &str, verified: bool, applied: bool) {
    let d = root.join("generations").join(iid).join(gen);
    std::fs::create_dir_all(&d).unwrap();
    let m = GenerationMeta {
        generation_id: gen.into(),
        version: ver.into(),
        created_at: chrono::Utc::now(),
        recipe_revision: 1,
        source: GenerationSource {
            kind: "test".into(),
            release_url: None,
            asset_name: None,
            sha256: "x".into(),
            imported_from: None,
        },
        verification: VerificationResult {
            status: "verified".into(),
            method: None,
            detail: None,
        },
        probed_version: None,
        verified,
        applied_at: applied.then(chrono::Utc::now),
        files: vec!["tool.exe".into()],
    };
    std::fs::write(d.join("genmeta.json"), serde_json::to_string(&m).unwrap()).unwrap();
    std::fs::write(d.join("tool.exe"), b"x").unwrap();
}

#[test]
fn rollback_skips_unverified_generations() {
    let root = tempfile::tempdir().unwrap();
    let store =
        dtm_core::Store::open(dtm_core::paths::Layout::new(root.path().to_path_buf())).unwrap();
    let iid = "iid-rollback-test";
    let inst = fixture_inst(iid, root.path());

    // 現行=0.2.0 適用済、旧版=0.1.0 適用済、失敗版=0.3.0 未検証
    mk_gen(root.path(), iid, "gen-old", "0.1.0", true, true);
    mk_gen(root.path(), iid, "gen-cur", "0.2.0", true, true);
    mk_gen(root.path(), iid, "gen-bad", "0.3.0", false, false);
    store
        .update_state::<()>(iid, |st| {
            st.active_generation = Some("gen-cur".into());
        })
        .unwrap();

    // 既定復元: 未検証の gen-bad を選ばず、適用済みの gen-old へ
    let v = dtm_core::apply::rollback(&store, &inst, None).unwrap();
    assert_eq!(v, "0.1.0");
    assert_eq!(
        store.active_generation(iid).unwrap().as_deref(),
        Some("gen-old")
    );

    // 未検証世代の明示指定も拒否
    assert!(matches!(
        dtm_core::apply::rollback(&store, &inst, Some("gen-bad")),
        Err(dtm_core::Error::NotFound(_))
    ));
}

// ---------- manage 可否ゲート (R5) ----------

#[test]
fn manage_rejects_unsupported_launch_forms() {
    use dtm_core::discover::{ConfigScope, McpServerEntry};
    let mk = |cmd: &str, args: Vec<String>| McpServerEntry {
        scope: ConfigScope::User,
        config_path: Path::new("C:/x/mcp_config.json").to_path_buf(),
        name: "t".into(),
        transport: "stdio".into(),
        launch: Some(LaunchSpec {
            command: cmd.into(),
            args,
            env: Default::default(),
            cwd: None,
        }),
        url: None,
        disabled: false,
        extra_keys: vec![],
    };
    // npx 経由 → 拒否
    assert!(matches!(
        dtm_core::register::ensure_manageable(&mk("npx", vec!["-y".into(), "pkg".into()])),
        Err(dtm_core::Error::NeedUser(_))
    ));
    // インタプリタ → 拒否 (スクリプト有無に関わらず)
    assert!(matches!(
        dtm_core::register::ensure_manageable(&mk("node", vec!["s.js".into()])),
        Err(dtm_core::Error::NeedUser(_))
    ));
    // リモート → 拒否
    let mut remote = mk("x", vec![]);
    remote.launch = None;
    assert!(dtm_core::register::ensure_manageable(&remote).is_err());
}

// ---------- プロセス間排他 (R4) ----------

#[test]
fn installation_lock_excludes_second_holder() {
    let root = tempfile::tempdir().unwrap();
    let layout = dtm_core::paths::Layout::new(root.path().to_path_buf());
    let l1 = dtm_core::lock::InstallationLock::try_acquire(&layout, "iid-x").unwrap();
    // 同一 installation の2つ目の取得は Busy (Windows os error 33 も含む)
    let r = dtm_core::lock::InstallationLock::try_acquire(&layout, "iid-x");
    assert!(matches!(r, Err(dtm_core::Error::Busy(_))));
    // 解放後は再取得できる
    drop(l1);
    assert!(dtm_core::lock::InstallationLock::try_acquire(&layout, "iid-x").is_ok());
}

// ---------- 進捗ファイルのゲート (job=running のみ有効) ----------

#[test]
fn progress_is_gated_by_running_job() {
    let root = tempfile::tempdir().unwrap();
    let layout = dtm_core::paths::Layout::new(root.path().to_path_buf());
    layout.ensure().unwrap();

    let p = dtm_core::lock::Progress::new(&layout, "iid-p");
    p.bytes(1024, Some(2048));
    // ファイルは書かれるが、running ジョブが無い間は読み取り側が無視する
    assert!(layout.progress_file("iid-p").exists());
    assert!(dtm_core::lock::active_progress(&layout, "iid-p").is_none());

    // running ジョブ開始 → 読める。ただし begin は前回残滓をクリアする
    let _lock = dtm_core::lock::InstallationLock::try_acquire(&layout, "iid-p").unwrap();
    let _job = dtm_core::lock::JobGuard::begin(&layout, "iid-p", "prepare").unwrap();
    assert!(dtm_core::lock::active_progress(&layout, "iid-p").is_none());
    p.phase("extract");
    let got = dtm_core::lock::active_progress(&layout, "iid-p").unwrap();
    assert_eq!(got.phase, "extract");
}

// ---------- 世代整理の候補選定 (現行/適用待ちは保護) ----------

#[test]
fn prune_candidates_protects_active_and_prepared() {
    let root = tempfile::tempdir().unwrap();
    let layout = dtm_core::paths::Layout::new(root.path().to_path_buf());
    for (i, g) in ["g1", "g2", "g3", "g4"].iter().enumerate() {
        mk_gen(root.path(), "iid-g", g, &format!("1.0.{i}"), true, false);
        // ディレクトリ mtime の分離 (新しい順ソートの安定化)
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    // keep=2: 新しい g4,g3 が残り g1,g2 が候補
    let mut names: Vec<String> = dtm_core::stage::prune_candidates(&layout, "iid-g", 2, None, None)
        .unwrap()
        .into_iter()
        .map(|(n, _)| n)
        .collect();
    names.sort();
    assert_eq!(names, vec!["g1", "g2"]);

    // keep=1 + 最古 g1 が現行 → g1 は保護され g2,g3 が候補 (g4 は keep 内)
    let mut names2: Vec<String> =
        dtm_core::stage::prune_candidates(&layout, "iid-g", 1, Some("g1"), None)
            .unwrap()
            .into_iter()
            .map(|(n, _)| n)
            .collect();
    names2.sort();
    assert_eq!(names2, vec!["g2", "g3"]);
}

// ---------- 登録解除 (remove) ----------

#[test]
fn remove_cli_installation_cleans_state_and_registry() {
    let root = tempfile::tempdir().unwrap();
    let store =
        dtm_core::Store::open(dtm_core::paths::Layout::new(root.path().to_path_buf())).unwrap();
    let iid = "iid-remove-test";
    let mut inst = fixture_inst(iid, root.path());
    inst.usages.push(UsageRef::Cli {
        via: "abs-path".into(),
        specifier: "C:\tool.exe".into(),
    });
    store.upsert_installation(inst.clone()).unwrap();
    mk_gen(root.path(), iid, "gen-a", "1.0.0", true, true);
    store
        .update_state::<()>(iid, |st| {
            st.active_generation = Some("gen-a".into());
        })
        .unwrap();

    let o = dtm_core::ops::remove(&store, &inst, false).unwrap();
    assert_eq!(o.name, "t");
    assert!(o.warnings.is_empty());

    // registry・状態・世代・ジョブが消え、検索も失敗する
    assert!(store.load_registry().unwrap().installations.is_empty());
    assert!(!root
        .path()
        .join("state")
        .join("installations")
        .join(format!("{iid}.json"))
        .exists());
    assert!(!root.path().join("generations").join(iid).exists());
    assert!(matches!(
        store.find_installation("t"),
        Err(dtm_core::Error::NotFound(_))
    ));
}

#[test]
fn remove_refuses_managed_mcp_without_force() {
    let root = tempfile::tempdir().unwrap();
    let store =
        dtm_core::Store::open(dtm_core::paths::Layout::new(root.path().to_path_buf())).unwrap();
    let iid = "iid-remove-mcp";
    let inst = fixture_inst(iid, root.path());
    store.upsert_installation(inst.clone()).unwrap();
    let mut map = store.load_managed_map().unwrap();
    map.entries.push(ManagedEntry {
        usage_key: "uk".into(),
        installation_id: iid.into(),
        config_path: root.path().join("cfg.json"),
        server_name: "srv".into(),
        original: LaunchSpec {
            command: "orig".into(),
            args: vec![],
            env: Default::default(),
            cwd: None,
        },
        config_sha256_at_write: "x".into(),
        managed_at: chrono::Utc::now(),
    });
    store.save_managed_map(&map).unwrap();

    // force なしは拒否
    assert!(matches!(
        dtm_core::ops::remove(&store, &inst, false),
        Err(dtm_core::Error::Config(_))
    ));
    assert_eq!(store.load_registry().unwrap().installations.len(), 1);

    // force ありは managed エントリを消して解除する (警告付き)
    let o = dtm_core::ops::remove(&store, &inst, true).unwrap();
    assert_eq!(o.removed_managed_entries, 1);
    assert_eq!(o.warnings.len(), 1);
    assert!(store.load_managed_map().unwrap().entries.is_empty());
    assert!(store.load_registry().unwrap().installations.is_empty());
}

//! MCP 設定ファイルの管理化・復元 (仕様 §13, §16)。
//! - 変更前にバックアップ。未知キー・環境変数を保持
//! - 読み込み後の外部編集は競合検知し古い内容で上書きしない
//! - ランチャー導入前の command/args を保存し管理解除時に復元

use crate::discover::{self, McpConfigFile};
use crate::error::{Error, Result};
use crate::id;
use crate::lock::InstallationLock;
use crate::models::{Installation, LaunchSpec, ManagedEntry};
use crate::paths::{self, Layout};
use crate::store::Store;
use chrono::Utc;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

/// 管理化: command を dtm.exe ランチャー経由へ書き換える。
/// original は managed-mcp.json に記録し、設定本体には残さない
/// (未知キーを他クライアントが厳格解釈する可能性を避ける)。
pub fn manage(
    store: &Store,
    inst: &Installation,
    cfg: &McpConfigFile,
    server_name: &str,
    original: &LaunchSpec,
) -> Result<()> {
    let _lock = InstallationLock::try_acquire(&store.layout, "mcp-config")?;
    let dtm_exe = launcher_exe_path(&store.layout);
    let mut raw = cfg.raw.clone();

    // 競合検知: 直前に読み取った sha と現在のファイル内容を照合 (仕様 §13)
    verify_unchanged(&cfg.path, &cfg.sha256)?;

    let servers = raw
        .get_mut("mcpServers")
        .and_then(|m| m.as_object_mut())
        .ok_or_else(|| Error::Config("mcpServers キーがありません".into()))?;
    let entry = servers
        .get_mut(server_name)
        .ok_or_else(|| Error::NotFound(format!("サーバー '{server_name}'")))?;
    let obj = entry
        .as_object_mut()
        .ok_or_else(|| Error::Config(format!("'{server_name}' がオブジェクトではありません")))?;

    // バックアップ → 書き換え
    // 起動引数は一意キー mcpkey:<hash> (config_path+server_name) を使い、
    // 同名サーバーが別プロジェクトに存在しても正しい installation へ解決する
    let usage_key = id::usage_key(&cfg.path, server_name);
    backup_config(&store.layout, &cfg.path)?;
    obj.insert(
        "command".into(),
        Value::String(dtm_exe.to_string_lossy().replace('/', "\\")),
    );
    let mut args = vec![
        Value::String("launch".into()),
        Value::String(format!("mcpkey:{usage_key}")),
        Value::String("--".into()),
    ];
    args.extend(original.args.iter().map(|a| Value::String(a.clone())));
    obj.insert("args".into(), Value::Array(args));

    write_config(&cfg.path, &raw)?;

    // 管理マップ記録
    let new_sha = paths::sha256_file(&cfg.path)?;
    let mut map = store.load_managed_map()?;
    map.entries.retain(|e| {
        !(e.server_name == server_name
            && paths::path_key(&e.config_path) == paths::path_key(&cfg.path))
    });
    map.entries.push(ManagedEntry {
        usage_key: usage_key.clone(),
        installation_id: inst.id.clone(),
        config_path: cfg.path.clone(),
        server_name: server_name.into(),
        original: original.clone(),
        config_sha256_at_write: new_sha,
        managed_at: Utc::now(),
    });
    store.save_managed_map(&map)?;
    store.push_event(&inst.id, "manage", &format!("MCP管理化: {server_name}"));
    crate::dlog!(
        "manage: {} → mcpkey:{} ({})",
        server_name,
        usage_key,
        inst.id
    );
    Ok(())
}

/// 管理解除: 元の起動設定を復元する。
/// strict=true のとき外部編集競合があれば中止。false なら警告して復元優先。
pub fn unmanage(
    store: &Store,
    cfg_path: &Path,
    server_name: &str,
    strict: bool,
) -> Result<LaunchSpec> {
    let _lock = InstallationLock::try_acquire(&store.layout, "mcp-config")?;
    let mut map = store.load_managed_map()?;
    let idx = map
        .entries
        .iter()
        .position(|e| {
            e.server_name == server_name
                && paths::path_key(&e.config_path) == paths::path_key(cfg_path)
        })
        .ok_or_else(|| Error::NotFound(format!("管理対象 '{server_name}' の記録がありません")))?;
    let entry = map.entries[idx].clone();

    // 現在ファイル読み取り + 競合検知
    let scope = discover::ConfigScope::User; // 復元はパスで特定、scope 表記は補助
    let cfg = discover::read_config(scope, cfg_path)?;
    if cfg.sha256 != entry.config_sha256_at_write {
        if strict {
            return Err(Error::Conflict(format!(
                "{server_name}: 管理化後に設定が外部編集されています。--force で復元優先"
            )));
        }
        crate::dlogw!("{server_name}: 外部編集を検出したが復元を優先");
    }

    let mut raw = cfg.raw.clone();
    backup_config(&store.layout, cfg_path)?;
    if let Some(obj) = raw
        .get_mut("mcpServers")
        .and_then(|m| m.as_object_mut())
        .and_then(|s| s.get_mut(server_name))
        .and_then(|e| e.as_object_mut())
    {
        obj.insert(
            "command".into(),
            Value::String(entry.original.command.clone()),
        );
        if entry.original.args.is_empty() {
            obj.remove("args");
        } else {
            obj.insert(
                "args".into(),
                Value::Array(
                    entry
                        .original
                        .args
                        .iter()
                        .map(|a| Value::String(a.clone()))
                        .collect(),
                ),
            );
        }
    }
    write_config(cfg_path, &raw)?;
    map.entries.remove(idx);
    store.save_managed_map(&map)?;
    crate::dlog!("unmanage: {} → 元起動設定へ復元", server_name);
    Ok(entry.original)
}

pub fn set_enabled(
    store: &Store,
    cfg_path: &Path,
    server_name: &str,
    enabled: bool,
    expected_disabled: Option<bool>,
) -> Result<()> {
    let _lock = InstallationLock::try_acquire(&store.layout, "mcp-config")?;
    let cfg = discover::read_config(discover::ConfigScope::User, cfg_path)?;
    let mut raw = cfg.raw.clone();
    let entry = raw
        .get_mut("mcpServers")
        .and_then(Value::as_object_mut)
        .and_then(|servers| servers.get_mut(server_name))
        .and_then(Value::as_object_mut)
        .ok_or_else(|| {
            Error::Config(format!(
                "MCP '{server_name}' の設定がありません、または形式が不正です"
            ))
        })?;
    let disabled = match entry.get("disabled") {
        Some(value) => value
            .as_bool()
            .ok_or_else(|| Error::Config("disabledは真偽値である必要があります".into()))?,
        None => false,
    };
    if expected_disabled.is_some_and(|expected| expected != disabled) {
        return Err(Error::Conflict(
            "有効状態が別の操作で変更されています。再読み込みしてください".into(),
        ));
    }
    if disabled != enabled {
        return Ok(());
    }
    entry.insert("disabled".into(), Value::Bool(!enabled));
    let mut map = store.load_managed_map()?;
    let affected: Vec<_> = map
        .entries
        .iter()
        .enumerate()
        .filter_map(|(index, entry)| {
            (paths::path_key(&entry.config_path) == paths::path_key(cfg_path)
                && entry.config_sha256_at_write == cfg.sha256)
                .then_some(index)
        })
        .collect();
    verify_unchanged(cfg_path, &cfg.sha256)?;
    backup_config(&store.layout, cfg_path)?;
    verify_unchanged(cfg_path, &cfg.sha256)?;
    let sha = paths::sha256_bytes(&serde_json::to_vec_pretty(&raw)?);
    write_config(cfg_path, &raw)?;
    if !affected.is_empty() {
        for index in affected {
            map.entries[index].config_sha256_at_write = sha.clone();
        }
        store.save_managed_map(&map)?;
    }
    Ok(())
}

/// 設定ファイルが最後に読み取られた時点から変わっていないか確認
fn verify_unchanged(path: &Path, expected_sha: &str) -> Result<()> {
    let cur = paths::sha256_file(path)?;
    if cur != expected_sha {
        return Err(Error::Conflict(format!(
            "{}: 読み取り後に外部編集されています。再読み込みしてやり直してください",
            path.display()
        )));
    }
    Ok(())
}

/// 変更前バックアップ (仕様 §13)
pub fn backup_config(layout: &Layout, path: &Path) -> Result<PathBuf> {
    let dir = layout.backups_dir().join("mcp_config");
    fs::create_dir_all(&dir)?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "mcp_config.json".into());
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dest = dir.join(format!(
        "{}.{}-{}-{}-{}",
        name,
        Utc::now().format("%Y%m%d%H%M%S%3f"),
        std::process::id(),
        SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        id::usage_key(path, "backup")
    ));
    fs::copy(path, &dest)?;
    Ok(dest)
}

fn write_config(path: &Path, raw: &Value) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(raw)?;
    paths::atomic_write(path, &bytes)
}

/// ランチャーとして登録する dtm.exe のパス。
/// <root>/dtm.exe を正とする (インストール配置)。
pub fn launcher_exe_path(layout: &Layout) -> PathBuf {
    layout.root.join("dtm.exe")
}

#[cfg(test)]
mod enabled_tests {
    use super::*;

    #[test]
    fn toggle_preserves_unknown_fields_and_keeps_each_backup() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(Layout::new(temp.path().join("dtm"))).unwrap();
        let config = temp.path().join("mcp_config.json");
        let original = serde_json::json!({"mcpServers": {
            "demo": {"command":"node", "args":["demo.js"], "env":{"TEST":"keep"}, "disabledTools":["one"], "unknown":{"keep":true}},
            "other": {"serverUrl":"http://localhost:1234/mcp"}
        }, "unknownRoot": true});
        fs::write(&config, serde_json::to_vec(&original).unwrap()).unwrap();
        set_enabled(&store, &config, "demo", false, Some(false)).unwrap();
        let disabled: Value = paths::read_json(&config).unwrap();
        assert_eq!(disabled["mcpServers"]["demo"]["disabled"], true);
        let mut expected = original.clone();
        expected["mcpServers"]["demo"]["disabled"] = Value::Bool(true);
        assert_eq!(disabled, expected);
        set_enabled(&store, &config, "demo", true, Some(true)).unwrap();
        let enabled: Value = paths::read_json(&config).unwrap();
        expected["mcpServers"]["demo"]["disabled"] = Value::Bool(false);
        assert_eq!(enabled, expected);
        let backups: Vec<_> = fs::read_dir(store.layout.backups_dir().join("mcp_config"))
            .unwrap()
            .collect();
        assert_eq!(backups.len(), 2);
        let values: Vec<Value> = backups
            .into_iter()
            .map(|entry| paths::read_json(&entry.unwrap().path()).unwrap())
            .collect();
        assert!(values.contains(&original));
        assert!(values.contains(&disabled));
    }

    #[test]
    fn stale_expected_state_is_rejected_without_writing() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(Layout::new(temp.path().join("dtm"))).unwrap();
        let config = temp.path().join("config.json");
        let bytes = br#"{"mcpServers":{"demo":{"command":"node","disabled":true}}}"#;
        fs::write(&config, bytes).unwrap();
        assert!(matches!(
            set_enabled(&store, &config, "demo", false, Some(false)),
            Err(Error::Conflict(_))
        ));
        assert_eq!(fs::read(&config).unwrap(), bytes);
    }

    #[test]
    fn managed_hash_tracks_own_changes_but_does_not_adopt_external_edits() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(Layout::new(temp.path().join("dtm"))).unwrap();
        let config = temp.path().join("config.json");
        fs::write(&config, br#"{"mcpServers":{"demo":{"command":"node"}}}"#).unwrap();
        let hash = paths::sha256_file(&config).unwrap();
        let mut map = store.load_managed_map().unwrap();
        map.entries.push(ManagedEntry {
            usage_key: id::usage_key(&config, "demo"),
            installation_id: "demo".into(),
            config_path: config.clone(),
            server_name: "demo".into(),
            original: LaunchSpec {
                command: "node".into(),
                args: vec![],
                env: Default::default(),
                cwd: None,
            },
            config_sha256_at_write: hash,
            managed_at: Utc::now(),
        });
        store.save_managed_map(&map).unwrap();
        set_enabled(&store, &config, "demo", false, Some(false)).unwrap();
        let own_hash = paths::sha256_file(&config).unwrap();
        assert_eq!(
            store.load_managed_map().unwrap().entries[0].config_sha256_at_write,
            own_hash
        );
        let mut external: Value = paths::read_json(&config).unwrap();
        external["external"] = Value::Bool(true);
        paths::atomic_write_json(&config, &external).unwrap();
        set_enabled(&store, &config, "demo", true, Some(true)).unwrap();
        assert_eq!(
            store.load_managed_map().unwrap().entries[0].config_sha256_at_write,
            own_hash
        );
    }
}

//! MCP サーバーへの短命プローブ (tools/list 取得)。
//!
//! Devin Next 1.126 でエージェント側ログに tools/list 応答が出なくなったため、
//! stdio 型の MCP サーバーを一時的に spawn し initialize + tools/list の
//! JSON-RPC ハンドシェイクでツール数を取得する。
//! 結果は `state/mcp-probe-cache.json` にキャッシュする
//! (設定指紋が変わる・TTL 超過・前回失敗でない限り再プローブしない)。

use crate::discover;
use crate::error::{Error, Result};
use crate::id;
use crate::launcher;
use crate::models::LaunchSpec;
use crate::paths::{self, Layout};
use crate::procutil;
use crate::store::Store;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration as StdDuration, Instant};

/// キャッシュの有効期間。設定指紋が同一でもこの期間を超えたら再プローブする
/// (サーバー側のツール増減・外部更新を一定期間で追従するため)
const CACHE_TTL_HOURS: i64 = 24;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProbeOutcome {
    pub server_name: String,
    pub config_path: String,
    /// "stdio" | "remote" | "disabled"
    pub transport: String,
    pub tools: Option<usize>,
    pub server_version: Option<String>,
    pub server_display: Option<String>,
    pub error: Option<String>,
    /// キャッシュ由来 (実プローブしていない)
    pub cached: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ProbeCache {
    pub schema_version: u32,
    #[serde(default)]
    pub entries: Vec<ProbeCacheEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProbeCacheEntry {
    pub usage_key: String,
    pub server_name: String,
    pub config_path: String,
    /// 起動指定 (command+args+env+cwd) のハッシュ。変わったら無効
    pub fingerprint: String,
    pub tools: Option<usize>,
    pub server_version: Option<String>,
    pub server_display: Option<String>,
    pub error: Option<String>,
    pub probed_at: DateTime<Utc>,
}

fn cache_file(layout: &Layout) -> std::path::PathBuf {
    layout.state_dir().join("mcp-probe-cache.json")
}

pub fn load_cache(layout: &Layout) -> ProbeCache {
    paths::read_json::<ProbeCache>(&cache_file(layout)).unwrap_or_else(|_| ProbeCache {
        schema_version: 1,
        entries: vec![],
    })
}

fn save_cache(layout: &Layout, cache: &ProbeCache) -> Result<()> {
    paths::atomic_write_json(&cache_file(layout), cache)
}

/// 起動指定の指紋 (設定が変わったらキャッシュを無効化するため)
fn fingerprint(launch: &LaunchSpec) -> String {
    let mut env: Vec<(&String, &String)> = launch.env.iter().collect();
    env.sort();
    let material = format!(
        "{}\0{}\0{}\0{}",
        launch.command,
        launch.args.join("\0"),
        env.iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("\0"),
        launch
            .cwd
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
    );
    paths::sha256_bytes(material.as_bytes())
}

/// 実プローブ対象の起動指定を確定する。
/// `dtm.exe launch mcpkey:<key>` 型は実体 exe+元引数へ解決する
/// (dtm.exe 二段起動を避け、kill 時の孤児化を防ぐ)。
fn resolve_spawn(store: &Store, launch: &LaunchSpec) -> Result<SpawnSpec> {
    let is_launcher = launch.command.to_lowercase().contains("dtm.exe")
        && launch.args.first().map(|a| a == "launch").unwrap_or(false);
    if is_launcher {
        if let Some(key) = launch.args.get(1).and_then(|a| a.strip_prefix("mcpkey:")) {
            let map = store.load_managed_map()?;
            let entry = map
                .entries
                .iter()
                .find(|e| e.usage_key == key)
                .ok_or_else(|| Error::NotFound(format!("mcpkey '{key}' の管理エントリが無い")))?;
            let exe = launcher::resolve_path_only(store, &format!("mcpkey:{key}"))?;
            return Ok(SpawnSpec {
                command: exe.display().to_string(),
                args: entry.original.args.clone(),
                env: launch.env.clone(),
                cwd: launch.cwd.clone(),
            });
        }
    }
    // 裸コマンド名 (npx/uvx/node 等) は spawn が PATHEXT を適用しないため
    // PATH 解決して実パスで起動する (拡張子の優先順位 .exe > .cmd > .bat)
    let command = match resolve_spawn_command(&launch.command) {
        Some(p) => p.display().to_string(),
        None => {
            return Err(Error::NotFound(format!(
                "コマンド '{}' を PATH 上に解決できません",
                launch.command
            )))
        }
    };
    Ok(SpawnSpec {
        command,
        args: launch.args.clone(),
        env: launch.env.clone(),
        cwd: launch.cwd.clone(),
    })
}

/// launch.command を実パスへ解決する。
/// 絶対パス/パス区切りを含む相対パスはそのまま、裸コマンド名は PATH 検索。
fn resolve_spawn_command(cmd: &str) -> Option<std::path::PathBuf> {
    let p = Path::new(cmd);
    if p.is_absolute() || cmd.contains(['\\', '/']) {
        return p.exists().then(|| p.to_path_buf());
    }
    let mut all: Vec<std::path::PathBuf> = match which::which_all(cmd) {
        Ok(iter) => iter.collect(),
        Err(_) => vec![],
    };
    if all.is_empty() {
        return None;
    }
    let rank = |p: &std::path::PathBuf| match p
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

struct SpawnSpec {
    command: String,
    args: Vec<String>,
    env: std::collections::BTreeMap<String, String>,
    cwd: Option<std::path::PathBuf>,
}

struct ProbeResult {
    tools: Option<usize>,
    server_version: Option<String>,
    server_display: Option<String>,
    error: Option<String>,
}

/// MCP stdio ハンドシェイク: initialize → notifications/initialized → tools/list。
/// 応答は NDJSON (1行=1メッセージ)。全体 deadline を越えたら子を kill して打ち切る。
fn handshake(spec: &SpawnSpec, timeout: StdDuration) -> ProbeResult {
    let deadline = Instant::now() + timeout;
    let mut cmd = Command::new(&spec.command);
    cmd.args(&spec.args)
        .envs(&spec.env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(cwd) = &spec.cwd {
        cmd.current_dir(cwd);
    }
    procutil::hide_window(&mut cmd);
    let mut child: Child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return ProbeResult {
                tools: None,
                server_version: None,
                server_display: None,
                error: Some(format!("起動失敗: {e}")),
            }
        }
    };
    let stdout = match child.stdout.take() {
        Some(s) => s,
        None => {
            let _ = child.kill();
            return ProbeResult {
                tools: None,
                server_version: None,
                server_display: None,
                error: Some("stdout を開けません".into()),
            };
        }
    };
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            match line {
                Ok(l) => {
                    if tx.send(l).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let send = |child: &mut Child, v: &Value| -> std::result::Result<(), String> {
        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| "stdin を開けません".to_string())?;
        let mut s = serde_json::to_string(v).map_err(|e| e.to_string())?;
        s.push('\n');
        stdin.write_all(s.as_bytes()).map_err(|e| e.to_string())?;
        stdin.flush().map_err(|e| e.to_string())
    };

    let mut server_version = None;
    let mut server_display = None;
    let mut tools = None;
    let mut error = None;

    let write_result = send(
        &mut child,
        &json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": {"name": "dtm-probe", "version": env!("CARGO_PKG_VERSION")},
            }
        }),
    );
    if let Err(e) = write_result {
        error = Some(format!("initialize 送信失敗: {e}"));
    }

    let mut sent_initialized = false;
    let mut sent_tools = false;
    while error.is_none() && (tools.is_none() || !sent_tools) {
        let remain = deadline.saturating_duration_since(Instant::now());
        if remain.is_zero() {
            error = Some(format!("タイムアウト ({}s)", timeout.as_secs()));
            break;
        }
        let line = match rx.recv_timeout(remain) {
            Ok(l) => l,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                error = Some(format!("タイムアウト ({}s)", timeout.as_secs()));
                break;
            }
            Err(_) => {
                error = Some("応答が途絶えました (プロセス終了)".into());
                break;
            }
        };
        let trimmed = line.trim();
        if trimmed.is_empty() || !trimmed.starts_with('{') {
            continue;
        }
        let v: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => continue,
        };
        // サーバー側通知/リクエスト (method 有り id 無し or 逆) は読み飛ばす
        let id = v.get("id").and_then(|i| i.as_i64());
        if let Some(e) = v.get("error") {
            if id == Some(1) || id == Some(2) {
                error = Some(format!("RPC エラー: {}", e));
                break;
            }
        }
        if id == Some(1) {
            let info = v.pointer("/result/serverInfo");
            server_display = info
                .and_then(|i| i.get("name"))
                .and_then(|n| n.as_str())
                .map(|s| s.to_string());
            server_version = info
                .and_then(|i| i.get("version"))
                .and_then(|n| n.as_str())
                .map(|s| s.to_string());
            if let Err(e) = send(
                &mut child,
                &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
            )
            .and_then(|_| {
                send(
                    &mut child,
                    &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}),
                )
            }) {
                error = Some(format!("tools/list 送信失敗: {e}"));
                break;
            }
            sent_initialized = true;
            sent_tools = true;
        } else if id == Some(2) {
            tools = v
                .pointer("/result/tools")
                .and_then(|t| t.as_array())
                .map(|a| a.len());
            if tools.is_none() {
                error = Some("tools/list 応答に tools 配列がありません".into());
            }
            break;
        }
    }
    let _ = sent_initialized;
    let _ = child.kill();
    let _ = child.wait();
    ProbeResult {
        tools,
        server_version,
        server_display,
        error,
    }
}

/// MCP サーバーをプローブする。結果をキャッシュへ記録する。
/// remote/disabled は spawn せずその区分を返す。
pub fn probe_server(
    store: &Store,
    config_path: &Path,
    server_name: &str,
    timeout_secs: u64,
    no_cache: bool,
) -> Result<ProbeOutcome> {
    let cfg = discover::read_config(discover::ConfigScope::User, config_path)?;
    let entry = discover::list_servers(&cfg)
        .into_iter()
        .find(|s| s.name == server_name)
        .ok_or_else(|| {
            Error::NotFound(format!(
                "サーバー '{server_name}' が {} にありません",
                config_path.display()
            ))
        })?;

    let usage_key = id::usage_key(config_path, server_name);
    let base = ProbeOutcome {
        server_name: server_name.into(),
        config_path: config_path.display().to_string(),
        transport: entry.transport.clone(),
        tools: None,
        server_version: None,
        server_display: None,
        error: None,
        cached: false,
    };
    if entry.disabled {
        return Ok(ProbeOutcome {
            transport: "disabled".into(),
            error: Some("設定が無効です".into()),
            ..base
        });
    }
    if entry.transport != "stdio" {
        return Ok(ProbeOutcome {
            error: Some("リモート型はプローブ対象外です".into()),
            ..base
        });
    }
    let launch = entry
        .launch
        .clone()
        .ok_or_else(|| Error::Config(format!("'{server_name}' に起動指定がありません")))?;
    let fp = fingerprint(&launch);

    // キャッシュ: 指紋一致・TTL内・前回成功 のみ再利用 (失敗は毎回試行する)
    let mut cache = load_cache(&store.layout);
    if !no_cache {
        if let Some(hit) = cache.entries.iter().find(|e| {
            e.usage_key == usage_key
                && e.fingerprint == fp
                && e.error.is_none()
                && Utc::now() - e.probed_at < Duration::hours(CACHE_TTL_HOURS)
        }) {
            return Ok(ProbeOutcome {
                tools: hit.tools,
                server_version: hit.server_version.clone(),
                server_display: hit.server_display.clone(),
                cached: true,
                ..base
            });
        }
    }

    let spec = resolve_spawn(store, &launch)?;
    let r = handshake(&spec, StdDuration::from_secs(timeout_secs.max(5)));

    // 結果をキャッシュへ (成否に関わらず記録。失敗は次回試行される)
    let now = Utc::now();
    let record = ProbeCacheEntry {
        usage_key,
        server_name: server_name.into(),
        config_path: config_path.display().to_string(),
        fingerprint: fp,
        tools: r.tools,
        server_version: r.server_version.clone(),
        server_display: r.server_display.clone(),
        error: r.error.clone(),
        probed_at: now,
    };
    cache.entries.retain(|e| e.usage_key != record.usage_key);
    cache.entries.push(record);
    if let Err(e) = save_cache(&store.layout, &cache) {
        crate::dlogw!("probe cache 保存失敗: {e}");
    }
    crate::dlog!(
        "probe-mcp: {server_name} → tools={:?} err={:?}",
        r.tools,
        r.error
    );
    Ok(ProbeOutcome {
        tools: r.tools,
        server_version: r.server_version,
        server_display: r.server_display,
        error: r.error,
        ..base
    })
}

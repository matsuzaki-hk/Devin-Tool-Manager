//! MCP 設定の検出・読み取り・サーバー分類 (仕様 §4.2, §2)。
//! - ユーザースコープ: %APPDATA%\devin\mcp_config.json
//! - プロジェクトスコープ: 利用者が登録・開いているプロジェクトの
//!   .devin/mcp_config.json, .devin/mcp_config.local.json
//! - PC 全体を無断で再帰探索しない。探索範囲は呼び出し側が明示する。

use crate::error::Result;
use crate::models::{LaunchSpec, ToolKind};
use crate::paths;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// 設定ファイルのスコープ
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigScope {
    User,
    Project,
    Local,
}

impl ConfigScope {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Project => "project",
            Self::Local => "local",
        }
    }
}

#[derive(Debug, Clone)]
pub struct McpConfigFile {
    pub scope: ConfigScope,
    pub path: PathBuf,
    /// mcpServers 直下の生JSON (未知キー保持のため Value で扱う。仕様 §13)
    pub raw: Value,
    /// 読み取り時点の SHA-256 (外部編集の競合検知用)
    pub sha256: String,
}

#[derive(Debug, Clone)]
pub struct McpServerEntry {
    pub scope: ConfigScope,
    pub config_path: PathBuf,
    pub name: String,
    /// "stdio" | "http" | "sse"
    pub transport: String,
    /// stdio の場合の起動指定
    pub launch: Option<LaunchSpec>,
    /// リモートの場合の URL
    pub url: Option<String>,
    pub disabled: bool,
    /// command/args/env 以外の未知キー (保持対象)
    pub extra_keys: Vec<String>,
}

/// ユーザー MCP 設定の既定パス (%APPDATA%\devin\mcp_config.json)
pub fn user_config_path() -> Result<PathBuf> {
    let base = dirs::config_dir()
        .or_else(dirs::data_dir)
        .ok_or_else(|| crate::error::Error::Config("APPDATA を解決できません".into()))?;
    Ok(base.join("devin").join("mcp_config.json"))
}

/// プロジェクトルートの MCP 設定 (存在するもののみ)
pub fn project_config_paths(project_root: &Path) -> Vec<(ConfigScope, PathBuf)> {
    let mut v = vec![];
    let devin = project_root.join(".devin");
    for (scope, name) in [
        (ConfigScope::Project, "mcp_config.json"),
        (ConfigScope::Local, "mcp_config.local.json"),
    ] {
        let p = devin.join(name);
        if p.exists() {
            v.push((scope, p));
        }
    }
    v
}

/// 設定ファイル読み取り (未知キーを保持するため生 Value + sha256 を保持)
pub fn read_config(scope: ConfigScope, path: &Path) -> Result<McpConfigFile> {
    let bytes = std::fs::read(path)?;
    let raw: Value = serde_json::from_slice(&bytes)?;
    Ok(McpConfigFile {
        scope,
        path: path.to_path_buf(),
        raw,
        sha256: paths::sha256_bytes(&bytes),
    })
}

/// 設定ファイル内のサーバーエントリを列挙・分類
pub fn list_servers(cfg: &McpConfigFile) -> Vec<McpServerEntry> {
    let mut out = vec![];
    let Some(map) = cfg.raw.get("mcpServers").and_then(|v| v.as_object()) else {
        return out;
    };
    for (name, v) in map {
        let obj = match v.as_object() {
            Some(o) => o,
            None => continue,
        };
        let disabled = obj
            .get("disabled")
            .and_then(|d| d.as_bool())
            .unwrap_or(false);
        let url = obj
            .get("url")
            .and_then(|u| u.as_str())
            .or_else(|| obj.get("serverUrl").and_then(|u| u.as_str()))
            .map(String::from);
        let command = obj.get("command").and_then(|c| c.as_str());

        let (transport, launch) = if let Some(cmd) = command {
            let args = obj
                .get("args")
                .and_then(|a| a.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            let env: BTreeMap<String, String> = obj
                .get("env")
                .and_then(|e| e.as_object())
                .map(|e| {
                    e.iter()
                        .filter_map(|(k, x)| x.as_str().map(|s| (k.clone(), s.to_string())))
                        .collect()
                })
                .unwrap_or_default();
            let cwd = obj.get("cwd").and_then(|c| c.as_str()).map(PathBuf::from);
            (
                "stdio".to_string(),
                Some(LaunchSpec {
                    command: cmd.to_string(),
                    args,
                    env,
                    cwd,
                }),
            )
        } else {
            let t = obj
                .get("transport")
                .and_then(|t| t.as_str())
                .unwrap_or("http")
                .to_string();
            (t, None)
        };

        let known: std::collections::HashSet<&str> = [
            "command",
            "args",
            "env",
            "cwd",
            "url",
            "transport",
            "disabled",
        ]
        .into_iter()
        .collect();
        let extra_keys = obj
            .keys()
            .filter(|k| !known.contains(k.as_str()))
            .cloned()
            .collect();

        out.push(McpServerEntry {
            scope: cfg.scope,
            config_path: cfg.path.clone(),
            name: name.clone(),
            transport,
            launch,
            url,
            disabled,
            extra_keys,
        });
    }
    out
}

/// サーバーの管理種別を決定 (仕様 §4.1)
pub fn classify(entry: &McpServerEntry) -> ToolKind {
    match entry.transport.as_str() {
        "stdio" => ToolKind::McpStdio,
        _ => ToolKind::McpRemote,
    }
}

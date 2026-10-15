//! installationId 生成 (仕様 §4.2)。
//! 排他単位は表示名やリポジトリ名ではなく、正規化した配置先と実行環境に紐付く。

use crate::paths::path_key;
use sha2::Digest;
use std::path::Path;

/// 実行環境識別子。初期対象は Windows ローカルのみ (仕様 §2)。
/// WSL/SSH/コンテナーは別環境として別 ID になるよう将来 "wsl:..." 等を使う。
pub fn current_environment() -> String {
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        other => other,
    };
    format!("{}-{}", std::env::consts::OS, arch)
}

/// 正規化配置先 + 実行環境から安定 ID を生成。
/// 同じ実体を指す複数登録は同じ ID に集約され、別フォルダ導入は別 ID。
pub fn installation_id(install_root: &Path, environment: &str) -> String {
    let key = format!("{}|{}", path_key(install_root), environment.to_lowercase());
    let h = sha2::Sha256::digest(key.as_bytes());
    // 先頭 16 バイトを hex 化
    h[..16].iter().map(|b| format!("{b:02x}")).collect()
}

/// MCP 利用元の一意キー (config_path+server_name → 短ハッシュ)。
/// 同名サーバーが複数のプロジェクト/スコープに存在する場合の識別に使う。
pub fn usage_key(config_path: &Path, server_name: &str) -> String {
    let key = format!("{}|{}", path_key(config_path), server_name);
    let h = sha2::Sha256::digest(key.as_bytes());
    h[..8].iter().map(|b| format!("{b:02x}")).collect()
}

/// 表示名をファイルシステム安全なスラッグに (レシピファイル名等に使用)
pub fn slugify(name: &str) -> String {
    let mut s = String::with_capacity(name.len());
    for c in name.chars() {
        if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' {
            s.push(c.to_ascii_lowercase());
        } else if c.is_whitespace() {
            s.push('-');
        }
    }
    let s = s.trim_matches(['-', '.']).to_string();
    if s.is_empty() {
        "unnamed".into()
    } else {
        s.chars().take(64).collect()
    }
}

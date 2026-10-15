//! 認証情報 (仕様 §13)。
//! GitHub トークンはレシピ・ログ・設定ファイルに書かず、
//! 環境変数または OS 資格情報ストア (Windows Credential Manager) から取得する。

pub const KEYRING_SERVICE: &str = "devin-tool-manager:github";

/// GitHub API トークン取得順:
/// 1. DTM_GITHUB_TOKEN
/// 2. GITHUB_TOKEN / GH_TOKEN
/// 3. OS 資格情報ストア (Windows: keyring/windows-native)
pub fn github_token() -> Option<String> {
    for var in ["DTM_GITHUB_TOKEN", "GITHUB_TOKEN", "GH_TOKEN"] {
        if let Ok(v) = std::env::var(var) {
            if !v.trim().is_empty() {
                return Some(v);
            }
        }
    }
    keyring_get("default")
}

#[cfg(feature = "keyring")]
fn keyring_get(user: &str) -> Option<String> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, user).ok()?;
    entry.get_password().ok()
}

#[cfg(not(feature = "keyring"))]
fn keyring_get(_user: &str) -> Option<String> {
    None
}

/// トークンを OS 資格情報ストアへ保存 (CLI `dtm auth github` 用)
#[cfg(feature = "keyring")]
pub fn store_github_token(token: &str) -> crate::error::Result<()> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, "default")
        .map_err(|e| crate::error::Error::Config(format!("資格情報ストア: {e}")))?;
    entry
        .set_password(token)
        .map_err(|e| crate::error::Error::Config(format!("資格情報ストア保存: {e}")))
}

#[cfg(not(feature = "keyring"))]
pub fn store_github_token(_token: &str) -> crate::error::Result<()> {
    Err(crate::error::Error::Config(
        "このビルドでは資格情報ストアが無効です。DTM_GITHUB_TOKEN を使用してください".into(),
    ))
}

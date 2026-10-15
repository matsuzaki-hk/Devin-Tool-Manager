#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O エラー: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON エラー: {0}")]
    Json(#[from] serde_json::Error),
    #[error("HTTP エラー: {0}")]
    Http(#[from] reqwest::Error),
    #[error("ZIP エラー: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("設定エラー: {0}")]
    Config(String),
    #[error("対象が見つかりません: {0}")]
    NotFound(String),
    #[error("他プロセスが処理中です: {0}")]
    Busy(String),
    #[error("タイムアウト: {0}")]
    Timeout(String),
    #[error("検証失敗: {0}")]
    Verify(String),
    #[error("安全チェック失敗: {0}")]
    Unsafe(String),
    #[error("更新競合: {0}")]
    Conflict(String),
    #[error("ユーザー操作が必要: {0}")]
    NeedUser(String),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn path_not_found(p: &std::path::Path) -> Self {
        Error::NotFound(p.display().to_string())
    }
}

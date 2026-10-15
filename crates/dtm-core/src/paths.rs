//! 配置ディレクトリ解決とファイルユーティリティ。
//! 仕様 §3(構成), §4.2(排他単位=正規化配置先), §13(原子的書き込み)

use crate::error::{Error, Result};
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

/// DTM ルートの解決順:
/// 1. `DTM_ROOT` 環境変数 / CLI `--root`
/// 2. 実行ファイルの親ディレクトリに `dtm.root` マーカーがある場合はそこ
///    (インストール配置時にマーカーを同梱する)
/// 3. `%LOCALAPPDATA%\Programs\Devin Next\DevinToolManager`
#[derive(Debug, Clone)]
pub struct Layout {
    pub root: PathBuf,
}

impl Layout {
    pub fn discover(cli_root: Option<&Path>) -> Result<Self> {
        if let Some(p) = cli_root {
            return Ok(Self::new(p.to_path_buf()));
        }
        if let Ok(p) = std::env::var("DTM_ROOT") {
            if !p.trim().is_empty() {
                return Ok(Self::new(PathBuf::from(p)));
            }
        }
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                if dir.join("dtm.root").exists() {
                    return Ok(Self::new(dir.to_path_buf()));
                }
            }
        }
        let base = dirs::data_local_dir()
            .ok_or_else(|| Error::Config("LOCALAPPDATA を解決できません".into()))?;
        Ok(Self::new(
            base.join("Programs")
                .join("Devin Next")
                .join("DevinToolManager"),
        ))
    }

    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn ensure(&self) -> Result<()> {
        for d in [
            &self.root,
            &self.state_dir(),
            &self.jobs_dir(),
            &self.locks_dir(),
            &self.logs_dir(),
            &self.recipes_dir(),
            &self.generations_dir(),
            &self.staging_dir(),
            &self.backups_dir(),
        ] {
            fs::create_dir_all(d)?;
        }
        Ok(())
    }

    pub fn state_dir(&self) -> PathBuf {
        self.root.join("state")
    }
    pub fn jobs_dir(&self) -> PathBuf {
        self.state_dir().join("jobs")
    }
    pub fn locks_dir(&self) -> PathBuf {
        self.state_dir().join("locks")
    }
    pub fn logs_dir(&self) -> PathBuf {
        self.state_dir().join("logs")
    }
    pub fn recipes_dir(&self) -> PathBuf {
        self.root.join("recipes")
    }
    pub fn generations_dir(&self) -> PathBuf {
        self.root.join("generations")
    }
    pub fn staging_dir(&self) -> PathBuf {
        self.root.join("staging")
    }
    pub fn backups_dir(&self) -> PathBuf {
        self.root.join("backups")
    }

    pub fn registry_file(&self) -> PathBuf {
        self.state_dir().join("registry.json")
    }
    pub fn managed_map_file(&self) -> PathBuf {
        self.state_dir().join("managed-mcp.json")
    }
    pub fn install_state_file(&self, installation_id: &str) -> PathBuf {
        self.state_dir()
            .join("installations")
            .join(format!("{installation_id}.json"))
    }
    pub fn job_file(&self, installation_id: &str) -> PathBuf {
        self.jobs_dir().join(format!("{installation_id}.json"))
    }
    /// 進行中ジョブの進捗ファイル (ワーカースレッド単独書き込み用に分離)
    pub fn progress_file(&self, installation_id: &str) -> PathBuf {
        self.jobs_dir()
            .join(format!("{installation_id}.progress.json"))
    }
    pub fn lock_file(&self, installation_id: &str) -> PathBuf {
        self.locks_dir().join(format!("{installation_id}.lock"))
    }
    pub fn log_file(&self) -> PathBuf {
        self.logs_dir().join("dtm.log")
    }
    pub fn generations_base(&self, installation_id: &str) -> PathBuf {
        self.generations_dir().join(installation_id)
    }
    pub fn generation_dir(&self, installation_id: &str, gen: &str) -> PathBuf {
        self.generations_base(installation_id).join(gen)
    }
    /// 有効世代ポインタ (原子的に切替。仕様 §6.1)
    pub fn active_pointer(&self, installation_id: &str) -> PathBuf {
        self.generations_base(installation_id).join("active.json")
    }
    pub fn recipe_file(&self, recipe_id: &str) -> PathBuf {
        self.recipes_dir().join(format!("{recipe_id}.json"))
    }
}

/// パス正規化 (仕様 §4.2): 絶対化、環境変数展開、区切り統一、
/// 末尾区切り除去、Windows 大小文字統一(ドライブ+全要素小文字化比較用)。
/// リンク解決はできれば canonicalize、存在しなくても構文正規化を行う。
pub fn normalize_path(p: &Path) -> PathBuf {
    // 存在する場合は canonicalize (シンボリックリンク・8.3短縮・大小文字を実体に合わせる)
    if let Ok(c) = p.canonicalize() {
        return strip_verbatim(c);
    }
    // 存在しない場合は構文的に正規化
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(p)
    };
    let mut out = PathBuf::new();
    for c in abs.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    strip_verbatim(out)
}

fn strip_verbatim(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        p
    }
}

/// 比較用キー: Windows は大小文字を区別しないため小文字化
pub fn path_key(p: &Path) -> String {
    normalize_path(p)
        .to_string_lossy()
        .replace('/', "\\")
        .to_lowercase()
}

/// 原子的ファイル書き込み: 同一ディレクトリ内の tmp へ書き fsync 後、
/// 宛先へ rename する。rename は同一ボリューム内で原子的。
///
/// Windows の `std::fs::rename` は `MoveFileExW` に
/// `MOVEFILE_REPLACE_EXISTING | MOVEFILE_COPY_ALLOWED | MOVEFILE_WRITE_THROUGH`
/// を指定する実装のため、既存の宛先を単一操作で原子的に置き換える。
/// (旧来の「退避→rename」方式は宛先が一時的に消失するため読者が
///  状態ファイルを見失う問題があった)
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| Error::Config(format!("親ディレクトリなし: {}", path.display())))?;
    fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(
        ".{}.tmp-{}",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    match fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(e.into())
        }
    }
}

pub fn atomic_write_json<T: serde::Serialize>(path: &Path, v: &T) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(v)?;
    atomic_write(path, &bytes)
}

pub fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let bytes = fs::read(path)?;
    Ok(serde_json::from_slice(&bytes)?)
}

pub fn sha256_file(path: &Path) -> Result<String> {
    use sha2::Digest;
    let mut f = fs::File::open(path)?;
    let mut h = sha2::Sha256::new();
    std::io::copy(&mut f, &mut h)?;
    Ok(hex_lower(&h.finalize()))
}

pub fn sha256_bytes(b: &[u8]) -> String {
    use sha2::Digest;
    hex_lower(&sha2::Sha256::digest(b))
}

fn hex_lower(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

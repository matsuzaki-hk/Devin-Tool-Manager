//! ZIP 安全展開 (仕様 §6-4)。
//! - 絶対パス、親ディレクトリ逸脱 (..)、ドライブレター、UNC を拒否
//! - 危険なリンク (UNIX シンボリックリンク属性) を拒否
//! - 展開量上限 (zip bomb 対策)、ファイル数上限

use crate::error::{Error, Result};
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

pub struct ZipLimits {
    /// 展開後合計サイズ上限 (既定 2 GiB)
    pub max_total_bytes: u64,
    /// ファイル数上限
    pub max_files: usize,
}

impl Default for ZipLimits {
    fn default() -> Self {
        Self {
            max_total_bytes: 2 * 1024 * 1024 * 1024,
            max_files: 20_000,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ZipEntryInfo {
    /// アーカイブ内相対パス (`/` 区切りに正規化)
    pub rel: String,
    pub is_dir: bool,
    pub size: u64,
}

/// 展開前に全エントリの安全性を検査し、一覧を返す (プレビューにも使用)
pub fn inspect(archive_path: &Path, limits: &ZipLimits) -> Result<Vec<ZipEntryInfo>> {
    let f = fs::File::open(archive_path)?;
    let mut zip = zip::ZipArchive::new(f)?;
    let mut entries = vec![];
    let mut total: u64 = 0;
    for i in 0..zip.len() {
        let file = zip.by_index(i)?;
        let raw_name = file.name().replace('\\', "/");
        if file.name_raw().is_empty() {
            continue;
        }
        check_safe_name(&raw_name)?;
        check_not_symlink(&file)?;
        total = total.saturating_add(file.size());
        if total > limits.max_total_bytes {
            return Err(Error::Unsafe(format!(
                "展開量が上限 {} バイトを超過 (zip bomb の可能性)",
                limits.max_total_bytes
            )));
        }
        if i + 1 > limits.max_files {
            return Err(Error::Unsafe(format!(
                "ファイル数が上限 {} を超過",
                limits.max_files
            )));
        }
        entries.push(ZipEntryInfo {
            rel: raw_name.clone(),
            is_dir: file.is_dir() || raw_name.ends_with('/'),
            size: file.size(),
        });
    }
    Ok(entries)
}

/// 安全検査を行ったうえで dest へ展開する。
/// 戻り値: 展開された相対パス一覧 (プレビューとの一致確認用)
pub fn extract(archive_path: &Path, dest: &Path, limits: &ZipLimits) -> Result<Vec<String>> {
    let _entries = inspect(archive_path, limits)?;
    let f = fs::File::open(archive_path)?;
    let mut zip = zip::ZipArchive::new(f)?;
    fs::create_dir_all(dest)?;
    let mut written = vec![];
    for i in 0..zip.len() {
        let mut file = zip.by_index(i)?;
        let raw_name = file.name().replace('\\', "/");
        if raw_name.is_empty() {
            continue;
        }
        let rel = sanitize_rel(&raw_name)?;
        let out = dest.join(&rel);
        // 最終防衛: 正規化後に dest 外に出ていないか確認
        let out_norm = crate::paths::normalize_path(&out);
        let dest_norm = crate::paths::normalize_path(dest);
        if !crate::paths::path_key(&out_norm).starts_with(&crate::paths::path_key(&dest_norm)) {
            return Err(Error::Unsafe(format!("展開先逸脱を検出: {raw_name}")));
        }
        if file.is_dir() || raw_name.ends_with('/') {
            fs::create_dir_all(&out)?;
            continue;
        }
        if let Some(parent) = out.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut w = fs::File::create(&out)?;
        io::copy(&mut file, &mut w)?;
        // 実行ビットは Windows では不要 (拡張子で判定)
        written.push(rel.to_string_lossy().replace('\\', "/"));
    }
    Ok(written)
}

fn check_safe_name(name: &str) -> Result<()> {
    let p = Path::new(name);
    if p.is_absolute() || name.starts_with('/') || name.starts_with("//") {
        return Err(Error::Unsafe(format!("絶対パスエントリ: {name}")));
    }
    if name.len() >= 2 && name.as_bytes()[1] == b':' {
        return Err(Error::Unsafe(format!("ドライブレターパス: {name}")));
    }
    for c in p.components() {
        match c {
            Component::ParentDir => {
                return Err(Error::Unsafe(format!("親ディレクトリ逸脱: {name}")));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(Error::Unsafe(format!("危険なパス要素: {name}")));
            }
            _ => {}
        }
    }
    Ok(())
}

fn sanitize_rel(name: &str) -> Result<PathBuf> {
    check_safe_name(name)?;
    let mut out = PathBuf::new();
    for c in Path::new(name).components() {
        if let Component::Normal(s) = c {
            out.push(s);
        }
    }
    if out.as_os_str().is_empty() {
        return Err(Error::Unsafe(format!("空のパス: {name}")));
    }
    Ok(out)
}

fn check_not_symlink(file: &zip::read::ZipFile) -> Result<()> {
    // UNIX モードの上位4ビット: 0o120000 = symlink
    if let Some(mode) = file.unix_mode() {
        if mode & 0o170000 == 0o120000 {
            return Err(Error::Unsafe(format!(
                "シンボリックリンクエントリ: {}",
                file.name()
            )));
        }
    }
    Ok(())
}

/// ZIP らしさの判定 (拡張子+マジックバイト)
pub fn is_zip(path: &Path) -> bool {
    if let Some(ext) = path.extension() {
        if ext.to_string_lossy().eq_ignore_ascii_case("zip") {
            return true;
        }
    }
    if let Ok(mut f) = fs::File::open(path) {
        use std::io::Read;
        let mut sig = [0u8; 4];
        if f.read_exact(&mut sig).is_ok() {
            return &sig[..2] == b"PK";
        }
    }
    false
}

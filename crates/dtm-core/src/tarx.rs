//! tar.gz 安全展開 (仕様 §6-4)。zipx.rs と同じ検査構造を tarball 用に実装。
//! - 絶対パス、親ディレクトリ逸脱 (..)、ドライブレター、UNC を拒否
//! - シンボリックリンク・ハードリンク・デバイス/FIFO エントリを拒否
//! - 展開量上限 (bomb 対策)、ファイル数上限

use crate::error::{Error, Result};
use flate2::read::GzDecoder;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use tar::{Archive, EntryType};

pub struct TarLimits {
    /// 展開後合計サイズ上限 (既定 2 GiB)
    pub max_total_bytes: u64,
    /// ファイル数上限
    pub max_files: usize,
}

impl Default for TarLimits {
    fn default() -> Self {
        Self {
            max_total_bytes: 2 * 1024 * 1024 * 1024,
            max_files: 20_000,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TarEntryInfo {
    /// アーカイブ内相対パス (`/` 区切りに正規化)
    pub rel: String,
    pub is_dir: bool,
    pub size: u64,
}

fn open(path: &Path) -> Result<Archive<GzDecoder<fs::File>>> {
    let f = fs::File::open(path)?;
    Ok(Archive::new(GzDecoder::new(f)))
}

/// 展開前に全エントリの安全性を検査し、一覧を返す (プレビューにも使用)
pub fn inspect(archive_path: &Path, limits: &TarLimits) -> Result<Vec<TarEntryInfo>> {
    let mut ar = open(archive_path)?;
    let mut entries = vec![];
    let mut total: u64 = 0;
    for (i, entry) in ar.entries()?.enumerate() {
        let entry = entry?;
        let raw_name = entry
            .path()
            .map_err(|e| Error::Unsafe(format!("エントリ名が不正: {e}")))?
            .to_string_lossy()
            .replace('\\', "/");
        if raw_name.is_empty() {
            continue;
        }
        check_safe_name(&raw_name)?;
        check_safe_type(&entry, &raw_name)?;
        total = total.saturating_add(entry.size());
        if total > limits.max_total_bytes {
            return Err(Error::Unsafe(format!(
                "展開量が上限 {} バイトを超過 (bomb の可能性)",
                limits.max_total_bytes
            )));
        }
        if i + 1 > limits.max_files {
            return Err(Error::Unsafe(format!(
                "ファイル数が上限 {} を超過",
                limits.max_files
            )));
        }
        let is_dir = entry.header().entry_type() == EntryType::Directory || raw_name.ends_with('/');
        entries.push(TarEntryInfo {
            rel: raw_name,
            is_dir,
            size: entry.size(),
        });
    }
    Ok(entries)
}

/// 安全検査を行ったうえで dest へ展開する。
/// 戻り値: 展開された相対パス一覧 (プレビューとの一致確認用)
pub fn extract(archive_path: &Path, dest: &Path, limits: &TarLimits) -> Result<Vec<String>> {
    let _entries = inspect(archive_path, limits)?;
    let mut ar = open(archive_path)?;
    fs::create_dir_all(dest)?;
    let mut written = vec![];
    for entry in ar.entries()? {
        let mut entry = entry?;
        let raw_name = entry
            .path()
            .map_err(|e| Error::Unsafe(format!("エントリ名が不正: {e}")))?
            .to_string_lossy()
            .replace('\\', "/");
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
        if entry.header().entry_type() == EntryType::Directory || raw_name.ends_with('/') {
            fs::create_dir_all(&out)?;
            continue;
        }
        if let Some(parent) = out.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut w = fs::File::create(&out)?;
        io::copy(&mut entry, &mut w)?;
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

/// npm tarball 等で出現し得るリンク・特殊ファイルを拒否する。
/// Regular/Directory/GNU 拡張ヘッダ等のみ許可。
fn check_safe_type(entry: &tar::Entry<'_, GzDecoder<fs::File>>, name: &str) -> Result<()> {
    match entry.header().entry_type() {
        EntryType::Symlink => Err(Error::Unsafe(format!("シンボリックリンクエントリ: {name}"))),
        EntryType::Link => Err(Error::Unsafe(format!("ハードリンクエントリ: {name}"))),
        EntryType::Char | EntryType::Block | EntryType::Fifo => {
            Err(Error::Unsafe(format!("特殊ファイルエントリ: {name}")))
        }
        _ => Ok(()),
    }
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

/// gzip 圧縮 tar らしさの判定 (拡張子+gzip マジックバイト 1f 8b)
pub fn is_targz(path: &Path) -> bool {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if name.ends_with(".tgz") || name.ends_with(".tar.gz") {
        return true;
    }
    if let Ok(mut f) = fs::File::open(path) {
        use std::io::Read;
        let mut sig = [0u8; 2];
        if f.read_exact(&mut sig).is_ok() {
            return sig == [0x1f, 0x8b];
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use tar::Builder;

    /// テスト用 tar.gz を (エントリ名, 内容, EntryType) リストから構築
    fn make_tgz(entries: &[(&str, &[u8], EntryType)]) -> Vec<u8> {
        let enc = GzEncoder::new(Vec::new(), Compression::fast());
        let mut b = Builder::new(enc);
        for (name, data, ty) in entries {
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(*ty);
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            b.append_data(&mut h, name, *data).unwrap();
        }
        b.into_inner().unwrap().finish().unwrap()
    }

    /// Builder は危険パスを自前で検証してしまうため、悪意エントリは
    /// tar ヘッダを生バイトで組み立てて生成する。
    fn make_raw_tgz(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let octal = |v: u64, w: usize| -> Vec<u8> {
            let mut s = format!("{:o}", v);
            while s.len() < w - 1 {
                s.insert(0, '0');
            }
            s.push('\0');
            s.into_bytes()
        };
        let mut tar = Vec::new();
        for (name, data) in entries {
            let mut h = [0u8; 512];
            let nb = name.as_bytes();
            h[..nb.len()].copy_from_slice(nb);
            h[100..108].copy_from_slice(b"0000644\0");
            h[108..116].copy_from_slice(b"0000000\0");
            h[116..124].copy_from_slice(b"0000000\0");
            h[124..136].copy_from_slice(&octal(data.len() as u64, 12));
            h[136..148].copy_from_slice(&octal(0, 12));
            h[148..156].copy_from_slice(b"        ");
            h[156] = b'0';
            h[257..263].copy_from_slice(b"ustar\0");
            h[263..265].copy_from_slice(b"00");
            let sum: u64 = h.iter().map(|&b| b as u64).sum();
            let field = format!("{:06o}\0 ", sum);
            h[148..156].copy_from_slice(field.as_bytes());
            tar.extend_from_slice(&h);
            tar.extend_from_slice(data);
            let pad = (512 - data.len() % 512) % 512;
            tar.extend(std::iter::repeat_n(0u8, pad));
        }
        tar.extend(std::iter::repeat_n(0u8, 1024));
        let mut enc = GzEncoder::new(Vec::new(), Compression::fast());
        std::io::Write::write_all(&mut enc, &tar).unwrap();
        enc.finish().unwrap()
    }

    #[test]
    fn extract_normal_package_layout() {
        let tgz = make_tgz(&[
            ("package/", b"", EntryType::Directory),
            ("package/scripts/rea.mjs", b"// rea", EntryType::Regular),
            ("package/dist/x.js", b"x", EntryType::Regular),
        ]);
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("pkg.tgz");
        fs::write(&p, &tgz).unwrap();
        assert!(is_targz(&p));
        let dest = dir.path().join("out");
        let written = extract(&p, &dest, &TarLimits::default()).unwrap();
        assert!(written.iter().any(|r| r == "package/scripts/rea.mjs"));
        assert_eq!(
            fs::read_to_string(dest.join("package/scripts/rea.mjs")).unwrap(),
            "// rea"
        );
    }

    #[test]
    fn rejects_parent_traversal() {
        let tgz = make_raw_tgz(&[("package/../evil.txt", b"x")]);
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("evil.tgz");
        fs::write(&p, &tgz).unwrap();
        assert!(matches!(
            extract(&p, &dir.path().join("out"), &TarLimits::default()),
            Err(Error::Unsafe(_))
        ));
    }

    #[test]
    fn rejects_symlink() {
        let tgz = make_tgz(&[("package/link", b"", EntryType::Symlink)]);
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("link.tgz");
        fs::write(&p, &tgz).unwrap();
        assert!(matches!(
            extract(&p, &dir.path().join("out"), &TarLimits::default()),
            Err(Error::Unsafe(_))
        ));
    }

    #[test]
    fn rejects_absolute() {
        let tgz = make_raw_tgz(&[("/abs/x.txt", b"x")]);
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("abs.tgz");
        fs::write(&p, &tgz).unwrap();
        assert!(matches!(
            extract(&p, &dir.path().join("out"), &TarLimits::default()),
            Err(Error::Unsafe(_))
        ));
    }
}

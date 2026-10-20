//! 取得と検証 (仕様 §6-2,3,4)。
//! - 一時領域(ステージング)へ取得
//! - 配布元が提供するチェックサム/署名があれば検証、不一致は拒否
//! - 提供がなければ「配布元署名未検証(unverified)」を記録し、成功扱いにしない

use crate::error::{Error, Result};
use crate::github::{Asset, GithubClient, Release};
use crate::models::VerificationResult;
use crate::paths;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub struct FetchOutcome {
    pub file: PathBuf,
    pub sha256: String,
    pub verification: VerificationResult,
}

/// ブロッキングI/O (reqwest blocking のボディ read) はソケット停滞時に
/// 無制限にブロックし得るため、ワーカースレッドで実行して deadline で
/// 呼び出し側を先に失敗終了させる。取り残したスレッドは one-shot CLI
/// プロセス終了時に OS が回収する (prepare 失敗→終了まで数秒以内)。
pub(crate) fn bounded<T, F>(what: &str, deadline: Instant, f: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    loop {
        match rx.try_recv() {
            Ok(r) => return r,
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                if Instant::now() > deadline {
                    return Err(Error::Timeout(format!(
                        "{what} が内部制限時間を超過しました（通信状態を確認して再試行してください）"
                    )));
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                return Err(Error::Other(format!(
                    "{what} の実行スレッドが中断されました"
                )));
            }
        }
    }
}

/// URL からステージングディレクトリへダウンロードし SHA-256 を計算。
/// 実体は bounded() 経由でワーカースレッド実行 — ソケット停滞で
/// read() が戻らない場合でも deadline で呼び出し側を失敗終了させる。
pub fn download(
    client: &GithubClient,
    url: &str,
    staging_dir: &Path,
    name: &str,
    deadline: Instant,
    progress: Option<std::sync::Arc<crate::lock::Progress>>,
) -> Result<FetchOutcome> {
    download_http(&client.http, url, staging_dir, name, deadline, progress)
}

/// 任意 HTTP クライアントでのダウンロード (GitHub 以外のレジストリ等)。
pub fn download_http(
    http: &reqwest::blocking::Client,
    url: &str,
    staging_dir: &Path,
    name: &str,
    deadline: Instant,
    progress: Option<std::sync::Arc<crate::lock::Progress>>,
) -> Result<FetchOutcome> {
    let http = http.clone();
    let url = url.to_string();
    let staging_dir = staging_dir.to_path_buf();
    let name = name.to_string();
    bounded("ダウンロード", deadline, move || {
        download_inner(&http, &url, &staging_dir, &name, progress.as_deref())
    })
}

fn download_inner(
    http: &reqwest::blocking::Client,
    url: &str,
    staging_dir: &Path,
    name: &str,
    progress: Option<&crate::lock::Progress>,
) -> Result<FetchOutcome> {
    fs::create_dir_all(staging_dir)?;
    let dest = staging_dir.join(name);
    let mut resp = http.get(url).send()?;
    if !resp.status().is_success() {
        return Err(Error::Config(format!(
            "ダウンロード失敗 {url}: HTTP {}",
            resp.status()
        )));
    }
    let total = resp.content_length();
    if let Some(p) = progress {
        p.bytes(0, total);
    }
    let mut f = fs::File::create(&dest)?;
    let mut hasher = sha2::Sha256::new();
    let mut done: u64 = 0;
    // ストリーミングでハッシュを取りながら書き込み
    let mut buf = [0u8; 256 * 1024];
    use std::io::Read;
    loop {
        let n = resp.read(&mut buf)?;
        if n == 0 {
            break;
        }
        f.write_all(&buf[..n])?;
        hasher.update(&buf[..n]);
        done += n as u64;
        if let Some(p) = progress {
            p.bytes(done, total);
        }
    }
    f.flush()?;
    use sha2::Digest;
    let sha = hex(&hasher.finalize());
    Ok(FetchOutcome {
        file: dest,
        sha256: sha,
        verification: VerificationResult {
            status: "pending".into(),
            method: None,
            detail: None,
        },
    })
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// 配布元提供のチェックサムを探索して検証する。
/// 順序: 1) asset.digest (API) 2) 同一リリース内のハッシュ一覧ファイル
/// 3) `<asset>.sha256` 等のサイドカー。
/// 見つからなければ status="unverified" (成功扱いしない)。
pub fn verify_checksum(
    client: &GithubClient,
    release: &Release,
    asset: &Asset,
    outcome: &mut FetchOutcome,
    _staging_dir: &Path,
    deadline: std::time::Instant,
) -> Result<()> {
    // 逐次プローブが全体上限を超えないよう各ダウンロード前に検査
    let check_dl = || -> Result<()> {
        if std::time::Instant::now() > deadline {
            return Err(Error::Timeout(
                "チェックサム探索が内部制限時間を超過しました".into(),
            ));
        }
        Ok(())
    };
    // 1) API digest
    if let Some(d) = &asset.digest {
        if let Some(expected) = d.strip_prefix("sha256:") {
            if expected.eq_ignore_ascii_case(&outcome.sha256) {
                outcome.verification = VerificationResult {
                    status: "verified".into(),
                    method: Some("github-asset-digest".into()),
                    detail: Some(format!("sha256={}", outcome.sha256)),
                };
                return Ok(());
            }
            return Err(Error::Verify(format!(
                "チェックサム不一致: 期待 {expected} / 実際 {}",
                outcome.sha256
            )));
        }
    }

    // 2) サイドカー `<name>.sha256` / `<name>.sha256sum`
    for cand_name in [
        format!("{}.sha256", asset.name),
        format!("{}.sha256sum", asset.name),
        format!("{}.sig", asset.name),
    ] {
        if let Some(side) = release.assets.iter().find(|a| a.name == cand_name) {
            if cand_name.ends_with(".sig") {
                // 署名ファイルのみは現状検証手段なし → 未検証記録
                outcome.verification = VerificationResult {
                    status: "unverified".into(),
                    method: Some("signature-present-not-verified".into()),
                    detail: Some(format!("署名ファイル {cand_name} あり。署名検証未実装")),
                };
                return Ok(());
            }
            check_dl()?;
            let text = download_text(client, &side.browser_download_url, deadline)?;
            if let Some(expected) = parse_checksum_text(&text, &asset.name) {
                if expected.eq_ignore_ascii_case(&outcome.sha256) {
                    outcome.verification = VerificationResult {
                        status: "verified".into(),
                        method: Some(format!("sidecar:{cand_name}")),
                        detail: Some(format!("sha256={}", outcome.sha256)),
                    };
                    return Ok(());
                }
                return Err(Error::Verify(format!(
                    "チェックサム不一致 ({cand_name}): 期待 {expected} / 実際 {}",
                    outcome.sha256
                )));
            }
        }
    }

    // 3) ハッシュ一覧ファイル (checksums.txt / SHA256SUMS / *.sha256 複数記載)
    for a in &release.assets {
        let n = a.name.to_lowercase();
        let is_list = (n.contains("checksum") || n.contains("sha256") || n.contains("sha512"))
            && a.name != asset.name
            && a.size < 1024 * 1024;
        if !is_list {
            continue;
        }
        check_dl()?;
        if let Ok(text) = download_text(client, &a.browser_download_url, deadline) {
            if let Some(expected) = parse_checksum_text(&text, &asset.name) {
                if expected.eq_ignore_ascii_case(&outcome.sha256) {
                    outcome.verification = VerificationResult {
                        status: "verified".into(),
                        method: Some(format!("checksum-file:{}", a.name)),
                        detail: Some(format!("sha256={}", outcome.sha256)),
                    };
                    return Ok(());
                }
                return Err(Error::Verify(format!(
                    "チェックサム不一致 ({}): 期待 {expected} / 実際 {}",
                    a.name, outcome.sha256
                )));
            }
        }
    }

    outcome.verification = VerificationResult {
        status: "unverified".into(),
        method: None,
        detail: Some("配布元がチェックサム/署名を提供していません".into()),
    };
    Ok(())
}

fn download_text(client: &GithubClient, url: &str, deadline: Instant) -> Result<String> {
    let http = client.http.clone();
    let url = url.to_string();
    bounded("チェックサム取得", deadline, move || {
        let mut resp = http.get(&url).send()?;
        if !resp.status().is_success() {
            return Err(Error::Config(format!("HTTP {}", resp.status())));
        }
        let mut s = String::new();
        use std::io::Read;
        resp.read_to_string(&mut s)?;
        Ok(s)
    })
}

/// "hash  filename" / "filename  hash" / 裸ハッシュ の行を解析。
/// target_name に対応する行のハッシュを返す。
fn parse_checksum_text(text: &str, target_name: &str) -> Option<String> {
    let is_hex = |s: &str, n: usize| s.len() == n && s.chars().all(|c| c.is_ascii_hexdigit());
    let mut bare: Option<String> = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let a = parts.next().unwrap_or("");
        let b = parts.next().unwrap_or("");
        // "hash  *filename" (GNU coreutils 形式は '*' 付き)
        let b_clean = b.trim_start_matches('*');
        if (is_hex(a, 64) || is_hex(a, 128))
            && (b_clean == target_name
                || b_clean.ends_with(&format!("/{target_name}"))
                || b_clean.ends_with(&format!("\\{target_name}")))
        {
            return Some(a.to_lowercase());
        }
        // "filename  hash"
        let a_clean = a.trim_start_matches('*');
        if (is_hex(b, 64) || is_hex(b, 128))
            && (a_clean == target_name
                || a_clean.ends_with(&format!("/{target_name}"))
                || a_clean.ends_with(&format!("\\{target_name}")))
        {
            return Some(b.to_lowercase());
        }
        // ファイル名なしの裸ハッシュ (単一行サイドカー用)
        if parts.next().is_none() && (is_hex(a, 64) || is_hex(a, 128)) {
            bare = Some(a.to_lowercase());
        }
    }
    bare
}

/// ローカルファイル取込 (手動成果物)。ハッシュ計算のみ (署名提供は利用者判断)。
pub fn import_local_file(src: &Path, staging_dir: &Path) -> Result<FetchOutcome> {
    if !src.exists() {
        return Err(Error::path_not_found(src));
    }
    fs::create_dir_all(staging_dir)?;
    let name = src
        .file_name()
        .ok_or_else(|| Error::Config("ファイル名を取得できません".into()))?
        .to_string_lossy()
        .to_string();
    let dest = staging_dir.join(&name);
    fs::copy(src, &dest)?;
    let sha = paths::sha256_file(&dest)?;
    Ok(FetchOutcome {
        file: dest,
        sha256: sha.clone(),
        verification: VerificationResult {
            status: "unverified".into(),
            method: Some("manual-import".into()),
            detail: Some(format!("ローカル取込 sha256={sha}")),
        },
    })
}

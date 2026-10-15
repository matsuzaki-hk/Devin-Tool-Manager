//! GitHub Releases 照会 (仕様 §5, §6-1)。
//! リリース選択 (stable/prerelease/tag固定)、アセット選択 (exact/glob/regex、
//! OS/CPUはレシピ保持でファイル名推測だけに任せない)、0件/複数件は保留。

use crate::credentials;
use crate::error::{Error, Result};
use crate::models::{AssetMatchType, GithubReleaseProvider, ReleaseChannel};
use globset::{Glob, GlobSetBuilder};
use serde::Deserialize;
use std::time::{Duration, Instant};

const API: &str = "https://api.github.com";
const UA: &str = "devin-tool-manager";

#[derive(Debug, Clone, Deserialize)]
pub struct Release {
    pub tag_name: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub prerelease: bool,
    #[serde(default)]
    pub draft: bool,
    pub html_url: String,
    #[serde(default)]
    pub published_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    pub assets: Vec<Asset>,
    /// リスト系APIでは body は含まれるが大きいので任意
    #[serde(default)]
    pub body: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Asset {
    pub name: String,
    pub browser_download_url: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub digest: Option<String>, // "sha256:..." (GitHub が付与する場合あり)
    #[serde(default)]
    pub content_type: Option<String>,
}

pub struct GithubClient {
    pub(crate) http: reqwest::blocking::Client,
}

impl GithubClient {
    pub fn new() -> Result<Self> {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::USER_AGENT,
            reqwest::header::HeaderValue::from_static(UA),
        );
        headers.insert(
            reqwest::header::ACCEPT,
            reqwest::header::HeaderValue::from_static("application/vnd.github+json"),
        );
        if let Some(tok) = credentials::github_token() {
            let v = reqwest::header::HeaderValue::from_str(&format!("Bearer {tok}"))
                .map_err(|_| Error::Config("トークン形式が不正".into()))?;
            headers.insert(reqwest::header::AUTHORIZATION, v);
        }
        let http = reqwest::blocking::Client::builder()
            .default_headers(headers)
            // blocking では timeout が各 read 呼び出しごとに適用される
            // (累計でなく逐次のため、低速継続DLは fetch::download 側の
            //  デッドライン検査で遮断する)
            .timeout(std::time::Duration::from_secs(60))
            .build()?;
        Ok(Self { http })
    }

    /// "owner/repo" を検証してパーツに分割
    fn split_repo(repo: &str) -> Result<(&str, &str)> {
        let mut it = repo.trim().trim_matches('/').split('/');
        match (it.next(), it.next(), it.next()) {
            (Some(o), Some(r), None) if !o.is_empty() && !r.is_empty() => Ok((o, r)),
            _ => Err(Error::Config(format!(
                "repository は 'owner/repo' 形式で指定: {repo}"
            ))),
        }
    }

    /// URL 指定対応: "https://github.com/owner/repo" → "owner/repo"
    pub fn normalize_repo(input: &str) -> String {
        let t = input.trim().trim_end_matches(".git").trim_matches('/');
        if let Some(idx) = t.find("github.com/") {
            let rest = &t[idx + "github.com/".len()..];
            let parts: Vec<&str> = rest.split('/').collect();
            if parts.len() >= 2 {
                return format!("{}/{}", parts[0], parts[1]);
            }
        }
        t.to_string()
    }

    /// リリース一覧 (最大100件/ページ、必要に応じて2ページ目)
    /// ボディ読み取りのソケット停滞は deadline で遮断する。
    pub fn releases(&self, repo: &str, per_page: u32) -> Result<Vec<Release>> {
        self.releases_bounded(repo, per_page, Instant::now() + Duration::from_secs(120))
    }

    pub fn releases_bounded(
        &self,
        repo: &str,
        per_page: u32,
        deadline: Instant,
    ) -> Result<Vec<Release>> {
        let (o, r) = Self::split_repo(repo)?;
        let url = format!("{API}/repos/{o}/{r}/releases?per_page={per_page}");
        let repo = repo.to_string();
        let http = self.http.clone();
        crate::fetch::bounded("GitHub releases 取得", deadline, move || {
            let resp = http.get(&url).send()?;
            if !resp.status().is_success() {
                return Err(Error::Config(format!(
                    "GitHub releases 取得失敗 {repo}: HTTP {}",
                    resp.status()
                )));
            }
            Ok(resp.json()?)
        })
    }

    /// channel に従い対象リリースを選択 (仕様 §5.1)
    pub fn select_release<'a>(
        releases: &'a [Release],
        provider: &GithubReleaseProvider,
    ) -> Option<&'a Release> {
        let mut cand: Vec<&Release> = releases.iter().filter(|r| !r.draft).collect();
        match provider.channel {
            ReleaseChannel::Stable => {
                cand.retain(|r| !r.prerelease);
            }
            ReleaseChannel::IncludePrerelease => {}
            ReleaseChannel::PinnedTag => {
                let tag = provider.asset.tag.as_deref().unwrap_or_default();
                return cand.into_iter().find(|r| r.tag_name == tag);
            }
        }
        // API は公開日降順で返る。先頭が最新。
        cand.into_iter().next()
    }

    /// アセット選択 (仕様 §5.1)。
    /// 0件・複数件は Err(NeedUser) で「更新を保留し条件修正を促す」。
    pub fn select_asset<'a>(
        release: &'a Release,
        provider: &GithubReleaseProvider,
    ) -> Result<&'a Asset> {
        let sel = &provider.asset;
        let matches: Vec<&Asset> = release
            .assets
            .iter()
            .filter(|a| asset_matches(&a.name, sel.match_type, &sel.pattern))
            .collect();
        match matches.len() {
            0 => Err(Error::NeedUser(format!(
                "アセット条件に一致なし: pattern='{}' (release {} のアセット: {})",
                sel.pattern,
                release.tag_name,
                release
                    .assets
                    .iter()
                    .map(|a| a.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))),
            1 => Ok(matches[0]),
            _ => Err(Error::NeedUser(format!(
                "アセット条件が複数一致 ({}件): {} — 条件を絞ってください",
                matches.len(),
                matches
                    .iter()
                    .map(|a| a.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))),
        }
    }
}

fn asset_matches(name: &str, ty: AssetMatchType, pattern: &str) -> bool {
    match ty {
        AssetMatchType::Exact => name == pattern,
        AssetMatchType::Glob => {
            let mut b = GlobSetBuilder::new();
            match Glob::new(pattern) {
                Ok(g) => {
                    b.add(g);
                    match b.build() {
                        Ok(set) => set.is_match(name),
                        Err(_) => false,
                    }
                }
                Err(_) => false,
            }
        }
        AssetMatchType::Regex => regex::Regex::new(pattern)
            .map(|re| re.is_match(name))
            .unwrap_or(false),
    }
}

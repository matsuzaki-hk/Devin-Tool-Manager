//! 版比較 (仕様 §5.1)。
//! 日付/SemVer/独自タグを区別し、文字列辞書順で新旧を判定しない。
//! 比較不能なら自動ダウングレードしない。

use crate::models::VersionScheme;
use semver::Version;
use std::cmp::Ordering;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareResult {
    Older,
    Equal,
    Newer,
    /// 比較不能 (scheme不一致・パース不可)
    Incomparable,
}

/// "v1.2.3", "1.2.3", "2024.01.15", "20240115" 等から semver 抽出を試みる
pub fn parse_semver(tag: &str) -> Option<Version> {
    let t = tag.trim().trim_start_matches(['v', 'V']);
    if let Ok(v) = Version::parse(t) {
        return Some(v);
    }
    // "1.2" → "1.2.0"
    if let Ok(v) = Version::parse(&format!("{t}.0")) {
        return Some(v);
    }
    // "1" → "1.0.0"
    Version::parse(&format!("{t}.0.0")).ok()
}

/// 日付系タグ: "2024-01-15", "2024.01.15", "20240115", "r2024.01" 等
fn parse_date(tag: &str) -> Option<chrono::NaiveDate> {
    let t = tag.trim().trim_start_matches(['v', 'V', 'r', 'R']);
    for (clean, fmt) in [
        (t.to_string(), "%Y-%m-%d"),
        (t.to_string(), "%Y.%m.%d"),
        (t.to_string(), "%Y%m%d"),
        (t.to_string(), "%Y/%m/%d"),
    ] {
        if let Ok(d) = chrono::NaiveDate::parse_from_str(&clean, fmt) {
            return Some(d);
        }
    }
    None
}

/// タグの版体系を推定 (recipe の versionScheme が auto 的に使う補助)
pub fn detect_scheme(tag: &str) -> VersionScheme {
    if parse_semver(tag).is_some() {
        VersionScheme::Semver
    } else if parse_date(tag).is_some() {
        VersionScheme::Date
    } else {
        VersionScheme::Custom
    }
}

/// "1.2.3" / "0.2.0.0" 等の純数値ドット列を整数タプル化 (非数値を含むなら None)。
/// Windows FileVersion の 4部形式を semver と比較可能にするために使う。
fn numeric_tuple(s: &str) -> Option<Vec<u64>> {
    let t = s.trim().trim_start_matches(['v', 'V']);
    let parts: Vec<&str> = t.split('.').collect();
    if parts.len() < 2
        || !parts
            .iter()
            .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
    {
        return None;
    }
    parts.iter().map(|p| p.parse().ok()).collect()
}

fn cmp_numeric_tuple(current: &str, candidate: &str) -> Option<CompareResult> {
    let (a, b) = (numeric_tuple(current)?, numeric_tuple(candidate)?);
    let n = a.len().max(b.len());
    for i in 0..n {
        let (x, y) = (
            a.get(i).copied().unwrap_or(0),
            b.get(i).copied().unwrap_or(0),
        );
        if x != y {
            return Some(if x < y {
                CompareResult::Newer
            } else {
                CompareResult::Older
            });
        }
    }
    Some(CompareResult::Equal)
}

/// `current` と `candidate` を scheme に従い比較。
/// Incomparable を返した場合、呼び出し側は自動更新せず候補提示のみ行う。
pub fn compare(scheme: VersionScheme, current: &str, candidate: &str) -> CompareResult {
    match scheme {
        VersionScheme::Semver => {
            // 純数値ドット列同士は部数の違いを0埋めして要素比較
            // (FileVersion "0.2.0.0" 対 リリースタグ "v0.2.0" → Equal)
            if let Some(r) = cmp_numeric_tuple(current, candidate) {
                return r;
            }
            match (parse_semver(current), parse_semver(candidate)) {
                (Some(a), Some(b)) => match a.cmp(&b) {
                    Ordering::Less => CompareResult::Newer,
                    Ordering::Equal => CompareResult::Equal,
                    Ordering::Greater => CompareResult::Older,
                },
                _ => CompareResult::Incomparable,
            }
        }
        VersionScheme::Date => match (parse_date(current), parse_date(candidate)) {
            (Some(a), Some(b)) => match a.cmp(&b) {
                std::cmp::Ordering::Less => CompareResult::Newer,
                std::cmp::Ordering::Equal => CompareResult::Equal,
                std::cmp::Ordering::Greater => CompareResult::Older,
            },
            _ => CompareResult::Incomparable,
        },
        VersionScheme::Custom => CompareResult::Incomparable,
    }
}

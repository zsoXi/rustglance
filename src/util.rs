//! Small helpers: locking, URL globs, hashes and time stamps.

use std::io;
use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

/// Locks a mutex whose data stays consistent even if a holder panicked.
///
/// Every critical section in this crate is a single insert, remove, push or
/// clone, so a panic cannot leave the protected data half-updated.
pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Matches `s` against a pattern where `*` is any run and `?` one character,
/// the same wildcards as CDP `Fetch` URL patterns.
pub fn glob(pattern: &str, s: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = s.chars().collect();
    let (mut i, mut j) = (0, 0);
    let mut star: Option<usize> = None;
    let mut mark = 0;
    while j < t.len() {
        if i < p.len() && (p[i] == '?' || p[i] == t[j]) {
            i += 1;
            j += 1;
        } else if i < p.len() && p[i] == '*' {
            star = Some(i);
            mark = j;
            i += 1;
        } else if let Some(si) = star {
            i = si + 1;
            mark += 1;
            j = mark;
        } else {
            return false;
        }
    }
    while i < p.len() && p[i] == '*' {
        i += 1;
    }
    i == p.len()
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Size and SHA-256 of a file, read in a streaming way.
pub fn sha256_file(path: &Path) -> io::Result<(u64, String)> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let size = io::copy(&mut file, &mut hasher)?;
    Ok((size, format!("{:x}", hasher.finalize())))
}

/// Keeps letters, digits, `.`, `-` and `_`; anything else becomes `_`.
pub fn safe_name(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let s = s.trim_start_matches('.').to_string();
    if s.is_empty() { "file".to_string() } else { s }
}

struct Utc {
    year: i64,
    month: i64,
    day: i64,
    secs_of_day: i64,
    millis: u32,
}

fn utc_now() -> Utc {
    // A clock before 1970 is a broken host; it prints as 1970-01-01.
    let since = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = i64::try_from(since.as_secs()).unwrap_or(i64::MAX);
    let days = secs.div_euclid(86_400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    Utc {
        year,
        month,
        day,
        secs_of_day: secs.rem_euclid(86_400),
        millis: since.subsec_millis(),
    }
}

/// UTC time like `2026-11-10T04:12:33.123Z`.
pub fn now() -> String {
    let u = utc_now();
    let s = u.secs_of_day;
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        u.year,
        u.month,
        u.day,
        s / 3600,
        s / 60 % 60,
        s % 60,
        u.millis
    )
}

/// UTC time for file names, like `20261110-041233-123`.
pub fn stamp() -> String {
    let u = utc_now();
    let s = u.secs_of_day;
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}-{:03}",
        u.year,
        u.month,
        u.day,
        s / 3600,
        s / 60 % 60,
        s % 60,
        u.millis
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_name_keeps_files_in_their_folder() {
        for (name, safe) in [
            ("report-1_2.json", "report-1_2.json"),
            ("a b/c.txt", "a_b_c.txt"),
            ("../x", "_x"),
            ("..\\x", "_x"),
            (".hidden", "hidden"),
            ("...", "file"),
            ("", "file"),
        ] {
            assert_eq!(safe_name(name), safe, "{name}");
        }
    }

    #[test]
    fn glob_matches_like_fetch_patterns() {
        for (p, s) in [
            ("*", "https://a.test/x"),
            ("*", ""),
            ("", ""),
            ("https://a.test/api/*", "https://a.test/api/items/7"),
            ("*/runs/*/resume", "https://a.test/api/runs/42/resume"),
            ("a?c", "abc"),
            ("*a*b", "xaxxb"),
            ("*ab", "aab"),
        ] {
            assert!(glob(p, s), "{p} should match {s}");
        }
        for (p, s) in [
            ("", "a"),
            ("https://a.test/api/*", "https://b.test/api/items"),
            ("*/runs/*/resume", "https://a.test/api/runs/42/resume/now"),
            ("a?c", "ac"),
            ("https://a.test/save", "https://a.test/save2"),
        ] {
            assert!(!glob(p, s), "{p} should not match {s}");
        }
    }
}

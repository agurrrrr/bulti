//! 프롬프트 히스토리 출처 수집 (↑/↓ 히스토리 탐색과 `/history` 명령용).
//!
//! 슬래시 커맨드(`/`) 자동완성은 `crate::slash` 가 담당하고, 이 모듈은
//! 세션·프롬프트 히스토리에서 프롬프트 문자열을 수집해 제공한다.
//!
//! - `CompletionSource`: 수집된 프롬프트 문자열 배열.
//! - `load_history_sources`: history DB + 세션 파일에서 프롬프트 수집.

use std::collections::HashSet;

/// 히스토리·세션에서 수집한 프롬프트 문자열 배열.
pub type CompletionSource = Vec<String>;

/// history DB + 세션 파일에서 프롬프트 후보를 수집한다.
///
/// - history DB `runs.prompt` (최근 200개)
/// - 세션 파일 `turns[].user` (최근 세션들)
///
/// DB·세션을 못 열면 빈 목록을 반환한다 (자동완성은 최선 노력).
pub fn load_history_sources() -> CompletionSource {
    let mut out: CompletionSource = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    // 1) history DB 프롬프트.
    if let Ok(conn) = crate::history::open() {
        if let Ok(runs) = crate::history::list_runs(&conn, Some(200), None, None) {
            for r in runs {
                let p = r.prompt.trim();
                if p.is_empty() || p.starts_with('/') {
                    continue;
                }
                if seen.insert(p.to_string()) {
                    out.push(p.to_string());
                }
            }
        }
    }

    // 2) 세션 파일 사용자 턴.
    if let Ok(metas) = crate::session::list() {
        for meta in metas.iter().take(20) {
            if let Ok(s) = crate::session::load(&meta.id) {
                for t in s.turns {
                    let u = t.user.trim();
                    if u.is_empty() || u.starts_with('/') {
                        continue;
                    }
                    if seen.insert(u.to_string()) {
                        out.push(u.to_string());
                    }
                }
            }
        }
    }

    out
}
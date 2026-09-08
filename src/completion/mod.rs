//! 일반 텍스트 자동완성 (고스트 텍스트 + fuzzy match) (DESIGN.md §4.13 대화형 프롬프트).
//!
//! 슬래시 커맨드(`/`) 자동완성은 `crate::slash` 가 담당하고, 이 모듈은
//! 세션·프롬프트 히스토리에서 수집한 프롬프트를 fuzzy 매칭해 일반 텍스트
//! 자동완성 후보를 제공한다. grok-build 의 `suggestion_controller` +
//! `CompletionItemParsed` 구조를 의존성 없이 단순화한 버전이다.
//!
//! - `CompletionItem`: display/description/insert_text/replace_range 구조.
//! - `progressive_ghost`: 사용자가 접두사 일치 입력 시 고스트가 자동 축소.
//! - `load_history_sources`: history DB + 세션 파일에서 프롬프트 수집.

use std::collections::HashSet;

/// 자동완성 후보 항목.
#[derive(Debug, Clone)]
pub struct CompletionItem {
    /// 후보 표시 텍스트 (드롭다운·고스트에 그대로 표시).
    pub display: String,
    /// 설명 (후보 출처: 히스토리/세션).
    pub description: String,
    /// 선택 시 입력창에 삽입할 전체 텍스트.
    pub insert_text: String,
    /// 입력창 내 교체 범위 (start, end). `None` 이면 전체 입력을 교체.
    pub replace_range: Option<(usize, usize)>,
}

/// 히스토리·세션에서 수집한 프롬프트 문자열 배열.
pub type CompletionSource = Vec<String>;

/// subsequence 퍼지 매칭. 빈 query 는 항상 true.
pub fn fuzzy_match(text: &str, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let mut qchars = query.chars();
    let mut cur = qchars.next().unwrap();
    for c in text.chars() {
        if c.to_lowercase().next() == Some(cur) {
            match qchars.next() {
                Some(n) => cur = n,
                None => return true,
            }
        }
    }
    false
}

/// `query`(현재 입력 전체)에 대한 자동완성 후보를 반환한다.
/// `sources` 는 프롬프트 후보 목록. 결과는 fuzzy 점수 내림차순으로 정렬되며
/// `limit` 개수만큼만 반환한다.
pub fn find_matches(query: &str, sources: &[String], limit: usize) -> Vec<CompletionItem> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return Vec::new();
    }
    let mut scored: Vec<(i64, CompletionItem)> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for src in sources {
        let src_trim = src.trim();
        if src_trim.is_empty() {
            continue;
        }
        // 이미 같은 후보를 넣었으면 건너뛴다 (중복 제거).
        if !seen.insert(src_trim.to_string()) {
            continue;
        }
        if !fuzzy_match(src_trim, &q) {
            continue;
        }
        // 점수: 접두사 일치가 최우선, 그 다음 subsequence.
        let score = if src_trim.to_lowercase().starts_with(&q) {
            // 접두사가 길수록(입력에 가까울수록) 높게.
            q.len() as i64 * 100 + src_trim.chars().count() as i64
        } else {
            src_trim.chars().count() as i64
        };
        scored.push((
            score,
            CompletionItem {
                display: src_trim.to_string(),
                description: "히스토리".to_string(),
                insert_text: src_trim.to_string(),
                replace_range: None,
            },
        ));
    }
    scored.sort_by_key(|x| std::cmp::Reverse(x.0));
    scored.into_iter().take(limit).map(|(_, it)| it).collect()
}

/// 고스트 텍스트 계산 — progressive matching.
///
/// `candidate` 가 현재 `input` 으로 접두사 일치하면 나머지 부분을 반환한다.
/// 사용자가 더 타이핑할수록(접두사가 길어질수록) 고스트가 자동으로 축소된다.
/// 일치하지 않으면 `None` 을 반환해 고스트를 해제한다.
pub fn progressive_ghost(input: &str, candidate: &str) -> Option<String> {
    if input.is_empty() || candidate.is_empty() {
        return None;
    }
    if candidate.starts_with(input) && candidate != input {
        return Some(candidate[input.len()..].to_string());
    }
    None
}

/// `sources` 중 `input` 과 progressive matching 되는 최선의 후보 고스트를 반환한다.
/// 접두사 일치 후보가 없으면 fuzzy 최선 후보의 고스트를 반환한다.
pub fn best_ghost(input: &str, sources: &[String]) -> Option<String> {
    if input.trim().is_empty() {
        return None;
    }
    // 1) 접두사(progressive) 일치 우선.
    for src in sources {
        let src_trim = src.trim();
        if src_trim.is_empty() || src_trim == input {
            continue;
        }
        if let Some(ghost) = progressive_ghost(input, src_trim) {
            return Some(ghost);
        }
    }
    // 2) fuzzy 최선 후보의 접두사 고스트.
    let matches = find_matches(input, sources, 1);
    matches.first().and_then(|m| progressive_ghost(input, &m.insert_text))
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_match_subsequence() {
        assert!(fuzzy_match("안녕하세요", "녕하"));
        assert!(fuzzy_match("bulti project", "bj"));
        assert!(!fuzzy_match("안녕하세요", "세안"));
        assert!(fuzzy_match("anything", ""));
    }

    #[test]
    fn find_matches_prefix_priority() {
        let sources = vec![
            "안녕하세요".to_string(),
            "안녕".to_string(),
            "오늘 날씨".to_string(),
        ];
        let m = find_matches("안녕", &sources, 10);
        // 접두사 일치("안녕" 으로 시작하는)가 우선.
        assert!(m[0].insert_text == "안녕" || m[0].insert_text == "안녕하세요");
        assert!(m.iter().any(|x| x.insert_text == "안녕하세요"));
        assert!(m.iter().any(|x| x.insert_text == "안녕"));
        // 오늘 날씨는 subsequence 상 '안녕' 과 매칭 안 됨.
        assert!(!m.iter().any(|x| x.insert_text == "오늘 날씨"));
    }

    #[test]
    fn progressive_ghost_shrinks_on_longer_input() {
        let cand = "안녕하세요";
        let g1 = progressive_ghost("안", cand).unwrap();
        let g2 = progressive_ghost("안녕", cand).unwrap();
        let g3 = progressive_ghost("안녕하", cand).unwrap();
        // 타이핑이 길어질수록 고스트가 축소된다.
        assert!(g1.chars().count() > g2.chars().count());
        assert!(g2.chars().count() > g3.chars().count());
        assert_eq!(g3, "세요");
        // 완전 일치하면 고스트 해제.
        assert!(progressive_ghost("안녕하세요", cand).is_none());
        // 불일치하면 고스트 해제.
        assert!(progressive_ghost("다른말", cand).is_none());
    }

    #[test]
    fn best_ghost_returns_prefix_match() {
        let sources = vec!["안녕하세요".to_string(), "안녕히 가세요".to_string()];
        let g = best_ghost("안녕", &sources).unwrap();
        assert!(g == "하세요" || g == "히 가세요");
    }

    #[test]
    fn best_ghost_none_on_empty() {
        assert!(best_ghost("", &["안녕".to_string()]).is_none());
        assert!(best_ghost("  ", &["안녕".to_string()]).is_none());
    }

    #[test]
    fn find_matches_dedups() {
        let sources = vec!["안녕".to_string(), "안녕".to_string()];
        let m = find_matches("안", &sources, 10);
        assert_eq!(m.len(), 1);
    }

    #[test]
    fn find_matches_empty_query_returns_empty() {
        let sources = vec!["안녕".to_string()];
        assert!(find_matches("", &sources, 10).is_empty());
    }
}
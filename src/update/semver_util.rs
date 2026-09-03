//! semver 비교 유틸리티 (DESIGN.md §4.11).
//!
//! `v` 접두어를 허용하고 `0.1.0`/`1.2.3` 형태의 태그를 파싱해 현재 버전과 비교한다.

/// 현재 버전 (CARGO_PKG_VERSION).
pub fn current_version() -> semver::Version {
    semver::Version::parse(env!("CARGO_PKG_VERSION")).expect("CARGO_PKG_VERSION 은 유효한 semver")
}

/// 태그(`v0.2.0` 등)를 파싱한다. 실패 시 None.
pub fn parse_tag(tag: &str) -> Option<semver::Version> {
    let trimmed = tag.trim().trim_start_matches('v');
    semver::Version::parse(trimmed).ok()
}

/// 태그가 현재 설치 버전보다 높은지 판단한다.
pub fn is_newer(tag: &str) -> bool {
    match parse_tag(tag) {
        Some(latest) => latest > current_version(),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_v_prefix() {
        assert!(parse_tag("v0.2.0").is_some());
        assert!(parse_tag("0.2.0").is_some());
        assert!(parse_tag("v1.2.3").is_some());
    }

    #[test]
    fn rejects_invalid() {
        assert!(parse_tag("abc").is_none());
        assert!(parse_tag("").is_none());
        assert!(parse_tag("v").is_none());
    }

    #[test]
    fn newer_detection() {
        // 현재 버전 0.1.0 기준.
        assert!(is_newer("v0.2.0"));
        assert!(is_newer("v1.0.0"));
        assert!(!is_newer("v0.1.0"));
        assert!(!is_newer("v0.0.9"));
        assert!(!is_newer("not-a-version"));
    }
}
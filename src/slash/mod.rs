//! 슬래시 커맨드 레지스트리와 자동완성 (DESIGN.md §4.13 대화형 프롬프트).
//!
//! 채팅창에서 `/` 로 시작하는 내부 커맨드를 등록·조회·자동완성한다.
//! grok-build 의 `slash/registry.rs` + `slash/matcher.rs` 패턴을 의존성 없이
//! 단순화한 버전 (nucleo 대신 prefix/subsequence 매칭 사용).

/// 슬래시 커맨드 메타데이터.
#[derive(Debug, Clone)]
pub struct SlashCommand {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub description: &'static str,
    pub usage: &'static str,
    /// 인자를 받는지 여부.
    pub takes_args: bool,
    /// 인자가 필수인지 여부.
    pub args_required: bool,
}

/// 내장 슬래시 커맨드 목록 (자동완성·파싱 공용).
pub const COMMANDS: &[SlashCommand] = &[
    SlashCommand {
        name: "exit",
        aliases: &["quit", "q"],
        description: "대화 종료",
        usage: "/exit",
        takes_args: false,
        args_required: false,
    },
    SlashCommand {
        name: "new",
        aliases: &[],
        description: "새 세션 시작",
        usage: "/new",
        takes_args: false,
        args_required: false,
    },
    SlashCommand {
        name: "help",
        aliases: &["?"],
        description: "명령 도움말",
        usage: "/help",
        takes_args: false,
        args_required: false,
    },
    SlashCommand {
        name: "resume",
        aliases: &[],
        description: "세션 재개",
        usage: "/resume <id>",
        takes_args: true,
        args_required: true,
    },
    SlashCommand {
        name: "model",
        aliases: &["m"],
        description: "모델 전환",
        usage: "/model <name> [effort]",
        takes_args: true,
        args_required: true,
    },
    SlashCommand {
        name: "effort",
        aliases: &["e"],
        description: "reasoning effort 설정",
        usage: "/effort <low|medium|high>",
        takes_args: true,
        args_required: true,
    },
    SlashCommand {
        name: "session-info",
        aliases: &["info"],
        description: "현재 세션 정보 표시 (id·모델·컨텍스트 사용량)",
        usage: "/session-info",
        takes_args: false,
        args_required: false,
    },
    SlashCommand {
        name: "sessions",
        aliases: &["ls"],
        description: "세션 목록 조회",
        usage: "/sessions",
        takes_args: false,
        args_required: false,
    },
    SlashCommand {
        name: "compact",
        aliases: &[],
        description: "대화 히스토리 컴팩트 (요약으로 컨텍스트 축소)",
        usage: "/compact",
        takes_args: false,
        args_required: false,
    },
    SlashCommand {
        name: "fork",
        aliases: &[],
        description: "현재 세션을 분기 (새 세션 id 로 복제)",
        usage: "/fork",
        takes_args: false,
        args_required: false,
    },
    SlashCommand {
        name: "export",
        aliases: &[],
        description: "대화 기록 마크다운 파일로 내보내기",
        usage: "/export [파일경로]",
        takes_args: true,
        args_required: false,
    },
];

/// 자동완성 제안 항목.
#[derive(Debug, Clone)]
pub struct Suggestion {
    pub display: String,
    pub insert: String,
    pub description: &'static str,
}

/// `query`(`/` 는 제외된 상태)에 대한 자동완성 제안을 반환한다.
/// 빈 query 는 전체 목록을 반환한다.
pub fn suggest(query: &str) -> Vec<Suggestion> {
    let q = query.trim().to_lowercase();
    let mut out: Vec<Suggestion> = Vec::new();
    for cmd in COMMANDS {
        // query 가 특정 alias 와 매칭되면 alias 를 우선 제안한다.
        // 빈 query(전체 목록)에서는 canonical name 을 제안한다.
        let mut cand: Option<&str> = None;
        if q.is_empty() {
            cand = Some(cmd.name);
        } else {
            for alias in cmd.aliases {
                if fuzzy_match(alias, &q) {
                    cand = Some(alias);
                    break;
                }
            }
            if cand.is_none() && fuzzy_match(cmd.name, &q) {
                cand = Some(cmd.name);
            }
        }
        if let Some(c) = cand {
            out.push(Suggestion {
                display: format!("/{c}"),
                insert: format!("/{c}"),
                description: cmd.description,
            });
        }
    }
    out
}

/// 간단한 subsequence 퍼지 매칭. 빈 query 는 항상 true.
fn fuzzy_match(text: &str, query: &str) -> bool {
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

/// 파싱된 커맨드.
#[derive(Debug, Clone)]
pub struct ParsedCommand {
    pub name: String,
    pub args: String,
}

/// `line` 이 슬래시 커맨드면 `(canonical_name, args)` 를 반환한다.
/// 슬래시 커맨드가 아니면 `None`. 미지원 커맨드도 `Some` 으로 파싱된다
/// (호출부에서 `is_supported` 로 판별).
pub fn parse(line: &str) -> Option<ParsedCommand> {
    let trimmed = line.trim();
    if !trimmed.starts_with('/') {
        return None;
    }
    let body = &trimmed[1..];
    let (cmd, args) = match body.find(char::is_whitespace) {
        Some(idx) => (&body[..idx], body[idx..].trim()),
        None => (body, ""),
    };
    Some(ParsedCommand {
        name: resolve_alias(cmd).to_string(),
        args: args.to_string(),
    })
}

/// alias 를 canonical name 으로 변환. 미지원이면 빈 문자열 반환.
pub fn resolve_alias(cmd: &str) -> &'static str {
    for c in COMMANDS {
        if c.name == cmd {
            return c.name;
        }
        if c.aliases.contains(&cmd) {
            return c.name;
        }
    }
    ""
}

/// `cmd` 가 지원되는지 여부.
pub fn is_supported(cmd: &str) -> bool {
    !resolve_alias(cmd).is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggests_all_commands_on_empty_query() {
        let s = suggest("");
        assert!(s.iter().any(|x| x.insert == "/exit"));
        assert!(s.iter().any(|x| x.insert == "/model"));
        assert!(s.iter().any(|x| x.insert == "/effort"));
    }

    #[test]
    fn suggests_by_prefix() {
        let s = suggest("mo");
        assert!(s.iter().any(|x| x.insert == "/model"));
        let s = suggest("res");
        assert!(s.iter().any(|x| x.insert == "/resume"));
    }

    #[test]
    fn suggests_by_alias() {
        let s = suggest("m");
        assert!(s.iter().any(|x| x.insert == "/m"));
    }

    #[test]
    fn parses_commands() {
        let p = parse("/model gpt-4").unwrap();
        assert_eq!(p.name, "model");
        assert_eq!(p.args, "gpt-4");
        let p = parse("/m").unwrap();
        assert_eq!(p.name, "model");
        assert!(parse("hello").is_none());
        assert!(parse("/unknown").is_some());
        assert!(!is_supported("unknown"));
        assert!(is_supported("m"));
    }

    #[test]
    fn fuzzy_matching_works() {
        assert!(fuzzy_match("model", "mo"));
        assert!(fuzzy_match("model", "mdl"));
        assert!(fuzzy_match("resume", "rs"));
        assert!(!fuzzy_match("model", "xyz"));
    }
}
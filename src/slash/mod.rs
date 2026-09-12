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
///
/// `description`·`usage` 는 i18n 키(영어 원문)다. 표시 시 `crate::i18n::tr` 로
/// 현재 언어로 번역한다.
pub const COMMANDS: &[SlashCommand] = &[
    SlashCommand {
        name: "exit",
        aliases: &["quit", "q"],
        description: "Exit conversation",
        usage: "/exit",
        takes_args: false,
        args_required: false,
    },
    SlashCommand {
        name: "new",
        aliases: &[],
        description: "Start a new session",
        usage: "/new",
        takes_args: false,
        args_required: false,
    },
    SlashCommand {
        name: "help",
        aliases: &["?"],
        description: "Command help",
        usage: "/help",
        takes_args: false,
        args_required: false,
    },
    SlashCommand {
        name: "resume",
        aliases: &[],
        description: "Resume a session",
        usage: "/resume <id>",
        takes_args: true,
        args_required: true,
    },
    SlashCommand {
        name: "model",
        aliases: &["m"],
        description: "Switch model",
        usage: "/model <name> [effort]",
        takes_args: true,
        args_required: true,
    },
    SlashCommand {
        name: "effort",
        aliases: &["e"],
        description: "Set reasoning effort",
        usage: "/effort <low|medium|high>",
        takes_args: true,
        args_required: true,
    },
    SlashCommand {
        name: "endpoint",
        aliases: &["ep"],
        description: "Show or configure endpoints (list / add / set / use / remove)",
        usage: "/endpoint [name | add|set|use|remove ...]",
        takes_args: true,
        args_required: false,
    },
    SlashCommand {
        name: "mcp",
        aliases: &[],
        description: "Show or register MCP servers (list / add / set / remove)",
        usage: "/mcp [name | add|set|remove ...]",
        takes_args: true,
        args_required: false,
    },
    SlashCommand {
        name: "language",
        aliases: &["lang", "l"],
        description: "Change language (en / ko / ja)",
        usage: "/language <en|ko|ja>",
        takes_args: true,
        args_required: false,
    },
    SlashCommand {
        name: "session-info",
        aliases: &["info"],
        description: "Show current session info (id / model / context usage)",
        usage: "/session-info",
        takes_args: false,
        args_required: false,
    },
    SlashCommand {
        name: "sessions",
        aliases: &["ls"],
        description: "List sessions",
        usage: "/sessions",
        takes_args: false,
        args_required: false,
    },
    SlashCommand {
        name: "compact",
        aliases: &[],
        description: "Compact conversation history (summarize to shrink context)",
        usage: "/compact",
        takes_args: false,
        args_required: false,
    },
    SlashCommand {
        name: "fork",
        aliases: &[],
        description: "Fork the current session (copy to a new session id)",
        usage: "/fork",
        takes_args: false,
        args_required: false,
    },
    SlashCommand {
        name: "export",
        aliases: &[],
        description: "Export conversation history to a markdown file",
        usage: "/export [path]",
        takes_args: true,
        args_required: false,
    },
    SlashCommand {
        name: "usage",
        aliases: &["u"],
        description: "Show session token and cost usage",
        usage: "/usage",
        takes_args: false,
        args_required: false,
    },
    SlashCommand {
        name: "history",
        aliases: &["h"],
        description: "Browse prompt history (↑/↓ keys or this command)",
        usage: "/history [query]",
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

/// 인자 자동완성에 쓸 동적 후보 목록 (설정·세션에서 수집).
///
/// 슬래시 커맨드는 정적 레지스트리(`COMMANDS`)지만, `/model`·`/endpoint`·`/mcp`
/// 처럼 인자를 받는 커맨드는 실행 시점의 설정에 따라 후보가 달라진다. 이 구조체가
/// 그 후보를 담아 `suggest_with` 에 전달된다.
#[derive(Debug, Clone, Default)]
pub struct CompletionContext {
    /// 모델 후보 (`/model`).
    pub models: Vec<String>,
    /// 엔드포인트 이름 후보 (`/endpoint`).
    pub endpoints: Vec<String>,
    /// MCP 서버 이름 후보 (`/mcp`).
    pub mcp: Vec<String>,
    /// 언어 코드 후보 (`/language`).
    pub languages: Vec<String>,
}

/// `cmd`(canonical name)에 대한 인자 후보와 설명 라벨을 반환한다.
/// 인자 자동완성을 지원하지 않는 커맨드면 `None`.
fn arg_candidates<'a>(
    cmd: &str,
    ctx: &'a CompletionContext,
) -> Option<(&'a [String], &'static str)> {
    match cmd {
        "model" => Some((&ctx.models, "model")),
        "endpoint" => Some((&ctx.endpoints, "endpoint")),
        "mcp" => Some((&ctx.mcp, "MCP server")),
        "language" => Some((&ctx.languages, "language")),
        _ => None,
    }
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
                description: crate::i18n::tr(cmd.description),
            });
        }
    }
    out
}

/// 입력 전체(`/` 포함 가능)에 대한 자동완성 제안.
///
/// - 첫 토큰만 입력한 상태(`/model`, `/endpoint`)면 기존 `suggest` 로 커맨드를
///   제안한다.
/// - 커맨드 뒤에 공백과 인자를 입력한 상태(`/model qw`, `/mcp git`)면
///   `ctx` 의 동적 후보로 인자를 제안하고, 선택 시 `insert` 는
///   `/cmd <후보>` 전체를 되돌려 입력을 교체한다.
pub fn suggest_with(query: &str, ctx: &CompletionContext) -> Vec<Suggestion> {
    let body = query.strip_prefix('/').unwrap_or(query);
    let Some((cmd_raw, rest)) = body.split_once(char::is_whitespace) else {
        return suggest(body);
    };
    let cmd = resolve_alias(cmd_raw);
    let Some((cands, desc)) = arg_candidates(cmd, ctx) else {
        // 인자 목록이 없는 커맨드 뒤에서는 드롭다운을 비운다.
        return Vec::new();
    };
    // 인자 첫 토큰만으로 필터링한다 (모델·이름에는 공백이 없다고 가정).
    let arg = rest.split_whitespace().next().unwrap_or("");
    let q = arg.to_lowercase();
    let mut out: Vec<Suggestion> = Vec::new();
    for c in cands {
        let lc = c.to_lowercase();
        if q.is_empty() || lc.starts_with(&q) || lc.contains(&q) {
            out.push(Suggestion {
                display: c.clone(),
                insert: format!("/{cmd} {c}"),
                description: crate::i18n::tr(desc),
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

/// `line` 의 첫 토큰(커맨드명, `/` 접두 제외)을 반환한다.
/// 미지원 커맨드는 `parse` 가 canonical name 을 빈 문자열로 되돌리기 때문에
/// 오타 제안용 원본 이름이 필요하다.
pub fn raw_name(line: &str) -> &str {
    let t = line.trim();
    let body = t.strip_prefix('/').unwrap_or(t);
    body.split_whitespace().next().unwrap_or("")
}

/// 오타가 의심되는 입력에 대해 유사한 커맨드(canonical name)를 제안한다.
/// Levenshtein 거리 기준으로 짧은 이름은 1, 긴 이름은 최대 2까지 허용하며,
/// name·alias 를 모두 후보로 본다. 최대 3개, 거리가 가까운 순.
pub fn suggest_similar(input: &str) -> Vec<String> {
    let q = input.trim().to_lowercase();
    if q.is_empty() {
        return Vec::new();
    }
    // 입력 길이에 비례해 허용 거리를 늘린다. 짧은 이름(3자)은 오타 1자만,
    // 4자 이상은 위치 교환 등 오타 2자까지 허용한다.
    let threshold = (q.len() + 3) / 4;
    let mut scored: Vec<(usize, &str)> = Vec::new();
    for cmd in COMMANDS {
        let mut best = usize::MAX;
        for n in std::iter::once(cmd.name).chain(cmd.aliases.iter().copied()) {
            let d = levenshtein(&n.to_lowercase(), &q);
            if d < best {
                best = d;
            }
        }
        if best <= threshold {
            scored.push((best, cmd.name));
        }
    }
    scored.sort();
    let mut out: Vec<String> = Vec::new();
    for (_, name) in scored {
        if !out.iter().any(|s: &String| s == name) {
            out.push(name.to_string());
        }
        if out.len() == 3 {
            break;
        }
    }
    out
}

/// 정규 Levenshtein 편집 거리 (의존성 없는 2행 DP).
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        for j in 1..=b.len() {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
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
    fn registry_has_endpoint_and_mcp() {
        assert!(suggest("").iter().any(|x| x.insert == "/endpoint"));
        assert!(suggest("").iter().any(|x| x.insert == "/mcp"));
        assert_eq!(resolve_alias("ep"), "endpoint");
        assert!(is_supported("mcp"));
    }

    #[test]
    fn suggest_with_completes_model_args() {
        let ctx = CompletionContext {
            models: vec!["qwen3-32b".to_string(), "llama3".to_string()],
            ..Default::default()
        };
        // 공백까지 입력하면 전체 모델 후보.
        let s = suggest_with("/model ", &ctx);
        assert_eq!(s.len(), 2);
        assert!(s.iter().any(|x| x.insert == "/model qwen3-32b"));
        // 접두 필터.
        let s = suggest_with("/model lla", &ctx);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].insert, "/model llama3");
        // 커맨드만 입력하면 커맨드 자체를 제안.
        let s = suggest_with("/model", &ctx);
        assert!(s.iter().any(|x| x.insert == "/model"));
    }

    #[test]
    fn suggest_with_completes_endpoint_and_mcp_args() {
        let ctx = CompletionContext {
            endpoints: vec!["main".to_string(), "backup".to_string()],
            mcp: vec!["files".to_string()],
            ..Default::default()
        };
        let s = suggest_with("/endpoint ", &ctx);
        assert_eq!(s.len(), 2);
        assert!(s.iter().any(|x| x.insert == "/endpoint main"));
        let s = suggest_with("/ep ma", &ctx);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].insert, "/endpoint main");
        let s = suggest_with("/mcp ", &ctx);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].insert, "/mcp files");
    }

    #[test]
    fn suggest_with_no_args_command_returns_empty() {
        let ctx = CompletionContext::default();
        // 인자 후보가 없는 커맨드 뒤에서는 제안이 없다.
        assert!(suggest_with("/help foo", &ctx).is_empty());
        assert!(suggest_with("/exit ", &ctx).is_empty());
    }

    #[test]
    fn registry_has_language_and_completes() {
        assert!(suggest("").iter().any(|x| x.insert == "/language"));
        assert_eq!(resolve_alias("lang"), "language");
        assert!(is_supported("language"));
        let ctx = CompletionContext {
            languages: vec!["en".to_string(), "ko".to_string(), "ja".to_string()],
            ..Default::default()
        };
        let s = suggest_with("/language ", &ctx);
        assert_eq!(s.len(), 3);
        assert!(s.iter().any(|x| x.insert == "/language ko"));
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
    fn parses_history() {
        // /history 는 alias h, 인자 선택.
        let p = parse("/history").unwrap();
        assert_eq!(p.name, "history");
        assert_eq!(p.args, "");
        let p = parse("/history 검색어").unwrap();
        assert_eq!(p.name, "history");
        assert_eq!(p.args, "검색어");
        let p = parse("/h").unwrap();
        assert_eq!(p.name, "history");
        assert!(is_supported("history"));
        assert_eq!(resolve_alias("h"), "history");
    }

    #[test]
    fn fuzzy_matching_works() {
        assert!(fuzzy_match("model", "mo"));
        assert!(fuzzy_match("model", "mdl"));
        assert!(fuzzy_match("resume", "rs"));
        assert!(!fuzzy_match("model", "xyz"));
    }

    #[test]
    fn raw_name_extracts_first_token() {
        assert_eq!(raw_name("/model gpt-4"), "model");
        assert_eq!(raw_name("/xyz"), "xyz");
        assert_eq!(raw_name("  /help  "), "help");
        assert_eq!(raw_name("/"), "");
        assert_eq!(raw_name("/unknown args here"), "unknown");
    }

    #[test]
    fn suggest_similar_finds_typo_commands() {
        // 한 글자 오타.
        let s = suggest_similar("resum");
        assert!(s.iter().any(|x| x == "resume"), "got {s:?}");
        let s = suggest_similar("hel");
        assert!(s.iter().any(|x| x == "help"), "got {s:?}");
        // alias 오타도 canonical name 으로 제안.
        let s = suggest_similar("modle");
        assert!(s.iter().any(|x| x == "model"), "got {s:?}");
        // 거리가 먼 입력에는 제안이 없다.
        assert!(suggest_similar("zzzzzzzz").is_empty());
        assert!(suggest_similar("").is_empty());
        // 제안은 최대 3개.
        assert!(suggest_similar("sessions").len() <= 3);
    }

    #[test]
    fn levenshtein_distance_is_correct() {
        assert_eq!(levenshtein("kitten", "sitting"), 3);
        assert_eq!(levenshtein("abc", "abc"), 0);
        assert_eq!(levenshtein("", "abc"), 3);
        assert_eq!(levenshtein("abc", ""), 3);
        // 문자 위치 교환(transposition)은 표준 Levenshtein에서 2 (삭제+삽입).
        assert_eq!(levenshtein("model", "modle"), 2);
    }
}

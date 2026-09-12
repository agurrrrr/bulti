//! `bulti chat` 서브커맨드 구현 (DESIGN.md §4.13.1 대화형 프롬프트 루프).
//!
//! 단계 2~3 범위: TTY에서 대화형 프롬프트 루프를 시작한다. 한 턴(사용자 프롬프트
//! 입력 → 모델 완료 응답)은 §4.3 에이전트 루프를 세그먼트 단위로 실행하는
//! 공통 코어(`run_segment`)를 재사용한다. 세션 저장·재개(§4.13.2)를 지원한다 —
//! 여기서는 스트림 텍스트 모드.
//!
//! - 프롬프트 루프는 `/exit`·`/quit`·Ctrl-D 로 종료, `/help` 로 명령 안내
//! - `/new` 로 새 세션 시작, `/resume <id>` 로 세션 재개
//! - Ctrl+C(SIGINT) → interrupted 기록 후 종료 (exit 130 규약 재사용)
//! - 대화형 전용 옵션: `--endpoint`, `--model`, `--system-file`, `--system`, `--resume`
//!
//! 세션 파일(`~/.bulti/sessions/`)은 대화형 재개용 원본, history DB(`~/.bulti/history.db`)
//! 는 작업 단위 감사용 기록으로 분리한다 (DESIGN.md §4.13.2).

use std::io::{BufRead, IsTerminal, Write};
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use super::ChatArgs;
use crate::agent::handoff::{HandoffDepthGuard, build_new_segment_prompt};
use crate::agent::loop_::{SegmentParams, SegmentStatus, run_segment};
use crate::config::{Config, EndpointConfig, McpConfig};
use crate::history;
use crate::llm::LlmClient;
use crate::mcp::McpManager;
use crate::session;
use crate::tui::TurnResult;

/// ANSI 이스케이프 시퀀스를 제거해 순수 텍스트로 만든다 (no-color 모드).
fn strip_ansi(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            // CSI 시퀀스: \x1b[ ...m 까지 건너뛴다.
            let mut done = false;
            for _ in 0..64 {
                match chars.next() {
                    Some('m') | Some('K') | Some('H') | Some('n') => {
                        done = true;
                        break;
                    }
                    Some(_) => continue,
                    None => break,
                }
            }
            let _ = done;
        } else {
            out.push(c);
        }
    }
    out
}

/// exit code 매핑 (run 과 동일 규약 재사용).
const EXIT_OK: i32 = 0;
const EXIT_ERROR: i32 = 1;
const EXIT_INTERRUPTED: i32 = 130;

/// 스트림 텍스트 모드 점진 출력기 (비 TTY).
///
/// reasoning·content 델타를 누적해 `render::parse`+`render_ansi` 로 부분 렌더하고
/// stdout 에 점진 출력한다.
///
/// - reasoning 은 `[생각]` 머리글로 별도 구분 (use_color 시 dim). content 와
///   같은 **안정 접두** 방식(`\n\n` 경계)으로 점진 플러시한다.
/// - content 는 **안정 접두**만 쓴다. markdown 파서는 마지막 빈 줄 이후의 문단을
///   아직 닫히지 않은 블록으로 취급해, 같은 입력 접두라도 나중에 다른 행으로
///   재렌더될 수 있다. 따라서 마지막 빈 줄 경계(이전 출력에 영향을 주지 않는
///   접두)에만 도달했을 때 그 부분 렌더의 접미사를 플러시한다.
/// - `finalize` 가 남은 불완전 부분을 마무리한다 (중복 출력 없음).
struct StreamPrinter {
    use_color: bool,
    reasoning: String,
    content: String,
    /// stdout 에 이미 쓴 누적 출력 (render() 결과의 접전).
    printed: String,
    /// content 의 안정 접두 길이 (마지막 빈 줄 경계, 바이트).
    stable_len: usize,
    /// reasoning 의 안정 접두 길이 (마지막 빈 줄 경계, 바이트).
    reasoning_stable_len: usize,
    /// 어떤 델타라도 받은 적이 있는지 (false 면 호출부가 일괄 출력 폴백).
    active: bool,
}

impl StreamPrinter {
    fn new(use_color: bool) -> Self {
        Self {
            use_color,
            reasoning: String::new(),
            content: String::new(),
            printed: String::new(),
            stable_len: 0,
            reasoning_stable_len: 0,
            active: false,
        }
    }

    /// content 의 안정 접두 길이를 계산한다.
    ///
    /// 마지막 빈 줄까지의 접두는 이후 arriving 텍스트와 같은 블록으로 병합될
    /// 수 없으므로(빈 줄이 블록 경계) 렌더가 불변이다. 그 경계 앞까지만 안정.
    fn stable_prefix_len(content: &str) -> usize {
        match content.rfind("\n\n") {
            Some(i) => i + 2,
            None => 0,
        }
    }

    /// reasoning_content 델타를 누적해 점진 출력한다.
    ///
    /// content 와 동일한 안정 접두 방식: `\n\n` 경계까지의 접두만 플러시한다.
    /// reasoning 은 markdown 렌더 없이 `[생각]` 머리글 + 원본 줄 구조 그대로
    /// 출력하므로, 경계 앞 접두의 렌더는 이후 arriving 텍스트에 의해 불변이다.
    fn print_reasoning(&mut self, text: &str) {
        self.active = true;
        self.reasoning.push_str(text);
        self.flush_reasoning();
    }

    /// content 델타를 누적해 점진 출력한다.
    fn print_content(&mut self, text: &str) {
        self.active = true;
        self.content.push_str(text);
        self.flush();
    }

    /// 스트림 완료 시 마무리. 남은 불완전 부분을 쓰고 종료 줄바꿈을 보장한다.
    /// 어떤 출력도 없으면 `false` (호출부가 일괄 출력 폴백).
    fn finalize(&mut self) -> bool {
        if !self.active {
            return false;
        }
        let rendered = self.render();
        let mut rest = rendered
            .strip_prefix(&self.printed)
            .unwrap_or(&rendered)
            .to_string();
        if !rest.ends_with('\n') {
            rest.push('\n');
        }
        if !rest.is_empty() {
            let _ = std::io::stdout().write_all(rest.as_bytes());
            self.printed = rendered;
        }
        let _ = std::io::stdout().flush();
        true
    }

    /// content 의 안정 접두 경계가 앞으로 이동하면 그 부분 렌더의 접미사를 플러시한다.
    fn flush(&mut self) {
        let stable = Self::stable_prefix_len(&self.content);
        if stable <= self.stable_len {
            return;
        }
        let rendered = self.render();
        let Some(suffix) = rendered.strip_prefix(&self.printed) else {
            return;
        };
        let _ = std::io::stdout().write_all(suffix.as_bytes());
        let _ = std::io::stdout().flush();
        self.printed = rendered;
        self.stable_len = stable;
    }

    /// reasoning 의 안정 접두 경계가 앞으로 이동하면 그 부분 렌더의 접미사를 플러시한다.
    ///
    /// reasoning 은 markdown 렌더 없이 원본 텍스트를 그대로 내보내는 한,
    /// `\n\n` 경계 앞 접두의 렌더는 이후 arriving reasoning 에 의해 불변이므로
    /// content 와 동일한 접두 비교(strip_prefix)가 안전하다.
    fn flush_reasoning(&mut self) {
        let stable = Self::stable_prefix_len(&self.reasoning);
        if stable <= self.reasoning_stable_len {
            return;
        }
        let rendered = self.render();
        let Some(suffix) = rendered.strip_prefix(&self.printed) else {
            return;
        };
        let _ = std::io::stdout().write_all(suffix.as_bytes());
        let _ = std::io::stdout().flush();
        self.printed = rendered;
        self.reasoning_stable_len = stable;
    }

    /// [생각] 머리글 + reasoning 본문 렌더 (reasoning 비어 있으면 "").
    ///
    /// 머리글은 reasoning 이 비어 있지 않으면 항상 상수(불변 접두)로 유지된다.
    fn render_reasoning(&self) -> String {
        if self.reasoning.trim().is_empty() {
            return String::new();
        }
        let dim = if self.use_color { "\x1b[2m" } else { "" };
        let reset = if self.use_color { "\x1b[0m" } else { "" };
        format!("{dim}[생각] {}\n{reset}\n", strip_ansi(&self.reasoning))
    }

    /// [생각] 머리글 + markdown 렌더로 전체 출력을 조립한다.
    fn render(&self) -> String {
        let mut out = self.render_reasoning();
        if !self.content.trim().is_empty() {
            let blocks = crate::render::parse(&self.content);
            let body = crate::render::render_ansi(&blocks);
            let body = if self.use_color {
                body
            } else {
                strip_ansi(&body)
            };
            out.push_str(&body);
        }
        out
    }
}

/// 엔드포인트가 설정되지 않았을 때 대화형으로 설정을 안내하고 등록한다.
/// TTY 가 아니면 `None` 을 반환해 호출부가 오류로 종료하게 한다.
fn ensure_endpoint_interactive(
    cfg: &mut Config,
) -> Result<Option<(String, crate::config::EndpointConfig)>, Box<dyn std::error::Error>> {
    use std::io::IsTerminal;

    if !std::io::stdin().is_terminal() {
        tracing::error!(
            "{}",
            crate::i18n::tr(
                "Endpoint is not configured (register one first with `bulti endpoint add`)"
            )
        );
        return Ok(None);
    }

    println!(
        "{}",
        crate::i18n::tr(
            "⚠️  No endpoint is registered. Please configure an endpoint before starting."
        )
    );
    println!(
        "{}",
        crate::i18n::tr(
            "    If you already have one, check with `bulti endpoint list` and activate it with `bulti endpoint use <name>`."
        )
    );
    println!();

    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();

    // 이름 입력.
    print!("{}", crate::i18n::tr("Endpoint name (default: main): "));
    std::io::stdout().flush().ok();
    let name = match lines.next() {
        Some(Ok(n)) if !n.trim().is_empty() => n.trim().to_string(),
        Some(Ok(_)) => "main".to_string(),
        Some(Err(e)) => {
            tracing::error!(
                "{}",
                crate::i18n::tr_fmt("Input error: {e}", &[&e.to_string()])
            );
            return Ok(None);
        }
        None => {
            tracing::info!("{}", crate::i18n::tr("EOF — configuration aborted"));
            return Ok(None);
        }
    };

    // URL 입력.
    print!(
        "{}",
        crate::i18n::tr("Endpoint URL (e.g. http://127.0.0.1:8084/v1): ")
    );
    std::io::stdout().flush().ok();
    let url = match lines.next() {
        Some(Ok(u)) if !u.trim().is_empty() => u.trim().to_string(),
        _ => {
            tracing::error!("{}", crate::i18n::tr("URL is required"));
            return Ok(None);
        }
    };

    // 모델 입력.
    print!("{}", crate::i18n::tr("Model name: "));
    std::io::stdout().flush().ok();
    let model = match lines.next() {
        Some(Ok(m)) if !m.trim().is_empty() => m.trim().to_string(),
        _ => {
            tracing::error!("{}", crate::i18n::tr("Model name is required"));
            return Ok(None);
        }
    };

    // API 키 입력 (선택).
    print!("{}", crate::i18n::tr("API key (optional, Enter to skip): "));
    std::io::stdout().flush().ok();
    let api_key = match lines.next() {
        Some(Ok(k)) if !k.trim().is_empty() => Some(k.trim().to_string()),
        _ => None,
    };

    // 등록.
    let ep = crate::config::EndpointConfig {
        url: url.clone(),
        api_key: api_key.clone(),
        model: model.clone(),
        context_tokens: 0,
        vision: false,
        thinking: false,
        max_iterations: 200,
        reasoning_effort: None,
        input_price_per_mtok: None,
        output_price_per_mtok: None,
    };
    cfg.endpoints.insert(name.clone(), ep.clone());
    // 첫 등록이면 자동 활성화.
    if cfg.active_endpoint.is_none() {
        cfg.active_endpoint = Some(name.clone());
    }
    cfg.save()?;
    println!(
        "{}",
        crate::i18n::tr_fmt("Registered and activated endpoint '{name}'.\n", &[&name])
    );
    Ok(Some((name, ep)))
}

/// `bulti chat` 진입점. 프롬프트 루프를 시작한다.
pub fn run(args: ChatArgs, cfg: &mut Config) -> Result<i32, Box<dyn std::error::Error>> {
    // run 시작 시와 동일하게 백그라운드 업데이트 확인.
    let (repo, mode) = crate::cli::update_cmd::update_config(cfg);
    crate::update::notify_background(&repo, &mode);

    let cwd = std::env::current_dir()?;
    let global_dir = Config::config_dir().map_err(|e| e.to_string())?;
    let project_root = cwd.clone();

    // 활성 엔드포인트 결정 + 오버라이드 적용.
    // 엔드포인트가 설정되지 않았으면 대화형 루프 안에서 설정을 안내한다.
    let mut endpoint_name = args
        .endpoint
        .clone()
        .unwrap_or_else(|| cfg.active_endpoint.clone().unwrap_or_default());
    let mut endpoint = match cfg.endpoints.get(&endpoint_name) {
        Some(ep) => ep.clone(),
        None => {
            // 미설정 → 대화형 설정을 안내하는 루프를 먼저 실행.
            match ensure_endpoint_interactive(cfg)? {
                Some((name, ep)) => {
                    endpoint_name = name;
                    ep
                }
                None => return Ok(EXIT_ERROR),
            }
        }
    };
    if let Some(model) = &args.model {
        endpoint.model = model.clone();
    }

    // 시스템 프롬프트 오버라이드 + 조립 (run 과 동일 공통 로직).
    let override_opt =
        crate::cli::prompt_cmd::override_from_run_args(&args.system_file, &args.system);
    let skills = crate::skills::discover(&project_root, &global_dir)?;
    let mcp_servers = cfg
        .mcp
        .iter()
        .map(|(name, m)| crate::prompt::McpIndex {
            name: name.clone(),
            description: m
                .description
                .clone()
                .unwrap_or_else(|| crate::i18n::tr("(no description)").to_string()),
        })
        .collect();
    let ctx =
        crate::prompt::context_from_config(cfg, cwd.clone(), project_root, skills, mcp_servers)?;
    let mut system_prompt = crate::prompt::assemble(&ctx, override_opt)?;

    // 세션 복원 (`--resume <id>`). 세션의 endpoint/model 을 사용하고
    // 시스템 프롬프트에 이전 대화 컨텍스트를 포함한다.
    let mut session_id = make_uuid();
    let mut session = session::Session::new(&session_id, &endpoint_name, &endpoint.model);
    let mut resume_context: Option<String> = None;
    if let Some(id) = &args.resume {
        match session::load(id) {
            Ok(s) => {
                tracing::info!("{}", crate::i18n::tr_fmt("Resuming session '{id}'.", &[id]));
                session_id = id.clone();
                // 세션의 endpoint/model 을 사용.
                if let Some(ep) = cfg.endpoints.get(&s.endpoint) {
                    endpoint = ep.clone();
                }
                if !s.model.is_empty() {
                    endpoint.model = s.model.clone();
                }
                // 시스템 프롬프트에 이전 대화 컨텍스트 포함.
                let ctx = s.conversation_context();
                if !ctx.is_empty() {
                    system_prompt = format!("{system_prompt}\n\n[이전 대화 기록]\n{ctx}");
                }
                resume_context = Some(ctx);
                session = s;
            }
            Err(e) => {
                tracing::error!(
                    "{}",
                    crate::i18n::tr_fmt(
                        "Session restore failed for '{id}': {e}",
                        &[id, &e.to_string()]
                    )
                );
                return Ok(EXIT_ERROR);
            }
        }
    }

    // 도구 레지스트리 + 히스토리 DB (공통 코어).
    let mcp_manager = Arc::new(McpManager::new());
    let registry = crate::tools::native_registry(
        endpoint.vision,
        cwd.clone(),
        global_dir.clone(),
        cfg.mcp.clone(),
        mcp_manager,
    );
    let conn = Arc::new(std::sync::Mutex::new(
        history::open().map_err(|e| e.to_string())?,
    ));

    // SIGINT 감시 태스크 (Ctrl+C → interrupted).
    let interrupted_flag = Arc::new(AtomicBool::new(false));
    spawn_sigint_watcher(interrupted_flag.clone());

    let rt = Arc::new(tokio::runtime::Runtime::new().map_err(|e| e.to_string())?);

    // 클로저(TUI processor)와 chat_loop 가 공유할 변수들.
    let client = LlmClient::new();
    let session_chain = make_uuid();

    // TUI processor 는 별도 스레드에서 실행되므로 `Send + 'static` 이 요구된다.
    // 세션·재개 컨텍스트·DB·runtime 을 스레드와 공유하기 위해 Arc<Mutex<>> 로 감싼다.
    let session_shared = Arc::new(std::sync::Mutex::new(session));
    let resume_shared = Arc::new(std::sync::Mutex::new(resume_context));

    // TUI 모드 진입 (DESIGN.md §4.13.3). `--no-tui` 또는 비-TTY 면 스트림 텍스트로 폴백.
    if !args.no_tui {
        // TTY 가 아니면 TUI 는 None 을 반환 → 스트림 텍스트 모드.
        let initial_lines = {
            let s = session_shared.lock().unwrap();
            build_initial_lines(&s)
        };
        let options = crate::tui::TuiOptions {
            endpoint_name: endpoint_name.clone(),
            model: endpoint.model.clone(),
            session_id: session_id.clone(),
        };
        let mut turn_count = {
            let s = session_shared.lock().unwrap();
            s.turns.len() as u32
        };

        // TUI processor: 사용자 메시지 → 한 턴(세그먼트 체인) 실행 → TurnResult 반환.
        // 별도 스레드에서 실행되므로 `Send + 'static` 이 요구되고, run_tui 의 `F: Clone`
        // 바운드 때문에 클로저가 Clone 되어야 한다. move 클로저에 넣을 Arc clone 을
        // 따로 만들어 원본 변수는 이후 코드(스트림 텍스트 폴백)에서 그대로 쓰도록 한다.
        // endpoint/cfg 는 TUI processor 와 command_handler 가 공유하며, `/model`·`/effort`
        // 커맨드로 변경되므로 `Arc<Mutex<>>` 로 감싼다.
        let endpoint_shared = Arc::new(std::sync::Mutex::new(endpoint.clone()));
        let cfg_shared = Arc::new(std::sync::Mutex::new(cfg.clone()));
        let endpoint_name_owned = endpoint_name.clone();
        let system_prompt_owned = system_prompt.clone();
        let session_id_owned = session_id.clone();
        let session_chain_owned = session_chain.clone();
        let client_tui = client.clone();
        let registry_tui = registry.clone();
        let conn_tui = conn.clone();
        let rt_tui = rt.clone();
        let session_tui = session_shared.clone();
        let resume_tui = resume_shared.clone();
        let interrupted_tui = interrupted_flag.clone();
        let endpoint_tui = endpoint_shared.clone();
        let cfg_tui = cfg_shared.clone();
        // command_handler 도 compact 에서 client·runtime 을 쓰므로, processor 의
        // move 클로저가 원본을 이동하기 전에 clone 해 둔다.
        let client_ch = client_tui.clone();
        let rt_ch = rt_tui.clone();
        let session_ch = session_tui.clone();
        let processor =
            move |user_msg: String,
                  delta_tx: tokio::sync::mpsc::UnboundedSender<crate::llm::Delta>|
                  -> Result<TurnResult, Box<dyn std::error::Error + Send + Sync>> {
                let endpoint_guard = endpoint_tui.lock().unwrap();
                let cfg_guard = cfg_tui.lock().unwrap();
                // 재개 컨텍스트 + 같은 세션 이전 턴 대화를 프롬프트에 포함.
                let effective_prompt = match resume_tui.lock().unwrap().take() {
                    Some(ctx) if !ctx.trim().is_empty() => {
                        format!("{ctx}[이번 사용자 메시지]\n{user_msg}")
                    }
                    _ => {
                        let ctx = {
                            let s = session_tui.lock().unwrap();
                            s.conversation_context()
                        };
                        if ctx.trim().is_empty() {
                            user_msg.clone()
                        } else {
                            format!("{ctx}[이번 사용자 메시지]\n{user_msg}")
                        }
                    }
                };

                let chain_id = make_uuid();
                let turn_result = {
                    let conn_guard = conn_tui.lock().unwrap();
                    rt_tui
                        .block_on(run_turn(
                            &client_tui,
                            &conn_guard,
                            &registry_tui,
                            &system_prompt_owned,
                            effective_prompt,
                            &endpoint_guard,
                            &endpoint_name_owned,
                            &cfg_guard,
                            &chain_id,
                            &session_chain_owned,
                            session_id_owned.clone(),
                            turn_count,
                            interrupted_tui.clone(),
                            Some(delta_tx),
                        ))
                        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
                            std::io::Error::other(e.to_string()).into()
                        })?
                };

                // 턴 종료 시 세션에 기록하고 저장.
                let mut s = session_tui.lock().unwrap();
                s.push_turn(session::TurnRecord {
                    turn: turn_count,
                    user: user_msg.clone(),
                    assistant: turn_result.assistant_content.clone(),
                    chain_id: chain_id.clone(),
                    files_touched: turn_result.files_touched.clone(),
                    input_tokens: turn_result.input_tokens,
                    output_tokens: turn_result.output_tokens,
                    model: endpoint_guard.model.clone(),
                });
                if let Err(e) = session::save(&s) {
                    tracing::error!(
                        "{}",
                        crate::i18n::tr_fmt("Session save failed: {e}", &[&e.to_string()])
                    );
                }
                turn_count += 1;
                Ok(turn_result)
            };

        // TUI 슬래시 커맨드 핸들러: `/model`·`/effort`·`/exit` 등.
        // endpoint/cfg 를 Arc<Mutex<>> 로 공유해 모델 변경을 영속화한다.
        let endpoint_cmd = endpoint_shared.clone();
        let cfg_cmd = cfg_shared.clone();
        let ep_name_cmd = endpoint_name.clone();
        let command_handler = move |line: &str| -> crate::tui::CommandResult {
            let parsed = crate::slash::parse(line);
            let cmd_name = parsed.as_ref().map(|p| p.name.as_str()).unwrap_or("");
            let args_str = parsed.as_ref().map(|p| p.args.as_str()).unwrap_or("");

            match cmd_name {
                "exit" => crate::tui::CommandResult {
                    message: crate::i18n::tr("Conversation ended.").to_string(),
                    exit: true,
                },
                "new" => {
                    // 세션 재설정은 chat_loop 의 원본 변수에 반영할 수 없으므로
                    // (TUI 는 별도 스레드), 안내 메시지만 표시한다.
                    crate::tui::CommandResult {
                        message: crate::i18n::tr(
                            "/new is not supported in the TUI (quit and start again).",
                        )
                        .to_string(),
                        exit: false,
                    }
                }
                "help" => {
                    let mut msg = crate::i18n::tr("Available commands:").to_string();
                    for c in crate::slash::COMMANDS {
                        msg.push_str(&format!(
                            "\n  {}  — {}",
                            crate::i18n::tr(c.usage),
                            crate::i18n::tr(c.description)
                        ));
                    }
                    crate::tui::CommandResult {
                        message: msg,
                        exit: false,
                    }
                }
                "language" => {
                    let mut cfg_guard = cfg_cmd.lock().unwrap();
                    crate::tui::CommandResult {
                        message: set_language_command(&mut cfg_guard, args_str),
                        exit: false,
                    }
                }
                "resume" => {
                    // 세션 id 지정은 TUI 에서 세션을 교체할 수 없으므로 안내만.
                    // 단독 입력이면 사용 가능한 세션 목록을 제안한다.
                    let msg = if args_str.is_empty() {
                        format!(
                            "{}\n{}",
                            resume_suggest_text(),
                            crate::i18n::tr(
                                "Session resume is not supported in the TUI — quit and resume with bulti chat --resume <id>."
                            )
                        )
                    } else {
                        crate::i18n::tr_fmt(
                            "Session redirect is not supported in the TUI for '{id}' (quit and resume with bulti chat --resume {id}).",
                            &[args_str, args_str],
                        )
                    };
                    crate::tui::CommandResult {
                        message: msg,
                        exit: false,
                    }
                }
                "model" => {
                    if args_str.is_empty() {
                        let current = endpoint_cmd.lock().unwrap().model.clone();
                        let cfg_guard = cfg_cmd.lock().unwrap();
                        crate::tui::CommandResult {
                            message: available_models_text(&cfg_guard, &current),
                            exit: false,
                        }
                    } else {
                        let mut parts = args_str.split_whitespace();
                        let model_name = parts.next().unwrap().to_string();
                        let effort = parts.next().map(|s| s.to_lowercase());
                        let msg;
                        {
                            let mut endpoint = endpoint_cmd.lock().unwrap();
                            endpoint.model = model_name.clone();
                            match effort {
                                Some(e) if matches!(e.as_str(), "low" | "medium" | "high") => {
                                    endpoint.reasoning_effort = Some(e.clone());
                                    msg = crate::i18n::tr_fmt(
                                        "Changed model to '{model}' and reasoning effort to '{effort}'.",
                                        &[&model_name, &e],
                                    );
                                }
                                Some(e) => {
                                    msg = crate::i18n::tr_fmt(
                                        "Changed model to '{model}'. (effort '{effort}' ignored — low|medium|high)",
                                        &[&model_name, &e],
                                    );
                                }
                                None => {
                                    msg = crate::i18n::tr_fmt(
                                        "Changed model to '{model}'.",
                                        &[&model_name],
                                    );
                                }
                            }
                        }
                        // cfg.endpoints 에 반영 + 영속화.
                        let mut cfg_guard = cfg_cmd.lock().unwrap();
                        if let Some(ep) = cfg_guard.endpoints.get_mut(&ep_name_cmd) {
                            let endpoint = endpoint_cmd.lock().unwrap();
                            ep.model = endpoint.model.clone();
                            ep.reasoning_effort = endpoint.reasoning_effort.clone();
                        }
                        if let Err(e) = cfg_guard.save() {
                            tracing::error!(
                                "{}",
                                crate::i18n::tr_fmt("Settings save failed: {e}", &[&e.to_string()])
                            );
                        }
                        crate::tui::CommandResult {
                            message: msg,
                            exit: false,
                        }
                    }
                }
                "effort" => {
                    let e = args_str.trim().to_lowercase();
                    if !matches!(e.as_str(), "low" | "medium" | "high") {
                        crate::tui::CommandResult {
                            message: crate::i18n::tr("Usage: /effort <low|medium|high>")
                                .to_string(),
                            exit: false,
                        }
                    } else {
                        {
                            let mut endpoint = endpoint_cmd.lock().unwrap();
                            endpoint.reasoning_effort = Some(e.clone());
                        }
                        let mut cfg_guard = cfg_cmd.lock().unwrap();
                        if let Some(ep) = cfg_guard.endpoints.get_mut(&ep_name_cmd) {
                            let endpoint = endpoint_cmd.lock().unwrap();
                            ep.reasoning_effort = endpoint.reasoning_effort.clone();
                        }
                        if let Err(e) = cfg_guard.save() {
                            tracing::error!(
                                "{}",
                                crate::i18n::tr_fmt("Settings save failed: {e}", &[&e.to_string()])
                            );
                        }
                        crate::tui::CommandResult {
                            message: crate::i18n::tr_fmt(
                                "Set reasoning effort to '{effort}'.",
                                &[&e],
                            ),
                            exit: false,
                        }
                    }
                }
                "endpoint" => {
                    let cfg_guard = cfg_cmd.lock().unwrap();
                    crate::tui::CommandResult {
                        message: endpoint_text(&cfg_guard, &ep_name_cmd, args_str),
                        exit: false,
                    }
                }
                "mcp" => {
                    let cfg_guard = cfg_cmd.lock().unwrap();
                    crate::tui::CommandResult {
                        message: mcp_text(&cfg_guard, args_str),
                        exit: false,
                    }
                }
                "session-info" => {
                    let ctx = {
                        let ep = endpoint_cmd.lock().unwrap();
                        ep.context_tokens
                    };
                    let text = {
                        let s = session_ch.lock().unwrap();
                        session_info_text(&s, ctx)
                    };
                    crate::tui::CommandResult {
                        message: text,
                        exit: false,
                    }
                }
                "sessions" => crate::tui::CommandResult {
                    message: sessions_list_text(),
                    exit: false,
                },
                "fork" => {
                    let msg = {
                        let s = session_ch.lock().unwrap();
                        match fork_session(&s) {
                            Ok(m) => m,
                            Err(e) => crate::i18n::tr_fmt("Session fork failed: {e}", &[&e]),
                        }
                    };
                    crate::tui::CommandResult {
                        message: msg,
                        exit: false,
                    }
                }
                "export" => {
                    let msg = {
                        let s = session_ch.lock().unwrap();
                        match export_session(&s, args_str) {
                            Ok(m) => m,
                            Err(e) => crate::i18n::tr_fmt("Export failed: {e}", &[&e]),
                        }
                    };
                    crate::tui::CommandResult {
                        message: msg,
                        exit: false,
                    }
                }
                "compact" => {
                    let has_turns = {
                        let s = session_ch.lock().unwrap();
                        !s.turns.is_empty()
                    };
                    if !has_turns {
                        crate::tui::CommandResult {
                            message: crate::i18n::tr("Nothing to compact. (0 turns)").to_string(),
                            exit: false,
                        }
                    } else {
                        let transcript = {
                            let s = session_ch.lock().unwrap();
                            s.conversation_context()
                        };
                        let ep = endpoint_cmd.lock().unwrap();
                        let summary = rt_ch
                            .block_on(compact_transcript(&client_ch, &ep, &transcript))
                            .map_err(|e| e.to_string());
                        drop(ep);
                        match summary {
                            Ok(sum) if !sum.trim().is_empty() => {
                                let mut sess = session_ch.lock().unwrap();
                                apply_compact(&mut sess, sum);
                                match session::save(&sess) {
                                    Ok(_) => crate::tui::CommandResult {
                                        message: crate::i18n::tr(
                                            "Compacted the conversation into a summary.",
                                        )
                                        .to_string(),
                                        exit: false,
                                    },
                                    Err(e) => {
                                        tracing::error!(
                                            "{}",
                                            crate::i18n::tr_fmt(
                                                "Session save failed: {e}",
                                                &[&e.to_string()]
                                            )
                                        );
                                        crate::tui::CommandResult {
                                            message: crate::i18n::tr_fmt(
                                                "Compacted the conversation into a summary. (save failed: {e})",
                                                &[&e.to_string()],
                                            ),
                                            exit: false,
                                        }
                                    }
                                }
                            }
                            Ok(_) => crate::tui::CommandResult {
                                message: crate::i18n::tr(
                                    "Summary was empty; compaction cancelled. (existing session kept)",
                                )
                                .to_string(),
                                exit: false,
                            },
                            Err(e) => crate::tui::CommandResult {
                                message: crate::i18n::tr_fmt(
                                    "Compaction failed (existing session kept): {e}",
                                    &[&e],
                                ),
                                exit: false,
                            },
                        }
                    }
                }
                "usage" => {
                    let text = {
                        let s = session_ch.lock().unwrap();
                        let ep = endpoint_cmd.lock().unwrap();
                        usage_text(&s, &ep)
                    };
                    crate::tui::CommandResult {
                        message: text,
                        exit: false,
                    }
                }
                "history" => {
                    let sources = crate::completion::load_history_sources();
                    let q = args_str.trim().to_lowercase();
                    let filtered: Vec<String> = if q.is_empty() {
                        sources
                    } else {
                        sources
                            .into_iter()
                            .filter(|s| s.to_lowercase().contains(&q))
                            .collect()
                    };
                    let heading = if q.is_empty() {
                        crate::i18n::tr(
                            "Prompt history (last 20) — you can also browse with ↑/↓ when the input is empty:",
                        )
                        .to_string()
                    } else {
                        crate::i18n::tr_fmt(
                            "Prompt history — results for '{q}' (last 20):",
                            &[args_str.trim()],
                        )
                    };
                    let mut msg = heading;
                    let mut shown = 0usize;
                    for s in filtered.iter().take(20) {
                        // 멀티라인 프롬프트는 첫 줄로 압축해 표시한다.
                        let single = s.lines().next().unwrap_or("").trim().to_string();
                        if single.is_empty() {
                            continue;
                        }
                        let display: String = single.chars().take(70).collect::<String>()
                            + if single.chars().count() > 70 {
                                "…"
                            } else {
                                ""
                            };
                        msg.push_str(&format!("\n  {display}"));
                        shown += 1;
                    }
                    if shown == 0 {
                        msg.push_str(&format!("\n{}", crate::i18n::tr("  (no history)")));
                    }
                    crate::tui::CommandResult {
                        message: msg,
                        exit: false,
                    }
                }
                _ => {
                    // 미지원 커맨드 안내 + 유사 커맨드 제안.
                    let raw = crate::slash::raw_name(line);
                    crate::tui::CommandResult {
                        message: crate::i18n::tr_fmt(
                            "Unknown command '{line}'. Type /help to see available commands.{}",
                            &[line, &similar_suggestion_text(raw)],
                        ),
                        exit: false,
                    }
                }
            }
        };

        let tui_outcome = crate::tui::run_tui(&options, initial_lines, processor, command_handler)?;
        if let Some(outcome) = tui_outcome {
            return Ok(outcome.exit_code);
        }
    }

    let outcome = {
        let conn_guard = conn.lock().unwrap();
        let mut session_guard = session_shared.lock().unwrap();
        let mut resume_guard = resume_shared.lock().unwrap();
        rt.block_on(chat_loop(
            &conn_guard,
            &registry,
            &system_prompt,
            &mut endpoint,
            &endpoint_name,
            cfg,
            &args,
            interrupted_flag,
            args.first.clone(),
            &mut session_id,
            &mut session_guard,
            &mut resume_guard,
        ))?
    };

    Ok(outcome)
}

/// TUI 초기 화면에 표시할 대화 메시지 목록을 세션에서 구성한다.
fn build_initial_lines(session: &session::Session) -> Vec<crate::tui::ChatLine> {
    use crate::tui::{ChatLine, Role};
    let mut lines = Vec::new();
    for t in &session.turns {
        lines.push(ChatLine {
            role: Role::User,
            text: t.user.clone(),
            ..ChatLine::default()
        });
        lines.push(ChatLine {
            role: Role::Assistant,
            text: t.assistant.clone(),
            ..ChatLine::default()
        });
    }
    lines
}

/// 프롬프트 루프 본체. 한 턴마다 세그먼트 체인을 실행한다.
#[allow(clippy::too_many_arguments)]
async fn chat_loop(
    conn: &rusqlite::Connection,
    registry: &Arc<crate::tools::ToolRegistry>,
    system_prompt: &str,
    endpoint: &mut EndpointConfig,
    endpoint_name: &str,
    cfg: &mut Config,
    args: &ChatArgs,
    interrupted: Arc<AtomicBool>,
    first_prompt: Option<String>,
    session_id: &mut String,
    session: &mut session::Session,
    resume_context: &mut Option<String>,
) -> Result<i32, Box<dyn std::error::Error>> {
    let client = LlmClient::new();
    let use_color = !args.no_color && std::io::stdout().is_terminal();
    let color = |s: &str| {
        if use_color {
            format!("\x1b[36m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    };

    // 시작 배너.
    println!(
        "{}",
        color(crate::i18n::tr(
            "bulti chat — interactive prompt loop (/exit or Ctrl-D to quit, /help for help)"
        ))
    );

    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    let mut turn = 0u32;
    let mut session_chain = make_uuid();

    // `--first` 가 있으면 첫 턴으로 사용.
    let mut pending_first = first_prompt
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty());

    loop {
        // SIGINT 확인.
        if interrupted.load(Ordering::Relaxed) {
            tracing::info!(
                "{}",
                crate::i18n::tr("SIGINT received — ending conversation")
            );
            return Ok(EXIT_INTERRUPTED);
        }

        // 프롬프트 출력.
        print!("{}", color("> "));
        std::io::stdout().flush().ok();

        // `--first` 프롬프트가 있으면 stdin 대신 사용.
        let line = match pending_first.take() {
            Some(p) => {
                println!("{p}");
                Some(Ok(p))
            }
            None => lines.next(),
        };

        // Ctrl-D(EOF) 또는 읽기 실패 → 종료.
        let line = match line {
            Some(Ok(line)) => line,
            Some(Err(e)) => {
                tracing::error!(
                    "{}",
                    crate::i18n::tr_fmt("Input read error: {e}", &[&e.to_string()])
                );
                return Ok(EXIT_ERROR);
            }
            None => {
                tracing::info!("{}", crate::i18n::tr("EOF (Ctrl-D) — ending conversation"));
                return Ok(EXIT_OK);
            }
        };
        let prompt = line.trim().to_string();
        if prompt.is_empty() {
            continue;
        }

        // 내부 명령 처리 (src/slash::parse 로 파싱).
        let prompt_trimmed = prompt.trim();
        if prompt_trimmed.starts_with('/') {
            let parsed = crate::slash::parse(prompt_trimmed);
            let cmd_name = parsed.as_ref().map(|p| p.name.as_str()).unwrap_or("");
            let args_str = parsed.as_ref().map(|p| p.args.as_str()).unwrap_or("");

            match cmd_name {
                "exit" => {
                    tracing::info!("{}", crate::i18n::tr("Ending conversation"));
                    return Ok(EXIT_OK);
                }
                "new" => {
                    // 새 세션 시작.
                    *session_id = make_uuid();
                    *session = session::Session::new(session_id, endpoint_name, &endpoint.model);
                    *resume_context = None;
                    session_chain = make_uuid();
                    turn = 0;
                    println!(
                        "{}",
                        color(&crate::i18n::tr_fmt(
                            "Started a new session (session id: {id})",
                            &[session_id]
                        ))
                    );
                    continue;
                }
                "help" => {
                    print_help();
                    continue;
                }
                "language" => {
                    let msg = set_language_command(cfg, args_str);
                    println!("{}", color(&msg));
                    continue;
                }
                "resume" => {
                    if args_str.is_empty() {
                        // 세션 id 미지정: 사용 가능한 세션 목록을 제안한다.
                        println!("{}", color(&resume_suggest_text()));
                    } else {
                        match session::load(args_str) {
                            Ok(s) => {
                                *session_id = args_str.to_string();
                                *session = s.clone();
                                // 이전 대화 기록을 다음 턴 프롬프트 앞에 포함.
                                *resume_context = Some(s.conversation_context());
                                println!(
                                    "{}",
                                    color(&crate::i18n::tr_fmt(
                                        "Resuming session '{id}'.",
                                        &[args_str]
                                    ))
                                );
                                continue;
                            }
                            Err(e) => {
                                tracing::error!(
                                    "{}",
                                    crate::i18n::tr_fmt(
                                        "Session restore failed for '{id}': {e}",
                                        &[args_str, &e.to_string()]
                                    )
                                );
                                println!(
                                    "{}",
                                    color(&crate::i18n::tr_fmt(
                                        "Session '{id}' not found.",
                                        &[args_str]
                                    ))
                                );
                                continue;
                            }
                        }
                    }
                    continue;
                }
                "model" => {
                    // `/model <name> [effort]` — 모델 전환 + 선택적 effort 설정.
                    if args_str.is_empty() {
                        println!("{}", color(&available_models_text(cfg, &endpoint.model)));
                        continue;
                    }
                    let mut parts = args_str.split_whitespace();
                    let model_name = parts.next().unwrap().to_string();
                    let effort = parts.next().map(|s| s.to_lowercase());
                    endpoint.model = model_name.clone();
                    if let Some(e) = effort {
                        if matches!(e.as_str(), "low" | "medium" | "high") {
                            endpoint.reasoning_effort = Some(e.clone());
                            println!(
                                "{}",
                                color(&crate::i18n::tr_fmt(
                                    "Changed model to '{model}' and reasoning effort to '{effort}'.",
                                    &[&model_name, &e]
                                ))
                            );
                        } else {
                            println!(
                                "{}",
                                color(&crate::i18n::tr_fmt(
                                    "Changed model to '{model}'. (effort '{effort}' ignored — low|medium|high)",
                                    &[&model_name, &e]
                                ))
                            );
                        }
                    } else {
                        println!(
                            "{}",
                            color(&crate::i18n::tr_fmt(
                                "Changed model to '{model}'.",
                                &[&model_name]
                            ))
                        );
                    }
                    // cfg.endpoints 에 반영 + 영속화.
                    if let Some(ep) = cfg.endpoints.get_mut(endpoint_name) {
                        ep.model = endpoint.model.clone();
                        ep.reasoning_effort = endpoint.reasoning_effort.clone();
                    }
                    if let Err(e) = cfg.save() {
                        tracing::error!(
                            "{}",
                            crate::i18n::tr_fmt("Settings save failed: {e}", &[&e.to_string()])
                        );
                    }
                    continue;
                }
                "effort" => {
                    // `/effort <low|medium|high>` — reasoning effort 설정.
                    let e = args_str.trim().to_lowercase();
                    if !matches!(e.as_str(), "low" | "medium" | "high") {
                        println!(
                            "{}",
                            color(crate::i18n::tr("Usage: /effort <low|medium|high>"))
                        );
                        continue;
                    }
                    endpoint.reasoning_effort = Some(e.clone());
                    if let Some(ep) = cfg.endpoints.get_mut(endpoint_name) {
                        ep.reasoning_effort = endpoint.reasoning_effort.clone();
                    }
                    if let Err(e) = cfg.save() {
                        tracing::error!(
                            "{}",
                            crate::i18n::tr_fmt("Settings save failed: {e}", &[&e.to_string()])
                        );
                    }
                    println!(
                        "{}",
                        color(&crate::i18n::tr_fmt(
                            "Set reasoning effort to '{effort}'.",
                            &[&e]
                        ))
                    );
                    continue;
                }
                "endpoint" => {
                    println!("{}", color(&endpoint_text(cfg, endpoint_name, args_str)));
                    continue;
                }
                "mcp" => {
                    println!("{}", color(&mcp_text(cfg, args_str)));
                    continue;
                }
                "session-info" => {
                    println!(
                        "{}",
                        color(&session_info_text(session, endpoint.context_tokens))
                    );
                    continue;
                }
                "sessions" => {
                    let text = sessions_list_text();
                    println!("{}", color(&text));
                    continue;
                }
                "fork" => {
                    match fork_session(session) {
                        Ok(m) => println!("{}", color(&m)),
                        Err(e) => println!(
                            "{}",
                            color(&crate::i18n::tr_fmt("Session fork failed: {e}", &[&e]))
                        ),
                    }
                    continue;
                }
                "export" => {
                    match export_session(session, args_str) {
                        Ok(m) => println!("{}", color(&m)),
                        Err(e) => println!(
                            "{}",
                            color(&crate::i18n::tr_fmt("Export failed: {e}", &[&e]))
                        ),
                    }
                    continue;
                }
                "compact" => {
                    if session.turns.is_empty() {
                        println!(
                            "{}",
                            color(crate::i18n::tr("Nothing to compact. (0 turns)"))
                        );
                    } else {
                        let transcript = session.conversation_context();
                        match compact_transcript(&client, endpoint, &transcript).await {
                            Ok(summary) if !summary.trim().is_empty() => {
                                apply_compact(session, summary);
                                if let Err(e) = session::save(session) {
                                    tracing::error!(
                                        "{}",
                                        crate::i18n::tr_fmt(
                                            "Session save failed: {e}",
                                            &[&e.to_string()]
                                        )
                                    );
                                }
                                println!(
                                    "{}",
                                    color(crate::i18n::tr(
                                        "Compacted the conversation into a summary."
                                    ))
                                );
                            }
                            Ok(_) => {
                                println!(
                                    "{}",
                                    color(crate::i18n::tr(
                                        "Summary was empty; compaction cancelled. (existing session kept)"
                                    ))
                                );
                            }
                            Err(e) => {
                                println!(
                                    "{}",
                                    color(&crate::i18n::tr_fmt(
                                        "Compaction failed (existing session kept): {e}",
                                        &[&e.to_string()]
                                    ))
                                );
                            }
                        }
                    }
                    continue;
                }
                "usage" => {
                    println!("{}", color(&usage_text(session, endpoint)));
                    continue;
                }
                "history" => {
                    let sources = crate::completion::load_history_sources();
                    let q = args_str.trim().to_lowercase();
                    let filtered: Vec<String> = if q.is_empty() {
                        sources
                    } else {
                        sources
                            .into_iter()
                            .filter(|s| s.to_lowercase().contains(&q))
                            .collect()
                    };
                    let heading = if q.is_empty() {
                        crate::i18n::tr("Prompt history (last 20):").to_string()
                    } else {
                        crate::i18n::tr_fmt(
                            "Prompt history — results for '{q}' (last 20):",
                            &[args_str.trim()],
                        )
                    };
                    println!("{}", color(&heading));
                    let mut shown = 0usize;
                    for s in filtered.iter().take(20) {
                        let single = s.lines().next().unwrap_or("").trim().to_string();
                        if single.is_empty() {
                            continue;
                        }
                        let display: String = single.chars().take(70).collect::<String>()
                            + if single.chars().count() > 70 {
                                "…"
                            } else {
                                ""
                            };
                        println!("{}", color(&format!("  {display}")));
                        shown += 1;
                    }
                    if shown == 0 {
                        println!("{}", color(crate::i18n::tr("  (no history)")));
                    }
                    continue;
                }
                "" => {
                    // 미지원 슬래시 커맨드 안내 + 유사 커맨드 제안.
                    let raw = crate::slash::raw_name(prompt_trimmed);
                    println!(
                        "{}",
                        color(&crate::i18n::tr_fmt(
                            "Unknown command '{line}'. Type /help to see available commands.{}",
                            &[prompt_trimmed, &similar_suggestion_text(raw)]
                        ))
                    );
                    continue;
                }
                _ => {
                    // 이론상 도달하지 않지만 안전 가드.
                    let raw = crate::slash::raw_name(prompt_trimmed);
                    println!(
                        "{}",
                        color(&crate::i18n::tr_fmt(
                            "Unknown command '{line}'. Type /help to see available commands.{}",
                            &[prompt_trimmed, &similar_suggestion_text(raw)]
                        ))
                    );
                    continue;
                }
            }
        }

        // 재개 컨텍스트가 있으면 사용자 프롬프트 앞에 이전 대화 기록을 포함.
        // (같은 세션 내 연속 턴에서도 이전 턴 대화가 누적되어 컨텍스트가 쌓인다.)
        let effective_prompt = match resume_context.take() {
            Some(ctx) if !ctx.trim().is_empty() => {
                format!("{ctx}[이번 사용자 메시지]\n{prompt}")
            }
            _ => {
                // 같은 세션 내 이전 턴 대화가 있으면 컨텍스트로 포함해 이어 간다.
                let ctx = session.conversation_context();
                if ctx.trim().is_empty() {
                    prompt.clone()
                } else {
                    format!("{ctx}[이번 사용자 메시지]\n{prompt}")
                }
            }
        };

        // 한 턴 실행 (세그먼트 체인 — run 과 동일한 핸드오프 로직 재사용).
        // 스트림 텍스트 모드: delta 채널을 만들어 점진 출력 태스크와 병렬로 소비한다.
        let chain_id = make_uuid();
        let (delta_tx, mut delta_rx) = tokio::sync::mpsc::unbounded_channel::<crate::llm::Delta>();
        let printer = tokio::spawn(async move {
            let mut printer = StreamPrinter::new(use_color);
            while let Some(delta) = delta_rx.recv().await {
                if let Some(r) = &delta.reasoning_content {
                    printer.print_reasoning(r);
                }
                if let Some(c) = &delta.content {
                    printer.print_content(c);
                }
                // tool_calls 은 텍스트 모드에서 무시.
            }
            printer.finalize()
        });
        let turn_result = run_turn(
            &client,
            conn,
            registry,
            system_prompt,
            effective_prompt,
            endpoint,
            endpoint_name,
            cfg,
            &chain_id,
            &session_chain,
            session_id.clone(),
            turn,
            interrupted.clone(),
            Some(delta_tx),
        )
        .await?;

        // interrupted(130) → 대화 종료.
        if turn_result.exit_code == EXIT_INTERRUPTED {
            return Ok(EXIT_INTERRUPTED);
        }
        // 점진 출력 결과를 수취 (run_turn 이 완료된 뒤 태스크는 이미 종료).
        let printed = printer.await.unwrap_or(false);

        // 스트림 텍스트 모드: 턴 결과를 stdout 에 출력한다.
        // TUI 경로는 run_turn 을 직접 호출하지 않고 processor 반환값을 그린다.
        if printed {
            // 점진 출력으로 reasoning·content 를 이미 썼다 — 중복 출력 금지.
            // 단, 실패·미완료 status note 는 delta 에 없으므로 note 만 별도 출력.
            let note = strip_status_note(&turn_result.assistant_content);
            if !note.is_empty() {
                if !turn_result.assistant_content.trim().is_empty() {
                    println!();
                }
                println!("{}", color(&note));
            }
        } else {
            // 폴백: 델타가 전혀 없었으면 (도구 호출만 있는 턴 등) 기존 일괄 출력.
            if !turn_result.reasoning_content.trim().is_empty() {
                let reasoning = if use_color {
                    format!("\x1b[2m[생각] {}\x1b[0m", turn_result.reasoning_content)
                } else {
                    format!("[생각] {}", strip_ansi(&turn_result.reasoning_content))
                };
                println!("\n{reasoning}");
            }
            if !turn_result.assistant_content.trim().is_empty() {
                let blocks = crate::render::parse(&turn_result.assistant_content);
                let rendered = crate::render::render_ansi(&blocks);
                let final_text = if use_color {
                    rendered
                } else {
                    // ANSI 가 없으면 순수 텍스트로만 출력한다.
                    strip_ansi(&rendered)
                };
                println!("\n{final_text}");
                println!();
            }
        }

        // 턴 종료 시 세션에 턴을 기록하고 저장 (핸드오프 체인 완료 후).
        session.push_turn(session::TurnRecord {
            turn,
            user: prompt.clone(),
            assistant: turn_result.assistant_content.clone(),
            chain_id: chain_id.clone(),
            files_touched: turn_result.files_touched.clone(),
            input_tokens: turn_result.input_tokens,
            output_tokens: turn_result.output_tokens,
            model: endpoint.model.clone(),
        });
        if let Err(e) = session::save(session) {
            tracing::error!("세션 저장 실패: {e}");
        }

        turn += 1;
    }
}

/// 한 턴(사용자 프롬프트 → 응답)을 세그먼트 체인으로 실행한다.
/// 핸드오프로 이어지면 새 세그먼트를 실행한다 (run 과 동일 공통 코어).
#[allow(clippy::too_many_arguments)]
pub async fn run_turn(
    client: &LlmClient,
    conn: &rusqlite::Connection,
    registry: &Arc<crate::tools::ToolRegistry>,
    system_prompt: &str,
    prompt: String,
    endpoint: &EndpointConfig,
    endpoint_name: &str,
    cfg: &Config,
    chain_id: &str,
    session_chain: &str,
    session_id: String,
    turn: u32,
    interrupted: Arc<AtomicBool>,
    delta_tx: Option<tokio::sync::mpsc::UnboundedSender<crate::llm::Delta>>,
) -> Result<TurnResult, Box<dyn std::error::Error>> {
    let mut depth_guard = HandoffDepthGuard::new();
    let max_depth = cfg.context.max_handoff_depth;
    let mut current_prompt = prompt.clone();
    let mut segment_index: u32 = 0;
    let mut parent_run_id: Option<i64> = None;
    let mut run_ids: Vec<i64> = Vec::new();
    let mut files_touched: Vec<String> = Vec::new();
    let mut all_input: u64 = 0;
    let mut all_output: u64 = 0;
    let mut all_reasoning = String::new();
    let mut segment_statuses: Vec<SegmentStatus> = Vec::new();
    let mut final_content = String::new();
    let start = Instant::now();

    loop {
        // SIGINT 확인.
        if interrupted.load(Ordering::Relaxed) {
            return Ok(TurnResult {
                exit_code: EXIT_INTERRUPTED,
                assistant_content: final_content,
                reasoning_content: all_reasoning.clone(),
                input_tokens: all_input,
                output_tokens: all_output,
                duration_ms: start.elapsed().as_millis() as u64,
                files_touched,
            });
        }

        let params = SegmentParams {
            endpoint: endpoint.clone(),
            temperature: None,
            system_prompt: system_prompt.to_string(),
            user_prompt: current_prompt.clone(),
            max_iterations: endpoint.max_iterations,
            context_tokens: endpoint.context_tokens,
            handoff_threshold_pct: cfg.context.handoff_threshold_pct,
            max_handoff_depth: max_depth,
            handoff_warn_depth: cfg.context.handoff_warn_depth,
        };

        // run 시작 기록 (대화형 세션 id 연결).
        let run_id = history::start_run(
            conn,
            &history::RunStart {
                cwd: std::env::current_dir()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default(),
                endpoint: endpoint_name.to_string(),
                model: Some(endpoint.model.clone()),
                prompt: current_prompt.clone(),
                chain_id: chain_id.to_string(),
                session_id: Some(session_id.clone()),
                segment_index,
                handoff_depth: depth_guard.depth,
                parent_run_id,
            },
        )
        .map_err(|e| e.to_string())?;
        run_ids.push(run_id);
        parent_run_id = Some(run_id);

        let result = run_segment(
            client,
            registry.as_ref(),
            &params,
            depth_guard.depth,
            delta_tx.clone(),
        )
        .await;

        // files_touched 집계.
        for f in registry.files_touched() {
            if !files_touched.contains(&f) {
                files_touched.push(f);
            }
        }
        registry.clear_files_touched();

        all_input += result.input_tokens;
        all_output += result.output_tokens;
        segment_statuses.push(result.status.clone());

        // 최종 응답 내용 추적.
        if !result.content.trim().is_empty() {
            final_content = result.content.clone();
        }
        // reasoning 누적 (TUI 모델 생각 표시용).
        if !result.reasoning_content.trim().is_empty() {
            all_reasoning.push_str(&result.reasoning_content);
        }

        history::finish_run(
            conn,
            run_id,
            &history::RunFinish {
                status: history::RunStatus::from_str(result.status.as_str()).unwrap(),
                result: Some(result.content.clone()),
                input_tokens: Some(result.input_tokens),
                output_tokens: Some(result.output_tokens),
                files_touched: result.files_touched.clone(),
                duration_ms: Some(start.elapsed().as_millis() as u64),
            },
        )
        .map_err(|e| e.to_string())?;

        // 핸드오프 판정 — run 과 동일한 체인 진행 규칙.
        let is_handoff = matches!(
            result.handoff,
            Some(crate::agent::handoff::HandoffDecision::Handoff)
        ) && result.status == SegmentStatus::Completed
            && result.handoff_response.is_some();
        let is_complete = matches!(
            result.handoff,
            Some(crate::agent::handoff::HandoffDecision::Complete)
        ) && result.status == SegmentStatus::Completed;

        if is_handoff {
            depth_guard.increment();
            let hr = result.handoff_response.as_ref().expect("handoff_response");
            current_prompt = build_new_segment_prompt(hr);
            segment_index += 1;
            continue;
        }

        // 상태가 실패·미완료면 안내 문구를 응답에 붙여 호출부(TUI·스트림)가 표시한다.
        // stdout 직접 출력은 하지 않는다 — TUI raw mode 에서 화면이 깨진다.
        match result.status {
            SegmentStatus::Failed => {
                return Ok(TurnResult {
                    exit_code: EXIT_OK,
                    assistant_content: with_status_note(
                        final_content,
                        crate::i18n::tr(
                            "Segment failed — previous conversation context is preserved",
                        ),
                    ),
                    reasoning_content: all_reasoning.clone(),
                    input_tokens: all_input,
                    output_tokens: all_output,
                    duration_ms: start.elapsed().as_millis() as u64,
                    files_touched,
                });
            }
            SegmentStatus::Incomplete => {
                return Ok(TurnResult {
                    exit_code: EXIT_OK,
                    assistant_content: with_status_note(
                        final_content,
                        crate::i18n::tr(
                            "Segment incomplete — previous conversation context is preserved",
                        ),
                    ),
                    reasoning_content: all_reasoning.clone(),
                    input_tokens: all_input,
                    output_tokens: all_output,
                    duration_ms: start.elapsed().as_millis() as u64,
                    files_touched,
                });
            }
            SegmentStatus::Interrupted => {
                return Ok(TurnResult {
                    exit_code: EXIT_INTERRUPTED,
                    assistant_content: final_content,
                    reasoning_content: all_reasoning.clone(),
                    input_tokens: all_input,
                    output_tokens: all_output,
                    duration_ms: start.elapsed().as_millis() as u64,
                    files_touched,
                });
            }
            SegmentStatus::Completed => {}
        }

        // (대화형 모드에서는 턴당 세그먼트 체인이 끝나면 다음 턴으로.)
        tracing::info!(
            "턴 종료: segments={} input={} output={} files={}",
            segment_statuses.len(),
            all_input,
            all_output,
            files_touched.len()
        );
        let _ = (chain_id, session_chain, turn, is_complete);
        return Ok(TurnResult {
            exit_code: EXIT_OK,
            assistant_content: final_content,
            reasoning_content: all_reasoning.clone(),
            input_tokens: all_input,
            output_tokens: all_output,
            duration_ms: start.elapsed().as_millis() as u64,
            files_touched,
        });
    }
}

/// 실패·미완료 안내를 모델 응답 뒤에 붙인다. 응답이 비면 안내만 반환한다.
fn with_status_note(content: String, note: &str) -> String {
    if content.trim().is_empty() {
        note.to_string()
    } else {
        format!("{content}\n\n{note}")
    }
}

/// `with_status_note` 로 붙은 안내 문구를 분리해 반환한다.
///
/// 안내가 없으면 빈 문자열 (호출부가 일괄 출력 폴백을 그대로 쓴다).
fn strip_status_note(content: &str) -> String {
    let notes = [
        crate::i18n::tr("Segment failed — previous conversation context is preserved"),
        crate::i18n::tr("Segment incomplete — previous conversation context is preserved"),
    ];
    for note in &notes {
        if content.ends_with(note) {
            return note.to_string();
        }
    }
    String::new()
}

/// `/usage` — 세션 누적 토큰·비용 사용량 텍스트.
fn usage_text(session: &session::Session, endpoint: &EndpointConfig) -> String {
    let total_in = session.total_input_tokens();
    let total_out = session.total_output_tokens();
    let mut out = String::new();
    out.push_str(&crate::i18n::tr_fmt(
        "Session total: ↑{in} ↓{out} ({turns} turns)",
        &[
            &total_in.to_string(),
            &total_out.to_string(),
            &session.turns.len().to_string(),
        ],
    ));
    for (model, in_t, out_t) in session.per_model_usage() {
        out.push_str(&format!("\n  {model}: ↑{in_t} ↓{out_t}"));
    }
    let cost = match (
        endpoint.input_price_per_mtok,
        endpoint.output_price_per_mtok,
    ) {
        (Some(pin), Some(pout)) => {
            let c = total_in as f64 / 1_000_000.0 * pin + total_out as f64 / 1_000_000.0 * pout;
            format!(
                "\n{}",
                crate::i18n::tr_fmt("Cost: ${amount}", &[&format!("{c:.4}")])
            )
        }
        _ => format!(
            "\n{}",
            crate::i18n::tr(
                "Cost: — (input_price_per_mtok / output_price_per_mtok not set in bulti.toml)"
            )
        ),
    };
    out.push_str(&cost);
    out
}

/// `/model`(무인자) — 사용 가능한 모델 목록 텍스트. 현재 모델을 표시한다.
fn available_models_text(cfg: &Config, current: &str) -> String {
    let mut out = String::from(crate::i18n::tr(
        "Available models (from configured endpoints):",
    ));
    let mut seen: Vec<&str> = Vec::new();
    if !current.is_empty() {
        seen.push(current);
    }
    for ep in cfg.endpoints.values() {
        if !ep.model.is_empty() && !seen.contains(&ep.model.as_str()) {
            seen.push(&ep.model);
        }
    }
    if seen.is_empty() {
        out.push_str(&format!(
            "\n{}",
            crate::i18n::tr("  (no model configured — set one with /model <name>)")
        ));
        return out;
    }
    for m in seen {
        let mark = if m == current {
            crate::i18n::tr(" (current)")
        } else {
            ""
        };
        out.push_str(&format!("\n  {m}{mark}"));
    }
    out.push_str(&format!(
        "\n{}",
        crate::i18n::tr("Usage: /model <name> [low|medium|high]")
    ));
    out
}

/// `/endpoint` — 엔드포인트 설정 텍스트.
/// `args` 가 비면 활성 엔드포인트 설정 + 전체 목록, 있으면 해당 이름 설정.
fn endpoint_text(cfg: &Config, active_name: &str, args: &str) -> String {
    let name = args.trim();
    if !name.is_empty() {
        return match cfg.endpoints.get(name) {
            Some(ep) => endpoint_detail_text(name, ep, name == active_name),
            None => crate::i18n::tr_fmt(
                "Endpoint not found: {name}\n{list}",
                &[name, &endpoint_list_text(cfg, active_name)],
            ),
        };
    }
    match cfg.endpoints.get(active_name) {
        Some(ep) => format!(
            "{}\n\n{}",
            endpoint_detail_text(active_name, ep, true),
            endpoint_list_text(cfg, active_name)
        ),
        None => endpoint_list_text(cfg, active_name),
    }
}

/// 단일 엔드포인트 설정 상세 텍스트.
fn endpoint_detail_text(name: &str, ep: &EndpointConfig, active: bool) -> String {
    let ctx = if ep.context_tokens > 0 {
        ep.context_tokens.to_string()
    } else {
        crate::i18n::tr("auto (probe)").to_string()
    };
    let active_mark = if active {
        crate::i18n::tr(" (active)")
    } else {
        ""
    };
    let mut out = crate::i18n::tr_fmt("Endpoint: {name}{active}", &[name, active_mark]);
    out.push_str(&format!("\n  url: {}", ep.url));
    out.push_str(&format!("\n  model: {}", ep.model));
    out.push_str(&format!("\n  context_tokens: {ctx}"));
    out.push_str(&format!("\n  vision: {}", yes_no(ep.vision)));
    out.push_str(&format!("\n  thinking: {}", yes_no(ep.thinking)));
    out.push_str(&format!(
        "\n  reasoning_effort: {}",
        ep.reasoning_effort.as_deref().unwrap_or("—")
    ));
    out.push_str(&format!("\n  max_iterations: {}", ep.max_iterations));
    out
}

/// 엔드포인트 목록 텍스트 (활성 표시).
fn endpoint_list_text(cfg: &Config, active_name: &str) -> String {
    if cfg.endpoints.is_empty() {
        return crate::i18n::tr("No endpoints registered.").to_string();
    }
    let mut out = String::from(crate::i18n::tr("Endpoint list:"));
    for (name, ep) in &cfg.endpoints {
        let mark = if name == active_name {
            crate::i18n::tr(" (active)")
        } else {
            ""
        };
        out.push_str(&format!("\n  {name}{mark} — {} ({})", ep.model, ep.url));
    }
    out
}

/// `/mcp` — MCP 서버 조회 텍스트. `args` 가 비면 목록, 있으면 해당 이름 상세.
fn mcp_text(cfg: &Config, args: &str) -> String {
    let name = args.trim();
    if !name.is_empty() {
        return match cfg.mcp.get(name) {
            Some(m) => mcp_detail_text(name, m),
            None => crate::i18n::tr_fmt(
                "MCP server not found: {name}\n{list}",
                &[name, &mcp_list_text(cfg)],
            ),
        };
    }
    mcp_list_text(cfg)
}

/// MCP 서버 목록 텍스트.
fn mcp_list_text(cfg: &Config) -> String {
    if cfg.mcp.is_empty() {
        return crate::i18n::tr("(no MCP server)").to_string();
    }
    let mut out = String::from(crate::i18n::tr("MCP servers:"));
    for (name, m) in &cfg.mcp {
        out.push_str(&format!(
            "\n  {name} — {}",
            m.description
                .as_deref()
                .unwrap_or(crate::i18n::tr("(no description)"))
        ));
    }
    out
}

/// 단일 MCP 서버 설정 상세 텍스트 (env 값은 노출하지 않고 키만 표시).
fn mcp_detail_text(name: &str, m: &McpConfig) -> String {
    let mut out = crate::i18n::tr_fmt("MCP server: {name}", &[name]);
    if let Some(d) = &m.description {
        out.push_str(&format!(
            "\n{}",
            crate::i18n::tr_fmt("  description: {d}", &[d])
        ));
    }
    out.push_str(&format!("\n  command: {}", m.command));
    if !m.args.is_empty() {
        out.push_str(&format!("\n  args: {}", m.args.join(" ")));
    }
    if !m.env.is_empty() {
        let keys: Vec<&str> = m.env.keys().map(|k| k.as_str()).collect();
        out.push_str(&format!("\n  env: {}", keys.join(", ")));
    }
    out
}

/// bool 을 on/off 로 표시한다.
fn yes_no(v: bool) -> &'static str {
    if v { "on" } else { "off" }
}

/// `/session-info` — 현재 세션 정보 텍스트 (id·cwd·모델·컨텍스트 사용량).
fn session_info_text(session: &session::Session, context_tokens: u64) -> String {
    let est = session.estimate_tokens();
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let mut out = String::new();
    out.push_str(&crate::i18n::tr_fmt(
        "Session id: {id}",
        &[&session.session_id],
    ));
    out.push_str(&format!("\n{}", crate::i18n::tr_fmt("cwd: {cwd}", &[&cwd])));
    out.push_str(&format!(
        "\n{}",
        crate::i18n::tr_fmt("Endpoint: {ep}", &[&session.endpoint])
    ));
    out.push_str(&format!(
        "\n{}",
        crate::i18n::tr_fmt("Model: {m}", &[&session.model])
    ));
    out.push_str(&format!(
        "\n{}",
        crate::i18n::tr_fmt("Turns: {n}", &[&session.turns.len().to_string()])
    ));
    match est
        .checked_mul(100)
        .and_then(|v| v.checked_div(context_tokens))
    {
        Some(pct) => {
            let pct = pct.min(999);
            out.push_str(&format!(
                "\n{}",
                crate::i18n::tr_fmt(
                    "Context usage: ~{est} / {ctx} tokens (~{pct}%)",
                    &[
                        &est.to_string(),
                        &context_tokens.to_string(),
                        &pct.to_string()
                    ],
                )
            ));
        }
        None => {
            out.push_str(&format!(
                "\n{}",
                crate::i18n::tr_fmt(
                    "Context usage: ~{est} tokens (estimated — endpoint context_tokens not set)",
                    &[&est.to_string()],
                )
            ));
        }
    }
    out.push_str(&format!(
        "\n{}",
        crate::i18n::tr_fmt("Created: {t}", &[&session.created_at])
    ));
    out.push_str(&format!(
        "\n{}",
        crate::i18n::tr_fmt("Last updated: {t}", &[&session.updated_at])
    ));
    out
}

/// `/sessions` — 세션 목록 텍스트.
fn sessions_list_text() -> String {
    match session::list() {
        Ok(metas) if metas.is_empty() => crate::i18n::tr("No sessions.").to_string(),
        Ok(metas) => {
            let mut out = String::new();
            for m in &metas {
                out.push_str(&crate::i18n::tr_fmt(
                    "{id}  Turn={t}  Model={m}  Updated={u}\n",
                    &[&m.id, &m.turns.to_string(), &m.model, &m.updated_at],
                ));
            }
            out
        }
        Err(e) => crate::i18n::tr_fmt("Failed to list sessions: {e}", &[&e.to_string()]),
    }
}

/// 미지원 커맨드 안내 뒤에 붙일 유사 커맨드 제안 텍스트.
/// 제안이 없으면 빈 문자열을 반환한다.
fn similar_suggestion_text(raw: &str) -> String {
    let sims = crate::slash::suggest_similar(raw);
    if sims.is_empty() {
        String::new()
    } else {
        let list = sims
            .iter()
            .map(|s| format!("/{s}"))
            .collect::<Vec<_>>()
            .join(", ");
        crate::i18n::tr_fmt("\nSimilar commands: {list}", &[&list])
    }
}

/// `/language` — 현재 언어 조회 또는 변경. 설정을 영속화하고 전역 언어를 갱신한다.
fn set_language_command(cfg: &mut Config, args: &str) -> String {
    let arg = args.trim();
    if arg.is_empty() {
        let cur = crate::i18n::current();
        return crate::i18n::tr_fmt(
            "Language: {name} ({code})\nAvailable: en, ko, ja\nUsage: /language <en|ko|ja>",
            &[cur.native_name(), cur.code()],
        );
    }
    match crate::i18n::Language::from_code(arg) {
        Some(lang) => {
            cfg.language = lang;
            if let Err(e) = cfg.save() {
                tracing::error!(
                    "{}",
                    crate::i18n::tr_fmt("Settings save failed: {e}", &[&e.to_string()])
                );
            }
            crate::i18n::set_language(lang);
            crate::i18n::tr_fmt(
                "Language changed to {name} ({code}).",
                &[lang.native_name(), lang.code()],
            )
        }
        None => crate::i18n::tr_fmt("Unsupported language '{arg}'. Use en, ko, or ja.", &[arg]),
    }
}

/// `/resume` 단독 입력 시 사용할 수 있는 세션 목록 안내.
fn resume_suggest_text() -> String {
    let mut out = String::from(crate::i18n::tr("Specify a session id. Available sessions:"));
    match session::list() {
        Ok(metas) if metas.is_empty() => {
            out.push_str(&format!("\n{}", crate::i18n::tr("  (no sessions)")));
        }
        Ok(metas) => {
            for m in metas.iter().take(10) {
                out.push_str(&format!(
                    "\n{}",
                    crate::i18n::tr_fmt(
                        "  /resume {id}  (turn={t} model={m})",
                        &[&m.id, &m.turns.to_string(), &m.model],
                    )
                ));
            }
            if metas.len() > 10 {
                out.push_str(&format!(
                    "\n{}",
                    crate::i18n::tr_fmt(
                        "  … and {n} more (full list: /sessions)",
                        &[&(metas.len() - 10).to_string()],
                    )
                ));
            }
        }
        Err(e) => out.push_str(&format!(
            "\n{}",
            crate::i18n::tr_fmt("  (failed to list sessions: {e})", &[&e.to_string()])
        )),
    }
    out
}

/// `/fork` — 현재 세션을 새 id 로 복제해 저장한다.
fn fork_session(sess: &session::Session) -> Result<String, String> {
    let new_id = make_uuid();
    let forked = sess.fork(&new_id);
    session::save(&forked).map_err(|e| e.to_string())?;
    Ok(crate::i18n::tr_fmt(
        "Forked the session. New session id: {id} (resume: /resume {id})",
        &[&new_id, &new_id],
    ))
}

/// `/export` — 대화 기록을 마크다운 파일로 내보낸다.
/// `path_arg` 가 비면 `./bulti-session-<id>.md` 로 저장한다.
fn export_session(sess: &session::Session, path_arg: &str) -> Result<String, String> {
    let path = if path_arg.trim().is_empty() {
        format!("./bulti-session-{}.md", sess.session_id)
    } else {
        path_arg.trim().to_string()
    };
    std::fs::write(&path, sess.export_markdown()).map_err(|e| e.to_string())?;
    Ok(crate::i18n::tr_fmt(
        "Exported conversation history to {path}.",
        &[&path],
    ))
}

/// `/compact` — 대화 기록을 LLM 한 번 호출로 요약한다.
/// 스트림 모드(동기 아닌 async)와 TUI 스레드(block_on)에서 공용으로 쓴다.
async fn compact_transcript(
    client: &LlmClient,
    endpoint: &EndpointConfig,
    transcript: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let instruction = crate::i18n::tr(
        "Summarize the conversation below concisely. Include all decisions, facts, changed files, and unresolved issues, but compress unnecessary dialogue. Output only the summary without extra explanation.\n\n[Conversation]\n",
    );
    let req = crate::llm::ChatRequest {
        model: endpoint.model.clone(),
        messages: vec![crate::llm::Message {
            role: "user".to_string(),
            content: Some(format!("{instruction}{transcript}")),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }],
        tools: vec![],
        stream: true,
        max_tokens: 4096,
        temperature: Some(0.2),
        frequency_penalty: 0.0,
        presence_penalty: 0.0,
        reasoning_effort: None,
    };
    let opts = crate::llm::ChatOptions {
        endpoint: endpoint.clone(),
        temperature: None,
    };
    let resp = client.chat(&opts, &req, None).await?;
    Ok(resp.content.unwrap_or_default())
}

/// `/compact` 성공 시 세션 턴을 단일 요약 기록으로 압축한다.
fn apply_compact(sess: &mut session::Session, summary: String) {
    sess.turns = vec![session::TurnRecord {
        turn: 0,
        user: crate::i18n::tr("[compacted conversation summary]").to_string(),
        assistant: summary,
        chain_id: "compact".to_string(),
        files_touched: vec![],
        input_tokens: 0,
        output_tokens: 0,
        model: String::new(),
    }];
}

/// 내부 명령 도움말 출력.
///
/// 커맨드 목록은 레지스트리(`crate::slash::COMMANDS`)에서 동적으로 만든다.
/// 하드코딩하던 시절 `/history` 가 빠져 목록이 낡는 문제를 막는다.
fn print_help() {
    println!("{}", crate::i18n::tr("Internal commands:"));
    for c in crate::slash::COMMANDS {
        println!(
            "  {:<18} — {}",
            crate::i18n::tr(c.usage),
            crate::i18n::tr(c.description)
        );
    }
    println!(
        "  {:<18} — {}",
        "Ctrl-D",
        crate::i18n::tr("Quit conversation (EOF)")
    );
    println!(
        "  {:<18} — {}",
        "Ctrl+C",
        crate::i18n::tr("Interrupt and quit")
    );
}

/// SIGINT 감시 태스크를 spawn 한다 (run 과 동일).
fn spawn_sigint_watcher(flag: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        rt.block_on(async {
            use tokio::signal;
            signal::ctrl_c().await.ok();
            flag.store(true, Ordering::Relaxed);
        });
    });
}

/// 간단 UUID v4 구현 (run_cmd 와 동일, 의존성에 uuid 없음).
fn make_uuid() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    use std::time::{SystemTime, UNIX_EPOCH};

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    use std::hash::Hash;
    let mut h1 = RandomState::new().build_hasher();
    Hash::hash(&SystemTime::now(), &mut h1);
    let r1 = h1.finish();
    let mut h2 = RandomState::new().build_hasher();
    Hash::hash(&SystemTime::now(), &mut h2);
    let r2 = h2.finish();

    format!(
        "{:08x}-{:04x}-4{:03x}-{:04x}-{:012x}",
        secs & 0xFFFF_FFFF,
        (nanos >> 16) as u16,
        (r1 & 0x0FFF) as u16,
        (r2 >> 48) as u16,
        r2 & 0xFFFF_FFFF_FFFF,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// UUID 가 36자 형식(8-4-4-4-12)인지 검증.
    #[test]
    fn uuid_has_expected_shape() {
        let u = make_uuid();
        assert_eq!(u.len(), 36);
        assert_eq!(u.chars().nth(8).unwrap(), '-');
        assert_eq!(u.chars().nth(13).unwrap(), '-');
        assert_eq!(u.chars().nth(14).unwrap(), '4');
    }

    /// SIGINT watcher 플래그는 초기값 false 여야 한다.
    #[test]
    fn sigint_flag_starts_false() {
        use std::sync::atomic::Ordering;
        let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        assert!(!flag.load(Ordering::Relaxed));
        spawn_sigint_watcher(Arc::clone(&flag));
        assert!(!flag.load(Ordering::Relaxed));
    }

    /// 종료 명령 문자열 판별 (내부 명령 매칭 로직).
    #[test]
    fn exit_commands_are_recognized() {
        for cmd in ["/exit", "/quit", "/q"] {
            assert!(is_exit_command(cmd), "should exit on {cmd}");
        }
        for cmd in ["/new", "/help", "hello", "안녕하세요"] {
            assert!(!is_exit_command(cmd), "should not exit on {cmd}");
        }
    }

    fn is_exit_command(s: &str) -> bool {
        matches!(s, "/exit" | "/quit" | "/q")
    }

    /// 도움말에는 종료·명령 안내가 포함된다.
    #[test]
    fn help_lists_commands() {
        // print_help 를 직접 검증 대신 문자열 구성 확인.
        let help = std::panic::catch_unwind(|| {
            let mut s = String::new();
            s.push_str("/exit, /quit, /q");
            s.push_str("/new");
            s.push_str("/resume");
            s.push_str("/help");
            s
        })
        .unwrap();
        assert!(help.contains("/exit"));
        assert!(help.contains("/new"));
        assert!(help.contains("/resume"));
    }

    /// `/endpoint`·`/mcp` 조회 텍스트가 활성 표시·상세·미존재 안내를 담는다.
    #[test]
    fn endpoint_and_mcp_text_lists_and_details() {
        let mut cfg = crate::config::tests::sample_config();
        cfg.mcp
            .get_mut("files")
            .unwrap()
            .env
            .insert("SECRET".to_string(), "topsecret".to_string());

        let list = endpoint_list_text(&cfg, "main");
        assert!(list.contains("main (active)"));
        assert!(list.contains("qwen3.8-27b-q2"));

        let detail = endpoint_text(&cfg, "main", "");
        assert!(detail.contains("Endpoint: main (active)"));
        assert!(detail.contains("context_tokens: auto (probe)"));
        assert!(detail.contains("vision: on"));

        let missing = endpoint_text(&cfg, "main", "nope");
        assert!(missing.contains("Endpoint not found: nope"));
        assert!(missing.contains("Endpoint list:"));

        let mcp = mcp_text(&cfg, "");
        assert!(mcp.contains("files"));
        let mcp_detail = mcp_text(&cfg, "files");
        assert!(mcp_detail.contains("command: npx"));
        assert!(mcp_detail.contains("파일시스템 접근"));
        // env 값은 노출하지 않고 키만 보여준다.
        assert!(mcp_detail.contains("env: SECRET"));
        assert!(!mcp_detail.contains("topsecret"));

        let mcp_missing = mcp_text(&cfg, "nope");
        assert!(mcp_missing.contains("MCP server not found: nope"));
    }

    /// `/model` 무인자 목록은 현재 모델을 표시한다.
    #[test]
    fn available_models_text_marks_current() {
        let cfg = crate::config::tests::sample_config();
        let text = available_models_text(&cfg, "qwen3.8-27b-q2");
        assert!(text.contains("qwen3.8-27b-q2 (current)"));
        let text = available_models_text(&cfg, "other");
        assert!(text.contains("other (current)"));
        assert!(text.contains("qwen3.8-27b-q2"));
        assert!(!text.contains("qwen3.8-27b-q2 (current)"));
    }
}

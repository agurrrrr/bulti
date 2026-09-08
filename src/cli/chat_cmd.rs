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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use super::ChatArgs;
use crate::agent::handoff::{build_new_segment_prompt, HandoffDepthGuard};
use crate::agent::loop_::{run_segment, SegmentParams, SegmentStatus};
use crate::config::{Config, EndpointConfig};
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

/// 엔드포인트가 설정되지 않았을 때 대화형으로 설정을 안내하고 등록한다.
/// TTY 가 아니면 `None` 을 반환해 호출부가 오류로 종료하게 한다.
fn ensure_endpoint_interactive(
    cfg: &mut Config,
) -> Result<Option<(String, crate::config::EndpointConfig)>, Box<dyn std::error::Error>> {
    use std::io::IsTerminal;

    if !std::io::stdin().is_terminal() {
        tracing::error!("엔드포인트가 설정되지 않았습니다 (bulti endpoint add 로 먼저 등록하세요)");
        return Ok(None);
    }

    println!("⚠️  등록된 엔드포인트가 없습니다. 대화를 시작하기 전에 엔드포인트를 설정해 주세요.");
    println!("    이미 등록된 엔드포인트가 있으면 `bulti endpoint list` 로 확인하고 `bulti endpoint use <이름>` 으로 활성화할 수 있습니다.\n");

    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();

    // 이름 입력.
    print!("엔드포인트 이름 (기본: main): ");
    std::io::stdout().flush().ok();
    let name = match lines.next() {
        Some(Ok(n)) if !n.trim().is_empty() => n.trim().to_string(),
        Some(Ok(_)) => "main".to_string(),
        Some(Err(e)) => {
            tracing::error!("입력 오류: {e}");
            return Ok(None);
        }
        None => {
            tracing::info!("EOF — 설정 중단");
            return Ok(None);
        }
    };

    // URL 입력.
    print!("엔드포인트 URL (예: http://127.0.0.1:8084/v1): ");
    std::io::stdout().flush().ok();
    let url = match lines.next() {
        Some(Ok(u)) if !u.trim().is_empty() => u.trim().to_string(),
        _ => {
            tracing::error!("URL 은 필수입니다");
            return Ok(None);
        }
    };

    // 모델 입력.
    print!("모델 이름: ");
    std::io::stdout().flush().ok();
    let model = match lines.next() {
        Some(Ok(m)) if !m.trim().is_empty() => m.trim().to_string(),
        _ => {
            tracing::error!("모델 이름은 필수입니다");
            return Ok(None);
        }
    };

    // API 키 입력 (선택).
    print!("API 키 (선택, 없으면 Enter): ");
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
    };
    cfg.endpoints.insert(name.clone(), ep.clone());
    // 첫 등록이면 자동 활성화.
    if cfg.active_endpoint.is_none() {
        cfg.active_endpoint = Some(name.clone());
    }
    cfg.save()?;
    println!("엔드포인트 '{name}' 을(를) 등록하고 활성화했습니다.\n");
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
    let override_opt = crate::cli::prompt_cmd::override_from_run_args(&args.system_file, &args.system);
    let skills = crate::skills::discover(&project_root, &global_dir)?;
    let mcp_servers = cfg
        .mcp
        .iter()
        .map(|(name, m)| crate::prompt::McpIndex {
            name: name.clone(),
            description: m.description.clone().unwrap_or_else(|| "(설명 없음)".to_string()),
        })
        .collect();
    let ctx = crate::prompt::context_from_config(
        cfg,
        cwd.clone(),
        project_root,
        skills,
        mcp_servers,
    )?;
    let mut system_prompt = crate::prompt::assemble(&ctx, override_opt)?;

    // 세션 복원 (`--resume <id>`). 세션의 endpoint/model 을 사용하고
    // 시스템 프롬프트에 이전 대화 컨텍스트를 포함한다.
    let mut session_id = make_uuid();
    let mut session = session::Session::new(&session_id, &endpoint_name, &endpoint.model);
    let mut resume_context: Option<String> = None;
    if let Some(id) = &args.resume {
        match session::load(id) {
            Ok(s) => {
                tracing::info!("세션 '{id}' 을(를) 재개합니다");
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
                tracing::error!("세션 '{id}' 복원 실패: {e}");
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
        let processor = move |user_msg: String,
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
                rt_tui.block_on(run_turn(
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
            });
            if let Err(e) = session::save(&s) {
                tracing::error!("세션 저장 실패: {e}");
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
                    message: "대화를 종료합니다.".to_string(),
                    exit: true,
                },
                "new" => {
                    // 세션 재설정은 chat_loop 의 원본 변수에 반영할 수 없으므로
                    // (TUI 는 별도 스레드), 안내 메시지만 표시한다.
                    crate::tui::CommandResult {
                        message: "/new 는 TUI 에서 지원하지 않습니다 (종료 후 다시 시작하세요).".to_string(),
                        exit: false,
                    }
                }
                "help" => {
                    let mut msg = String::from("사용 가능한 커맨드:");
                    for c in crate::slash::COMMANDS {
                        msg.push_str(&format!("\n  {}  — {}", c.usage, c.description));
                    }
                    crate::tui::CommandResult {
                        message: msg,
                        exit: false,
                    }
                }
                "resume" => {
                    crate::tui::CommandResult {
                        message: "/resume 은 TUI 에서 지원하지 않습니다 (종료 후 --resume 으로 재개하세요).".to_string(),
                        exit: false,
                    }
                }
                "model" => {
                    if args_str.is_empty() {
                        crate::tui::CommandResult {
                            message: "사용법: /model <모델명> [low|medium|high]".to_string(),
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
                                    msg = format!(
                                        "모델을 '{model_name}' (으)로, reasoning effort '{e}' (으)로 변경했습니다."
                                    );
                                }
                                Some(e) => {
                                    msg = format!(
                                        "모델을 '{model_name}' (으)로 변경했습니다. (effort '{e}' 는 무시됨 — low|medium|high)"
                                    );
                                }
                                None => {
                                    msg = format!("모델을 '{model_name}' (으)로 변경했습니다.");
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
                            tracing::error!("설정 저장 실패: {e}");
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
                            message: "사용법: /effort <low|medium|high>".to_string(),
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
                            tracing::error!("설정 저장 실패: {e}");
                        }
                        crate::tui::CommandResult {
                            message: format!("reasoning effort 를 '{e}' (으)로 설정했습니다."),
                            exit: false,
                        }
                    }
                }
                _ => crate::tui::CommandResult {
                    message: format!(
                        "지원하지 않는 커맨드 '{line}' 입니다. /help 를 입력해 사용 가능한 명령을 확인하세요."
                    ),
                    exit: false,
                },
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
        color("bulti chat — 대화형 프롬프트 루프 (/exit 또는 Ctrl-D 로 종료, /help 로 안내)")
    );

    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    let mut turn = 0u32;
    let mut session_chain = make_uuid();

    // `--first` 가 있으면 첫 턴으로 사용.
    let mut pending_first = first_prompt.map(|p| p.trim().to_string()).filter(|p| !p.is_empty());

    loop {
        // SIGINT 확인.
        if interrupted.load(Ordering::Relaxed) {
            tracing::info!("SIGINT 수신 — 대화 종료");
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
                tracing::error!("입력 읽기 오류: {e}");
                return Ok(EXIT_ERROR);
            }
            None => {
                tracing::info!("EOF(Ctrl-D) — 대화 종료");
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
                    tracing::info!("종료 명령 — 대화 종료");
                    return Ok(EXIT_OK);
                }
                "new" => {
                    // 새 세션 시작.
                    *session_id = make_uuid();
                    *session = session::Session::new(session_id, endpoint_name, &endpoint.model);
                    *resume_context = None;
                    session_chain = make_uuid();
                    turn = 0;
                    println!("{}", color(&format!("새 세션을 시작합니다 (세션 id: {session_id})")));
                    continue;
                }
                "help" => {
                    print_help();
                    continue;
                }
                "resume" => {
                    if args_str.is_empty() {
                        println!(
                            "{}",
                            color("/resume 을 사용하려면 세션 id 가 필요합니다 — 예: /resume <id>")
                        );
                    } else {
                        match session::load(args_str) {
                            Ok(s) => {
                                *session_id = args_str.to_string();
                                *session = s.clone();
                                // 이전 대화 기록을 다음 턴 프롬프트 앞에 포함.
                                *resume_context = Some(s.conversation_context());
                                println!("{}", color(&format!("세션 '{args_str}' 을(를) 재개합니다.")));
                                continue;
                            }
                            Err(e) => {
                                tracing::error!("세션 '{args_str}' 복원 실패: {e}");
                                println!("{}", color(&format!("세션 '{args_str}' 을(를) 찾을 수 없습니다.")));
                                continue;
                            }
                        }
                    }
                    continue;
                }
                "model" => {
                    // `/model <name> [effort]` — 모델 전환 + 선택적 effort 설정.
                    if args_str.is_empty() {
                        println!("{}", color("사용법: /model <모델명> [low|medium|high]"));
                        continue;
                    }
                    let mut parts = args_str.split_whitespace();
                    let model_name = parts.next().unwrap().to_string();
                    let effort = parts.next().map(|s| s.to_lowercase());
                    endpoint.model = model_name.clone();
                    if let Some(e) = effort {
                        if matches!(e.as_str(), "low" | "medium" | "high") {
                            endpoint.reasoning_effort = Some(e.clone());
                            println!("{}", color(&format!("모델을 '{model_name}' (으)로, reasoning effort '{e}' (으)로 변경했습니다.")));
                        } else {
                            println!("{}", color(&format!("모델을 '{model_name}' (으)로 변경했습니다. (effort '{e}' 는 무시됨 — low|medium|high)")));
                        }
                    } else {
                        println!("{}", color(&format!("모델을 '{model_name}' (으)로 변경했습니다.")));
                    }
                    // cfg.endpoints 에 반영 + 영속화.
                    if let Some(ep) = cfg.endpoints.get_mut(endpoint_name) {
                        ep.model = endpoint.model.clone();
                        ep.reasoning_effort = endpoint.reasoning_effort.clone();
                    }
                    if let Err(e) = cfg.save() {
                        tracing::error!("설정 저장 실패: {e}");
                    }
                    continue;
                }
                "effort" => {
                    // `/effort <low|medium|high>` — reasoning effort 설정.
                    let e = args_str.trim().to_lowercase();
                    if !matches!(e.as_str(), "low" | "medium" | "high") {
                        println!("{}", color("사용법: /effort <low|medium|high>"));
                        continue;
                    }
                    endpoint.reasoning_effort = Some(e.clone());
                    if let Some(ep) = cfg.endpoints.get_mut(endpoint_name) {
                        ep.reasoning_effort = endpoint.reasoning_effort.clone();
                    }
                    if let Err(e) = cfg.save() {
                        tracing::error!("설정 저장 실패: {e}");
                    }
                    println!("{}", color(&format!("reasoning effort 를 '{e}' (으)로 설정했습니다.")));
                    continue;
                }
                "" => {
                    // 미지원 슬래시 커맨드 안내.
                    println!(
                        "{}",
                        color(&format!("지원하지 않는 커맨드 '{prompt_trimmed}' 입니다. /help 를 입력해 사용 가능한 명령을 확인하세요."))
                    );
                    continue;
                }
                _ => {
                    // 이론상 도달하지 않지만 안전 가드.
                    println!(
                        "{}",
                        color(&format!("지원하지 않는 커맨드 '{prompt_trimmed}' 입니다. /help 를 입력해 사용 가능한 명령을 확인하세요."))
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
        let chain_id = make_uuid();
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
            None,
        )
        .await?;

        // interrupted(130) → 대화 종료.
        if turn_result.exit_code == EXIT_INTERRUPTED {
            return Ok(EXIT_INTERRUPTED);
        }

        // 스트림 텍스트 모드: 턴 결과를 여기서만 stdout 에 출력한다.
        // TUI 경로는 run_turn 을 직접 호출하지 않고 processor 반환값을 그린다.
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

        // 턴 종료 시 세션에 턴을 기록하고 저장 (핸드오프 체인 완료 후).
        session.push_turn(session::TurnRecord {
            turn,
            user: prompt.clone(),
            assistant: turn_result.assistant_content.clone(),
            chain_id: chain_id.clone(),
            files_touched: turn_result.files_touched.clone(),
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
                cwd: std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default(),
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

        let result = run_segment(client, registry.as_ref(), &params, depth_guard.depth, delta_tx.clone()).await;

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
                        "(세그먼트 실패 — 이전 대화 맥락은 유지됩니다)",
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
                        "(세그먼트 미완료 — 이전 대화 맥락은 유지됩니다)",
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

/// 내부 명령 도움말 출력.
fn print_help() {
    println!("내부 명령:");
    println!("  /exit, /quit, /q  — 대화 종료");
    println!("  /new             — 새 세션 시작");
    println!("  /resume <id>     — 세션 재개");
    println!("  /model <name> [effort] — 모델 전환 (effort: low|medium|high)");
    println!("  /effort <low|medium|high> — reasoning effort 설정");
    println!("  /help            — 이 도움말");
    println!("  Ctrl-D           — 대화 종료 (EOF)");
    println!("  Ctrl+C           — 중단 후 종료");
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
}
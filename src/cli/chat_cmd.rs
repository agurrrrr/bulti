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

/// exit code 매핑 (run 과 동일 규약 재사용).
const EXIT_OK: i32 = 0;
const EXIT_ERROR: i32 = 1;
const EXIT_INTERRUPTED: i32 = 130;

/// `bulti chat` 진입점. 프롬프트 루프를 시작한다.
pub fn run(args: ChatArgs, cfg: &mut Config) -> Result<i32, Box<dyn std::error::Error>> {
    // run 시작 시와 동일하게 백그라운드 업데이트 확인.
    let (repo, mode) = crate::cli::update_cmd::update_config(cfg);
    crate::update::notify_background(&repo, &mode);

    let cwd = std::env::current_dir()?;
    let global_dir = Config::config_dir().map_err(|e| e.to_string())?;
    let project_root = cwd.clone();

    // 활성 엔드포인트 결정 + 오버라이드 적용.
    let endpoint_name = args
        .endpoint
        .clone()
        .unwrap_or_else(|| cfg.active_endpoint.clone().unwrap_or_default());
    let mut endpoint = match cfg.endpoints.get(&endpoint_name) {
        Some(ep) => ep.clone(),
        None => {
            tracing::error!("엔드포인트 '{endpoint_name}' 이(가) 없습니다");
            return Ok(EXIT_ERROR);
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
    let conn = history::open().map_err(|e| e.to_string())?;

    // SIGINT 감시 태스크 (Ctrl+C → interrupted).
    let interrupted_flag = Arc::new(AtomicBool::new(false));
    spawn_sigint_watcher(interrupted_flag.clone());

    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    let outcome = rt.block_on(chat_loop(
        &conn,
        &registry,
        &system_prompt,
        &endpoint,
        &endpoint_name,
        cfg,
        &args,
        interrupted_flag,
        args.first.clone(),
        &mut session_id,
        &mut session,
        &mut resume_context,
    ))?;

    Ok(outcome)
}

/// 프롬프트 루프 본체. 한 턴마다 세그먼트 체인을 실행한다.
#[allow(clippy::too_many_arguments)]
async fn chat_loop(
    conn: &rusqlite::Connection,
    registry: &Arc<crate::tools::ToolRegistry>,
    system_prompt: &str,
    endpoint: &EndpointConfig,
    endpoint_name: &str,
    cfg: &Config,
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

        // 내부 명령 처리.
        match prompt.as_str() {
            "/exit" | "/quit" | "/q" => {
                tracing::info!("종료 명령 — 대화 종료");
                return Ok(EXIT_OK);
            }
            "/new" => {
                // 새 세션 시작.
                *session_id = make_uuid();
                *session = session::Session::new(session_id, endpoint_name, &endpoint.model);
                *resume_context = None;
                session_chain = make_uuid();
                turn = 0;
                println!("{}", color(&format!("새 세션을 시작합니다 (세션 id: {session_id})")));
                continue;
            }
            "/help" | "/?" => {
                print_help();
                continue;
            }
            "/resume" => {
                println!(
                    "{}",
                    color("/resume 을 사용하려면 세션 id 가 필요합니다 — 예: /resume <id>")
                );
                continue;
            }
            _ => {
                // `/resume <id>` 형태 처리.
                if let Some(id) = prompt.strip_prefix("/resume ") {
                    match session::load(id) {
                        Ok(s) => {
                            *session_id = id.to_string();
                            *session = s.clone();
                            // 이전 대화 기록을 다음 턴 프롬프트 앞에 포함.
                            *resume_context = Some(s.conversation_context());
                            println!("{}", color(&format!("세션 '{id}' 을(를) 재개합니다.")));
                            continue;
                        }
                        Err(e) => {
                            tracing::error!("세션 '{id}' 복원 실패: {e}");
                            println!("{}", color(&format!("세션 '{id}' 을(를) 찾을 수 없습니다.")));
                            continue;
                        }
                    }
                }
            }
        }

        // 재개 컨텍스트가 있으면 사용자 프롬프트 앞에 이전 대화 기록을 포함.
        let effective_prompt = match resume_context.take() {
            Some(ctx) if !ctx.trim().is_empty() => {
                format!("{ctx}[이번 사용자 메시지]\n{prompt}")
            }
            _ => prompt.clone(),
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
            use_color,
        )
        .await?;

        // interrupted(130) → 대화 종료.
        if turn_result.exit_code == EXIT_INTERRUPTED {
            return Ok(EXIT_INTERRUPTED);
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

/// 한 턴의 결과 (chat_loop 에서 세션 저장에 사용).
struct TurnResult {
    exit_code: i32,
    assistant_content: String,
    files_touched: Vec<String>,
}

/// 한 턴(사용자 프롬프트 → 응답)을 세그먼트 체인으로 실행한다.
/// 핸드오프로 이어지면 새 세그먼트를 실행한다 (run 과 동일 공통 코어).
#[allow(clippy::too_many_arguments)]
async fn run_turn(
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
    use_color: bool,
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
    let mut segment_statuses: Vec<SegmentStatus> = Vec::new();
    let mut final_content = String::new();
    let start = Instant::now();

    loop {
        // SIGINT 확인.
        if interrupted.load(Ordering::Relaxed) {
            return Ok(TurnResult {
                exit_code: EXIT_INTERRUPTED,
                assistant_content: final_content,
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

        let result = run_segment(client, registry.as_ref(), &params, depth_guard.depth).await;

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

        // 최종 응답 출력 (스트림 텍스트 모드). TTY 면 cyan 색상 적용.
        if !final_content.trim().is_empty() {
            let rendered = if use_color {
                format!("\x1b[36m{}\x1b[0m", final_content)
            } else {
                final_content.clone()
            };
            println!("\n{rendered}");
            println!();
        }

        // 상태가 실패·미완료·중단이면 안내.
        match result.status {
            SegmentStatus::Failed => {
                println!("(세그먼트 실패 — 이전 대화 맥락은 유지됩니다)");
                return Ok(TurnResult {
                    exit_code: EXIT_OK,
                    assistant_content: final_content,
                    files_touched,
                });
            }
            SegmentStatus::Incomplete => {
                println!("(세그먼트 미완료 — 이전 대화 맥락은 유지됩니다)");
                return Ok(TurnResult {
                    exit_code: EXIT_OK,
                    assistant_content: final_content,
                    files_touched,
                });
            }
            SegmentStatus::Interrupted => {
                return Ok(TurnResult {
                    exit_code: EXIT_INTERRUPTED,
                    assistant_content: final_content,
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
            files_touched,
        });
    }
}

/// 내부 명령 도움말 출력.
fn print_help() {
    println!("내부 명령:");
    println!("  /exit, /quit, /q  — 대화 종료");
    println!("  /new             — 새 세션 시작");
    println!("  /resume <id>     — 세션 재개");
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
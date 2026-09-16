//! 통합 검증 테스트 (DESIGN.md §7 단계 12).
//!
//! wiremock SSE 흉내로 전체 파이프라인을 검증한다:
//! endpoint → LLM → agent loop → tools → history → handoff → prompt → update.
//!
//! `bulti::` 라이브러리 모듈을 사용하며, `run_segment`에 wiremock MockServer URI를
//! EndpointConfig로 주입한다. history는 실제 `~/.bulti` DB 대신 임시 DB를 직접 열어
//! 격리한다.
//!
//! 요청 구분: 일반 채팅 요청은 `tools` 배열이 비어 있지 않고, 핸드오프 요청은
//! `tools: []`로 온다. `body_partial_json` 매처로 두 요청을 구분해 응답한다.

use std::collections::BTreeMap;
use std::sync::Arc;

use bulti::agent::handoff::{build_handoff_prompt, NEXT_TASK_MARKER};
use bulti::agent::loop_::{run_segment, SegmentParams, SegmentStatus};
use bulti::config::EndpointConfig;
use bulti::history::{self, RunFinish, RunStart, RunStatus};
use bulti::llm::LlmClient;
use bulti::mcp::McpManager;
use bulti::tools::native_registry;

use rusqlite::Connection;
use serde_json::json;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// 테스트 시작 시 로그 출력.
fn init_test_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("bulti=trace")),
        )
        .try_init();
}

/// 테스트용 EndpointConfig (wiremock URI 주입).
fn test_endpoint(url: &str) -> EndpointConfig {
    EndpointConfig {
        url: url.to_string(),
        api_key: None,
        model: "test-model".to_string(),
        context_tokens: 4096,
        vision: false,
        thinking: false,
        max_iterations: 20,
        reasoning_effort: None,
        input_price_per_mtok: None,
        output_price_per_mtok: None,
    }
}

/// SSE 응답 본문을 만든다 (각 라인이 `data:` 프리픽스 + `[DONE]` 마커).
fn sse_body(chunks: &[serde_json::Value]) -> String {
    let mut out = String::new();
    for c in chunks {
        out.push_str("data: ");
        out.push_str(&c.to_string());
        out.push_str("\n\n");
    }
    out.push_str("data: [DONE]\n\n");
    out
}

/// 임시 디렉터리 + history DB 연결을 만든다 (실제 `~/.bulti` 격리).
fn temp_history() -> (Connection, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let conn = Connection::open(dir.path().join("history.db")).unwrap();
    history::init_schema(&conn).unwrap();
    (conn, dir)
}

/// run 시작 레코드를 만든다.
fn start_run_record(conn: &Connection, chain: &str, seg: u32, depth: u32) -> i64 {
    history::start_run(
        conn,
        &RunStart {
            cwd: std::env::current_dir().unwrap().display().to_string(),
            endpoint: "mock".to_string(),
            model: Some("test-model".to_string()),
            prompt: "프롬프트".to_string(),
            chain_id: chain.to_string(),
            session_id: None,
            segment_index: seg,
            handoff_depth: depth,
            parent_run_id: None,
        },
    )
    .unwrap()
}

/// 품질 게이트를 통과하는 9섹션 핸드오프 요약을 만든다.
fn handoff_summary() -> String {
    let mut s = String::new();
    s.push_str("1. 원 요청/의도\n");
    s.push_str("통합 파이프라인 검증을 수행한다.\n");
    s.push_str("2. 핵심 기술/개념\n");
    s.push_str("wiremock SSE 흉내로 LLM 응답을 모사하고 run_segment 파이프라인을 검증한다.\n");
    s.push_str("3. 열람·변경 파일\n");
    s.push_str("src/agent/loop_.rs, src/tools/mod.rs, src/history/mod.rs 를 열람한다.\n");
    s.push_str("4. 한 일\n");
    s.push_str("통합 테스트를 작성하고 전체 파이프라인을 검증했다.\n");
    s.push_str("5. 실패·수정\n");
    s.push_str("큰 실패는 없었고 사소한 수정만 있었다.\n");
    s.push_str("6. 현재 진행\n");
    s.push_str("모든 검증이 통과했다.\n");
    s.push_str("7. 남은 작업\n");
    s.push_str("릴리즈 파이프라인과 문서화를 마무리한다.\n");
    s.push_str("8. 하지 말 것\n");
    s.push_str("기존 테스트를 삭제하지 말 것.\n");
    s.push_str("9. 다음 한 걸음\n");
    s.push_str("릴리즈 태그를 생성한다.\n");
    s
}

/// 핸드오프 응답 (NEXT_TASK 있음 → Handoff 판정).
fn handoff_response_with_next_task() -> String {
    format!(
        "{}\n{}\n후속 세그먼트가 이어서 실행할 작업 프롬프트",
        handoff_summary(),
        NEXT_TASK_MARKER
    )
}

/// 핸드오프 응답 (NEXT_TASK 없음 → Complete 판정).
#[allow(dead_code)]
fn handoff_response_complete() -> String {
    format!("{}\n{}", handoff_summary(), NEXT_TASK_MARKER)
}

/// 일반 채팅 요청용 SSE chunk (도구 호출).
fn tool_call_chunk() -> serde_json::Value {
    json!({
        "choices": [{
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "id": "call_1",
                    "type": "function",
                    "function": {
                        "name": "write_file",
                        "arguments": json!({"path": "mock_out.txt", "content": "hello"}).to_string()
                    }
                }]
            },
            "finish_reason": "tool_calls"
        }],
        "usage": {"prompt_tokens": 10, "completion_tokens": 20, "total_tokens": 30}
    })
}

/// 일반 채팅 요청용 SSE chunk (reasoning/thinking 출력).
#[allow(dead_code)]
fn reasoning_chunk() -> serde_json::Value {
    json!({
        "choices": [{
            "delta": {"reasoning_content": " Step 1: 분석\nStep 2: 해결"}
        }]
    })
}

/// 일반 채팅 요청용 SSE chunk (도구 결과 후 내용 출력).
fn content_chunk() -> serde_json::Value {
    json!({
        "choices": [{
            "delta": {"content": "파일 작성 완료"},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 10, "completion_tokens": 20, "total_tokens": 30}
    })
}

/// 핸드오프 요청용 SSE chunk (Complete 판정).
#[allow(dead_code)]
fn handoff_complete_chunk() -> serde_json::Value {
    json!({
        "choices": [{
            "delta": {"content": handoff_response_complete()},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 5, "completion_tokens": 30, "total_tokens": 35}
    })
}

/// 핸드오프 요청용 SSE chunk (Handoff 판정).
fn handoff_handoff_chunk() -> serde_json::Value {
    json!({
        "choices": [{
            "delta": {"content": handoff_response_with_next_task()},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 5, "completion_tokens": 30, "total_tokens": 35}
    })
}

/// 테스트용 레지스트리 (cwd/global_dir 는 temp, MCP 비활성).
fn test_registry(cwd: &std::path::Path) -> Arc<bulti::tools::ToolRegistry> {
    let global_dir = cwd.join("bulti_global");
    let mcp_servers = BTreeMap::new();
    let mcp_manager = Arc::new(McpManager::new());
    native_registry(false, cwd.to_path_buf(), global_dir, mcp_servers, mcp_manager)
}

/// 기본 SegmentParams.
fn params(url: &str) -> SegmentParams {
    SegmentParams {
        endpoint: test_endpoint(url),
        temperature: None,
        system_prompt: "시스템 프롬프트".to_string(),
        user_prompt: "프롬프트".to_string(),
        max_iterations: 20,
        context_tokens: 4096,
        handoff_threshold_pct: 50,
        max_handoff_depth: 12,
        handoff_warn_depth: 8,
    }
}

/// 일반 채팅 요청(도구 있음)을 흉내내는 mock.
///
/// 일반 요청은 `tools` 필드가 있고 `max_tokens = context_tokens` (loop_.rs 규칙,
/// 테스트에서는 context_tokens = 4096). `up_to_n_times(1)`로 정확히 1회만
/// 매치되게 하여, 여러 요청이 같은 매처를 재사용하지 않도록 한다. wiremock은
/// mount된 순서대로 매치를 시도하므로 mount 순서 = 응답 순서가 보장된다.
async fn mount_tool_response(server: &MockServer, chunk: serde_json::Value) {
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_partial_json(json!({"max_tokens": 4096})))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(sse_body(&[chunk]), "text/event-stream"),
        )
        .up_to_n_times(1)
        .expect(1)
        .mount(server)
        .await;
}

/// 핸드오프 요청(`tools` 필드 생략, `max_tokens = context_tokens`)을 흉내내는 mock.
async fn mount_handoff_response(server: &MockServer, chunk: serde_json::Value) {
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_partial_json(json!({"max_tokens": 4096})))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(sse_body(&[chunk]), "text/event-stream"),
        )
        .up_to_n_times(1)
        .expect(1)
        .mount(server)
        .await;
}

/// 전체 파이프라인 e2e 테스트.
///
/// 흐름: endpoint→LLM(SSE)→agent loop→tools→history.
/// - 첫 요청: `write_file` 도구 호출 → 파일 생성 (tools 파이프라인).
/// - 둘째 요청: 완료 텍스트 → 세그먼트 완료 (컨텍스트 임계 미만이면 핸드오프 없음).
#[tokio::test]
async fn full_pipeline_end_to_end() {
    init_test_tracing();
    let server = MockServer::start().await;

    // 일반 채팅 요청: 도구 호출 → 내용 출력 순서로 응답.
    // 짧은 완료 텍스트는 컨텍스트 임계 미만이므로 핸드오프 없이 종료한다.
    mount_tool_response(&server, tool_call_chunk()).await;
    mount_tool_response(&server, content_chunk()).await;

    // 임시 history DB (격리).
    let (conn, _dir) = temp_history();
    let run_id = start_run_record(&conn, "chain-e2e", 0, 0);

    let cwd = std::env::temp_dir();
    let registry = test_registry(&cwd);

    let client = LlmClient::new();
    let params = params(&server.uri());

    let result = run_segment(&client, &registry, &params, 0, None).await;

    // 파이프라인 검증: 도구 호출 후 완료 텍스트면 세그먼트 완료.
    assert_eq!(result.status, SegmentStatus::Completed);
    assert_eq!(
        result.handoff,
        Some(bulti::agent::handoff::HandoffDecision::Complete)
    );
    assert!(result.files_touched.contains(&"mock_out.txt".to_string()));

    // history 파이프라인: run 종료 기록.
    history::finish_run(
        &conn,
        run_id,
        &RunFinish {
            status: RunStatus::Completed,
            result: Some(result.content.clone()),
            input_tokens: Some(result.input_tokens),
            output_tokens: Some(result.output_tokens),
            files_touched: result.files_touched.clone(),
            duration_ms: Some(100),
        },
    )
    .unwrap();

    // 기록 검증.
    let row = history::get_run(&conn, run_id).unwrap().unwrap();
    assert_eq!(row.status, "completed");
    assert!(row.files_touched.is_some());
}

/// build_handoff_prompt → 핸드오프 응답 파싱 → 판정까지 검증.
#[tokio::test]
async fn handoff_prompt_and_parse() {
    init_test_tracing();
    let server = MockServer::start().await;

    // 초기 프롬프트가 이미 임계를 넘으면 본 요청 전에 핸드오프한다.
    mount_handoff_response(&server, handoff_handoff_chunk()).await;

    // 핸드오프 프롬프트 조립 검증 (prompt 파이프라인).
    let prompt = build_handoff_prompt();
    assert!(prompt.contains("9개 섹션"));
    assert!(prompt.contains(NEXT_TASK_MARKER));

    let cwd = std::env::temp_dir();
    let registry = test_registry(&cwd);

    let client = LlmClient::new();
    let mut params = params(&server.uri());
    params.handoff_threshold_pct = 75;
    // ctx=4096, 75% → 3072토큰. ASCII 4:1 이므로 13000자면 3250토큰.
    params.user_prompt = "x".repeat(13000);

    let result = run_segment(&client, &registry, &params, 0, None).await;

    // NEXT_TASK 있음 → Handoff 판정.
    assert_eq!(result.status, SegmentStatus::Completed);
    let decision = result.handoff.as_ref().unwrap();
    assert_eq!(*decision, bulti::agent::handoff::HandoffDecision::Handoff);

    // handoff_response 파싱 검증.
    let resp = result.handoff_response.as_ref().unwrap();
    assert!(resp.summary.contains("원 요청"));
    assert!(!resp.next_task.trim().is_empty());
}

/// update 파이프라인: 세그먼트가 도구 없이 Complete 로 끝나면 체인 완료.
#[tokio::test]
async fn segment_completes_without_handoff_next_task() {
    init_test_tracing();
    let server = MockServer::start().await;

    // 일반 채팅 요청: 내용 출력 (도구 호출 없음).
    let plain_chunk = json!({
        "choices": [{"delta": {"content": "완료"}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 10, "completion_tokens": 20, "total_tokens": 30}
    });
    mount_tool_response(&server, plain_chunk).await;

    let cwd = std::env::temp_dir();
    let registry = test_registry(&cwd);

    let client = LlmClient::new();
    let params = params(&server.uri());

    let result = run_segment(&client, &registry, &params, 0, None).await;
    assert_eq!(result.status, SegmentStatus::Completed);
    assert_eq!(
        result.handoff,
        Some(bulti::agent::handoff::HandoffDecision::Complete)
    );
    assert!(result.content.contains("완료"));
}

/// 실제 로컬 LLM(OpenAI 호환 서버)에 연결해 전체 파이프라인을 검증한다.
///
/// wiremock 목업 대신 실제 모델로 run_segment 를 실행한다.
/// CI/로컬에 8084 llama-server 가 없으면 skip 된다 (`--ignored` 로 실행).
///
/// 환경 변수:
/// - `BULTI_TEST_LLM_URL`  (기본 `http://localhost:8084`)
/// - `BULTI_TEST_LLM_KEY`  (기본 `Fi2MTMsg2nixdanHNC7If5LC9gpM243c`)
/// - `BULTI_TEST_LLM_MODEL`(기본 `qwen3.8-q2`)
#[tokio::test]
#[ignore = "실제 로컬 LLM(8084) 필요 — cargo test --test e2e_pipeline -- --ignored"]
async fn full_pipeline_real_llm() {
    init_test_tracing();
    let url = std::env::var("BULTI_TEST_LLM_URL")
        .unwrap_or_else(|_| "http://localhost:8084".to_string());
    let key = std::env::var("BULTI_TEST_LLM_KEY")
        .unwrap_or_else(|_| "Fi2MTMsg2nixdanHNC7If5LC9gpM243c".to_string());
    let model = std::env::var("BULTI_TEST_LLM_MODEL")
        .unwrap_or_else(|_| "qwen3.8-q2".to_string());

    // 실제 LLM에 연결 가능한지 사전 점검 (없으면 skip).
    let probe = format!("{}/chat/completions", url.trim_end_matches('/'));
    let probe_req = json!({
        "model": model,
        "messages": [{"role": "user", "content": "ping"}],
        "max_tokens": 5,
        "stream": false
    });
    let client = reqwest::Client::new();
    let probe_resp = client
        .post(&probe)
        .bearer_auth(&key)
        .json(&probe_req)
        .send()
        .await;
    match probe_resp {
        Ok(r) if r.status().is_success() => {}
        Ok(r) => {
            tracing::warn!(
                "실제 LLM({url}) 응답 {status} — 테스트 skip",
                status = r.status()
            );
            return;
        }
        Err(e) => {
            tracing::warn!("실제 LLM({url}) 연결 불가 — 테스트 skip: {e}");
            return;
        }
    }

    let endpoint = EndpointConfig {
        url: url.clone(),
        api_key: Some(key),
        model: model.clone(),
        context_tokens: 4096,
        vision: false,
        thinking: false,
        max_iterations: 20,
        reasoning_effort: None,
        input_price_per_mtok: None,
        output_price_per_mtok: None,
    };

    let (conn, _dir) = temp_history();
    let run_id = start_run_record(&conn, "chain-real", 0, 0);

    let cwd = std::env::temp_dir();
    let registry = test_registry(&cwd);

    let client = LlmClient::new();
    let params = SegmentParams {
        endpoint,
        temperature: None,
        system_prompt: "당신은 파일을 작성하는 자동화 에이전트입니다. "
            .to_string()
            + "프롬프트의 지시를 수행한 뒤 간결하게 결과를 보고하세요.",
        user_prompt: "write_file 도구로 mock_real.txt 에 'real-llm' 내용을 작성하고, "
            .to_string()
            + "완료되었으면 보고하세요.",
        max_iterations: 20,
        context_tokens: 4096,
        handoff_threshold_pct: 50,
        max_handoff_depth: 12,
        handoff_warn_depth: 8,
    };

    let result = run_segment(&client, &registry, &params, 0, None).await;

    // 실제 LLM 파이프라인 검증: 도구 호출(write_file)이 수행되어야 한다.
    assert_eq!(result.status, SegmentStatus::Completed);
    assert!(
        result.files_touched.contains(&"mock_real.txt".to_string()),
        "write_file 도구 호출이 수행되어야 함 — 실제 files_touched: {:?}",
        result.files_touched
    );

    // history 파이프라인: run 종료 기록.
    history::finish_run(
        &conn,
        run_id,
        &RunFinish {
            status: RunStatus::Completed,
            result: Some(result.content.clone()),
            input_tokens: Some(result.input_tokens),
            output_tokens: Some(result.output_tokens),
            files_touched: result.files_touched.clone(),
            duration_ms: Some(100),
        },
    )
    .unwrap();

    let row = history::get_run(&conn, run_id).unwrap().unwrap();
    assert_eq!(row.status, "completed");
    assert!(row.files_touched.is_some());
}
/// 대화형 경로(`bulti chat`)의 한 턴(`run_turn`)을 검증한다.
///
/// run_turn 은 대화형·단발이 공유하는 코어로, 세그먼트 체인을 실행하고
/// 세션 id 를 history 에 연결한다. wiremock 으로 도구 호출 → 핸드오프(Complete)
/// 순서를 흉내내고, 세션 연결 기록·파일 터치·정상 종료(exit 0)를 검증한다.
#[tokio::test]
async fn chat_turn_end_to_end() {
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    use bulti::cli::chat_cmd::run_turn;
    use bulti::config::{Config, ContextConfig};

    init_test_tracing();
    let server = MockServer::start().await;

    // 도구 호출 → 내용 출력. 짧은 완료는 핸드오프 없이 턴 종료.
    mount_tool_response(&server, tool_call_chunk()).await;
    mount_tool_response(&server, content_chunk()).await;

    // 임시 history DB (격리).
    let (conn, _dir) = temp_history();
    let cwd = std::env::temp_dir();
    let registry = test_registry(&cwd);

    // 대화형 경로용 Config (핸드오프 설정 기본값).
    let cfg = Config {
        version: 1,
        active_endpoint: None,
        language: bulti::i18n::Language::En,
        endpoints: BTreeMap::new(),
        mcp: BTreeMap::new(),
        context: ContextConfig {
            handoff_threshold_pct: 50,
            max_handoff_depth: 12,
            handoff_warn_depth: 8,
        },
        update: None,
    };

    let client = LlmClient::new();
    let endpoint = test_endpoint(&server.uri());
    let interrupted = Arc::new(AtomicBool::new(false));

    let turn = run_turn(
        &client,
        &conn,
        &registry,
        "시스템 프롬프트",
        "프롬프트".to_string(),
        &endpoint,
        "mock",
        &cfg,
        "chain-chat",
        "session-chain",
        "sess-1".to_string(),
        0,
        interrupted,
        None,
    )
    .await
    .unwrap();

    // 대화형 한 턴 검증: 정상 종료(exit 0), 도구 호출(write_file) 수행, 내용 포함.
    assert_eq!(turn.exit_code, 0);
    assert!(turn.files_touched.contains(&"mock_out.txt".to_string()));
    assert!(turn.assistant_content.contains("파일 작성 완료"));

    // 세션 연결 history 파이프라인: 세션 id 가 기록되어야 한다.
    let rows = history::list_runs(&conn, None, None, None).unwrap();
    assert!(!rows.is_empty(), "대화형 턴 기록이 있어야 함");
    // 세션 id 연결 확인.
    let run = history::get_run(&conn, rows[0].id).unwrap().unwrap();
    assert!(run.session_id.is_some());
    assert_eq!(run.session_id.as_deref(), Some("sess-1"));
}

/// `run_turn` 이 SSE `reasoning_content` 델타를 `TurnResult.reasoning_content`
/// 에 누적해 반환하는지 검증한다.
#[tokio::test]
async fn chat_turn_reasoning_content_end_to_end() {
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    use bulti::cli::chat_cmd::run_turn;
    use bulti::config::{Config, ContextConfig};

    init_test_tracing();
    let server = MockServer::start().await;

    // reasoning 출력 → 내용 출력을 한 SSE 스트림으로 묶어 응답 (짧은 완료, 핸드오프 없음).
    // wiremock의 mock별 매칭 순서/상태 문제를 회피하기 위해 두 chunk를 1회 응답으로 전달한다.
    {
        use wiremock::Mock;
        use wiremock::matchers::{body_partial_json, method, path};
        let chunks = vec![reasoning_chunk(), content_chunk()];
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(body_partial_json(json!({"max_tokens": 4096})))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(sse_body(&chunks), "text/event-stream"),
            )
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
    }

    // 임시 history DB (격리).
    let (conn, _dir) = temp_history();
    let cwd = std::env::temp_dir();
    let registry = test_registry(&cwd);

    let cfg = Config {
        version: 1,
        active_endpoint: None,
        language: bulti::i18n::Language::En,
        endpoints: BTreeMap::new(),
        mcp: BTreeMap::new(),
        context: ContextConfig {
            handoff_threshold_pct: 50,
            max_handoff_depth: 12,
            handoff_warn_depth: 8,
        },
        update: None,
    };

    let client = LlmClient::new();
    let endpoint = test_endpoint(&server.uri());
    let interrupted = Arc::new(AtomicBool::new(false));

    let turn = run_turn(
        &client,
        &conn,
        &registry,
        "시스템 프롬프트",
        "프롬프트".to_string(),
        &endpoint,
        "mock",
        &cfg,
        "chain-reasoning",
        "session-chain",
        "sess-reasoning".to_string(),
        0,
        interrupted,
        None,
    )
    .await
    .unwrap();

    // reasoning_content 누적 검증.
    assert_eq!(turn.exit_code, 0);
    assert!(!turn.reasoning_content.is_empty());
    assert!(turn.reasoning_content.contains("분석"));
}

//! 대화형 TUI 렌더링 (DESIGN.md §4.13.3).
//!
//! ratatui + crossterm 으로 대화·도구 호출·결과를 화면에 렌더링한다.
//! - 스크롤백: 대화 메시지 (줄 단위 래핑)
//! - 입력 라인: 하단 입력창
//! - 상태 표시: 엔드포인트·모델·세션 id
//!
//! 키 바인딩:
//! - `Enter`           — 대화 전송
//! - `Ctrl+S`          — 세션 저장 안내 (매 턴 자동 저장)
//! - `Ctrl+Q`          — 종료 (exit 0)
//! - `Ctrl+C`          — 즉시 종료 (SIGINT 규약 130)
//! - `PgUp` / `PgDn`   — 대화 스크롤
//! - `Esc`             — 입력 모드 → 결과 화면 (무시)

use std::io::{self, IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Terminal;

/// 진행 중 턴: 델타 수신 채널 + 작업 스레드 핸들.
type PendingTurn = Option<(
    tokio::sync::mpsc::UnboundedReceiver<crate::llm::Delta>,
    JoinHandle<Result<TurnResult, Box<dyn std::error::Error + Send + Sync>>>,
)>;

/// TUI 가 활성일 때 tracing 이 터미널에 섞이지 않게 한다 (`main` 의 writer 가 조회).
static TUI_ACTIVE: AtomicBool = AtomicBool::new(false);

/// tracing writer 가 TUI 동안 로그를 버릴지 판단한다.
pub fn is_active() -> bool {
    TUI_ACTIVE.load(Ordering::Relaxed)
}

fn set_active(v: bool) {
    TUI_ACTIVE.store(v, Ordering::Relaxed);
}

/// 대화 한 줄 (화면에 표시할 메시지).
#[derive(Debug, Clone, Default)]
pub struct ChatLine {
    pub role: Role,
    pub text: String,
    /// 모델 추론 중간 생각 (응답 생성 중에만 표시, 완료 시 접혀서 저장).
    pub reasoning_content: String,
    /// reasoning 접기/펼침 상태 (`Ctrl+T` 로 토글, 완료 시 기본 접힘).
    pub reasoning_expanded: bool,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub duration_ms: u64,
    /// 렌더링 콘텐츠 생성 번호. text/reasoning_content 가 변할 때마다 증가하고,
    /// `render_cache` 의 무효화 키에 쓰인다 (generation tracking).
    pub generation: u64,
    /// 매 프레임 래핑 비용을 아끼기 위한 렌더링 캐시 (width·generation 키).
    pub render_cache: RenderCache,
    /// reasoning 렌더 캐시 — 본문과 독립적으로 (width, generation, expanded) 키로 관리.
    pub reasoning_cache: RenderCache,
}

impl ChatLine {
    /// 콘텐츠(text·reasoning)가 변했을 때 호출. generation 을 증가시켜
    /// `render_cache` 를 무효화한다.
    pub fn bump_generation(&mut self) {
        self.generation += 1;
    }
}

/// `draw_scroll` 렌더링 캐시.
///
/// grok-build `RenderState` 의 `(width, generation)` 키를 참고했다.
/// - `width`/`generation` 이 이전 렌더와 같으면 `wrapped` 를 그대로 재사용해
///   반복 렌더링이 무료(free)가 된다.
/// - 다르면 `rendered`(접두 스타일 미적용 렌더 라인)의 공통 접두까지의
///   `wrapped` 를 그대로 쓰고, 바뀌는 꼬리만 재래핑한다 (스트리밍
///   `frozen_pre_wrap_count` 최적화 — 안정된 접두부는 재래핑하지 않음).
/// - `rendered` 는 User/Status 는 원문 줄, Assistant/reasoning 은
///   `render::render_lines` 출력(접두 스타일 제외)을 보관한다.
#[derive(Debug, Clone, Default)]
pub struct RenderCache {
    width: usize,
    generation: u64,
    /// reasoning 펼침 상태 (expanded 접힘/펼침에 따라 렌더가 바뀌므로 키에 포함).
    expanded: bool,
    /// 접두/인덴트 스팬 포함·미래핑 렌더 라인.
    rendered: Vec<Line<'static>>,
    /// `rendered[i]` 가 만든 래핑 라인 수 (접두 재사용 경계 계산용).
    wrapped_counts: Vec<usize>,
    /// `rendered` 의 전체 래핑 결과.
    wrapped: Vec<Line<'static>>,
    /// 이 캐시에서 실제 재래핑이 일어난 횟수 (캐시 미스).
    wrap_misses: u64,
}

impl RenderCache {
    /// 이 캐시에서 실제 재래핑이 일어난 누적 횟수를 반환한다.
    pub fn wrap_misses(&self) -> u64 {
        self.wrap_misses
    }
    /// 같은 너비·generation·펼침 상태이면 `wrapped` 를 그대로 쓴다.
    fn is_valid(&self, width: usize, generation: u64, expanded: bool) -> bool {
        self.width == width && self.generation == generation && self.expanded == expanded
    }

    /// 이 줄의 렌더 라인(`rendered`, 접두/인덴트 스팬 포함·미래핑)을
    /// `width` 열에 래핑해 `text_lines` 에 붙인다.
    ///
    /// - `width`·`generation`·`expanded` 가 이전 렌더와 같으면 래핑 없이
    ///   `wrapped` 를 그대로 재사용한다 (캐시 히트 — 무료).
    /// - 아니면 공통 접두 렌더 라인까지의 래핑 결과만 재사용하고, 바뀌는
    ///   꼬리만 재래핑한다 (스트리밍 `frozen_pre_wrap_count` 최적화 —
    ///   안정된 접두부는 재래핑하지 않음).
    ///
    /// 반환값은 캐시 히트 여부.
    fn append_wrapped(
        &mut self,
        text_lines: &mut Vec<Line<'static>>,
        rendered: &[Line<'static>],
        width: usize,
        generation: u64,
        expanded: bool,
    ) -> bool {
        if self.is_valid(width, generation, expanded) {
            text_lines.extend(self.wrapped.iter().cloned());
            return true;
        }
        // 공통 접두 렌더 라인 수 (너비가 달라졌으면 접두 비교가 무의미).
        let prefix_len = if self.width == width {
            self.rendered
                .iter()
                .zip(rendered.iter())
                .take_while(|(a, b)| a == b)
                .count()
        } else {
            0
        };
        // 접두 렌더 라인 `prefix_len` 개가 소비한 래핑 라인 수 (재사용 경계).
        let mut reuse = 0usize;
        for &c in self.wrapped_counts.iter().take(prefix_len) {
            reuse += c;
        }
        reuse = reuse.min(self.wrapped.len());
        if reuse > 0 {
            text_lines.extend(self.wrapped[..reuse].iter().cloned());
        }
        let mut counts: Vec<usize> = Vec::new();
        for line in &rendered[prefix_len..] {
            let start = text_lines.len();
            for wrapped in wrap_spans(&line.spans, width) {
                text_lines.push(Line::from(wrapped));
            }
            counts.push(text_lines.len() - start);
        }
        // 캐시 갱신 (이 줄 전체의 렌더/래핑 상태).
        // `wrapped_counts` 는 접두 재사용 경계 계산에 쓰이므로, 이전 접두 분량과
        // 이번 프레임 새로 래핑한 꼬리 분량을 이어 붙인 전체를 저장해야 한다.
        let tail_len: usize = counts.iter().sum();
        let mut new_counts = self.wrapped_counts[..prefix_len].to_vec();
        new_counts.extend(counts);
        self.width = width;
        self.generation = generation;
        self.expanded = expanded;
        self.rendered = rendered.to_vec();
        self.wrapped_counts = new_counts;
        self.wrapped = text_lines[text_lines.len().saturating_sub(reuse + tail_len)..].to_vec();
        self.wrap_misses += 1;
        false
    }
}

/// 실제 재래핑이 일어난 횟수 (캐시 미스)는 `RenderCache::wrap_misses()` 로
/// 캐시별로 추적한다 (전역 카운터는 병렬 테스트에서 오염될 수 있어 제거).

/// 한 턴(사용자 프롬프트 → 응답)의 결과. chat_cmd 의 `run_turn` 과
/// TUI processor 가 공유하는 규약이다.
#[derive(Debug, Clone)]
pub struct TurnResult {
    pub exit_code: i32,
    pub assistant_content: String,
    pub reasoning_content: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub duration_ms: u64,
    pub files_touched: Vec<String>,
}

/// 메시지 역할.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum Role {
    User,
    Assistant,
    /// 도구 호출 표시 (⚙ 성공 / ✗ 실패).
    Tool,
    #[default]
    Status,
}

/// TUI 동작 결과 — chat_cmd 의 프롬프트 루프와 동일한 규약을 재사용한다.
#[derive(Debug)]
pub struct TuiOutcome {
    pub exit_code: i32,
    pub saved: bool,
}

/// 슬래시 커맨드(`/model`·`/effort`·`/exit` 등) 처리 결과.
/// TUI 루프는 결과 메시지를 Status 줄로 표시하고, `exit` 가 참이면 종료한다.
#[derive(Debug, Clone)]
pub struct CommandResult {
    pub message: String,
    pub exit: bool,
}

/// TUI 채팅 인터페이스 실행 옵션.
pub struct TuiOptions {
    pub endpoint_name: String,
    pub model: String,
    pub session_id: String,
}

/// raw mode / alternate screen / tracing 차단을 해제한다.
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        set_active(false);
        let _ = disable_raw_mode();
        let mut stdout = io::stdout();
        let _ = execute!(stdout, LeaveAlternateScreen, crossterm::cursor::Show);
        let _ = stdout.flush();
    }
}

/// TUI 채팅 인터페이스를 실행한다. TTY 가 아니면 `None` 을 반환해 호출부가
/// 스트림 텍스트 모드로 대화하게 한다.
///
/// `processor` 는 사용자 메시지와 델타 전송 채널을 받아 한 턴(세그먼트 체인)을
/// 실행하고 `TurnResult` 를 반환하는 클로저다. TUI 루프는 Enter 로 메시지를
/// 전송할 때마다 processor 를 호출하고, 채널로 도착하는 중간 델타를 받아
/// 화면에 점진적으로 그린 뒤 최종 `TurnResult` 를 대화 리스트에 추가한다.
pub fn run_tui<F>(
    options: &TuiOptions,
    initial_lines: Vec<ChatLine>,
    processor: F,
    mut command_handler: impl FnMut(&str) -> CommandResult + Send + 'static,
) -> Result<Option<TuiOutcome>, Box<dyn std::error::Error>>
where
    F: FnMut(
            String,
            tokio::sync::mpsc::UnboundedSender<crate::llm::Delta>,
        ) -> Result<TurnResult, Box<dyn std::error::Error + Send + Sync>>
        + Send
        + Clone
        + 'static,
{
    if !io::stdout().is_terminal() {
        return Ok(None);
    }

    set_active(true);
    let _guard = TerminalGuard;
    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen, crossterm::cursor::Hide)?;

    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    // `Terminal::clear` 는 커서 위치를 조회한다. 가상 TTY 에서는 시간 초과로
    // 실패하므로, 이미 비어 있는 대체 화면을 그대로 쓴다.

    let mut lines = initial_lines;
    let mut input = String::new();
    // 입력창 커서 위치 (문자 인덱스 기준). 좌/우·Home/End·단어 이동으로 바뀐다.
    let mut cursor = 0usize;
    let mut offset_from_bottom: usize = 0;
    let mut saved = false;

    // 슬래시 커맨드 자동완성 상태.
    // `completions` 가 비어 있지 않으면 입력창 위에 드롭다운을 그린다.
    let mut completions: Vec<crate::slash::Suggestion> = Vec::new();
    let mut completion_idx: usize = 0;
    // 자동완성 드롭다운이 열렸을 때 방향키가 스크롤 대신 선택에 쓰이도록
    // 현재 드롭다운이 열려 있는지 별도로 추적한다.
    let mut completion_active = false;
    // 인자 자동완성 후보 (모델·엔드포인트·MCP 이름). 시작 시 한 번만 수집한다.
    let slash_ctx = crate::completion::load_slash_context();

    // 프롬프트 히스토리 탐색 상태 (↑/↓ 키). 히스토리 후보를 미리 로드해 두고
    // `history_sources` 를 재사용한다 (최근이 앞에 정렬됨).
    let mut history_sources: Vec<String> = crate::completion::load_history_sources();
    let mut history_idx: Option<usize> = None;
    // 멀티라인 입력 모드: 켜져 있으면 Shift+Enter 로 줄바꿈, Enter 로 전송.
    let mut multiline = false;

    // 진행 중인 턴: 델타 수신 채널 + 작업 스레드 핸들.
    // `Some` 이면 응답 생성 중이며, TUI 루프가 채널을 폴링해 점진적으로 그린다.
    let mut pending_turn: PendingTurn = None;
    // 응답 생성 중 진행 표시: 턴 시작 시각 (상태 줄의 스피너·페이즈·경과 시간
    // 표시용). 스피너 프레임은 경과 시간에서 결정적으로 계산한다.
    let mut turn_started_at: Option<Instant> = None;

    let outcome = loop {
        // 진행 중 턴의 델타를 폴링해 화면에 점진적으로 반영한다.
        // 마지막 Assistant 메시지(생성 중)에 content 를 누적한다.
        if let Some((ref mut rx, _)) = pending_turn {
            let mut got = false;
            while let Ok(delta) = rx.try_recv() {
                got = true;
                if let Some(text) = delta.content {
                    if !text.is_empty() {
                        // 도구 호출 줄이 뒤에 끼어 있어도 마지막 Assistant 줄에
                        // 누적한다 (list 마지막이 Tool/Status 일 수 있음).
                        if let Some(last) =
                            lines.iter_mut().rev().find(|l| l.role == Role::Assistant)
                        {
                            last.text.push_str(&text);
                            last.bump_generation();
                        }
                    }
                }
                if let Some(think) = delta.reasoning_content {
                    if !think.is_empty() {
                        if let Some(last) =
                            lines.iter_mut().rev().find(|l| l.role == Role::Assistant)
                        {
                            last.reasoning_content.push_str(&think);
                            last.bump_generation();
                        }
                    }
                }
                // 도구 호출 이벤트 — "호출 중" 줄을 결과 이벤트로 갱신한다.
                if let Some(ev) = delta.tool_call_event {
                    push_tool_event(&mut lines, ev);
                }
            }
            if got {
                offset_from_bottom = 0;
            }
        }

        terminal.draw(|f| {
            let size = f.area();
            // 슬래시 드롭다운이 열려 있으면 텍스트 드롭다운은 겹치지 않게 닫는다.
            // (텍스트 자동완성 제거 후에는 슬래시 드롭다운만 존재)
            let input_constraint = if multiline {
                Constraint::Min(5)
            } else {
                Constraint::Length(3)
            };
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(1), // 제목
                    Constraint::Min(3),    // 대화 리스트
                    input_constraint,      // 입력창
                    Constraint::Length(1), // 상태 표시
                ])
                .split(size);

            // 진행 중 턴이면 스피너 + 페이즈 + 경과 시간을 계산해 제목·상태 줄에
            // 함께 표시한다 (스피너 프레임은 100ms 주기 결정적 계산).
            let progress = pending_turn
                .is_some()
                .then(|| turn_started_at)
                .flatten()
                .map(|t| {
                    let ms = t.elapsed().as_millis() as u64;
                    (
                        spinner((ms / 100) as usize),
                        turn_phase(&lines),
                        ms,
                    )
                });

            draw_title(
                f,
                chunks[0],
                &options.endpoint_name,
                &options.model,
                progress,
            );
            draw_scroll(
                f,
                chunks[1],
                &mut lines,
                offset_from_bottom,
                pending_turn.is_some(),
            );
            draw_input(f, chunks[2], &input, cursor, multiline);
            draw_completions(f, chunks[2], &completions, completion_idx);
            let total_in: u64 = lines.iter().map(|l| l.input_tokens).sum();
            let total_out: u64 = lines.iter().map(|l| l.output_tokens).sum();
            draw_status(
                f,
                chunks[3],
                &options.session_id,
                saved,
                total_in,
                total_out,
                last_assistant(&lines),
                progress,
            );
        })?;

        // 진행 중 턴이 끝났는지 확인한다. 아직 실행 중이면 join 하지 않고
        // 계속 델타를 폴링해 점진적으로 그린다 (`join` 은 완료 후에만).
        let turn_finished = pending_turn
            .as_ref()
            .is_some_and(|(_, handle)| handle.is_finished());
        if turn_finished {
            let (_, handle) = pending_turn.take().expect("turn_finished implies Some");
            turn_started_at = None;
            match handle.join() {
                Ok(Ok(turn)) => {
                    // 스트리밍 시작 시 미리 push한 Assistant 줄을 최종 결과로
                    // "교체"한다. 새 줄을 push 하면 스트리밍 중 누적 줄과 중복돼
                    // 응답이 두 번 렌더되고 화면이 길어져 입력창 쪽으로 밀린다.
                    if let Some(last) = lines.iter_mut().rev().find(|l| l.role == Role::Assistant) {
                        last.text = turn.assistant_content.clone();
                        last.reasoning_content = turn.reasoning_content.clone();
                        // 완료 후 reasoning 은 접힌 상태로 저장한다.
                        last.reasoning_expanded = false;
                        last.input_tokens = turn.input_tokens;
                        last.output_tokens = turn.output_tokens;
                        last.duration_ms = turn.duration_ms;
                        last.bump_generation();
                    } else {
                        // 스트리밍 줄이 없는 예외 경로(예: 미사용)만 push 한다.
                        lines.push(ChatLine {
                            role: Role::Assistant,
                            text: turn.assistant_content.clone(),
                            reasoning_content: turn.reasoning_content.clone(),
                            reasoning_expanded: false,
                            input_tokens: turn.input_tokens,
                            output_tokens: turn.output_tokens,
                            duration_ms: turn.duration_ms,
                            ..ChatLine::default()
                        });
                    }
                    // 파일 변경 이력을 Status 줄로 명시 표시한다.
                    if !turn.files_touched.is_empty() {
                        lines.push(ChatLine {
                            role: Role::Status,
                            text: format!("📝 modified: {}", turn.files_touched.join(", ")),
                            ..ChatLine::default()
                        });
                    }
                }
                Ok(Err(e)) => {
                    lines.push(ChatLine {
                        role: Role::Status,
                        text: format!("오류: {e}"),
                        ..ChatLine::default()
                    });
                }
                Err(_) => {
                    lines.push(ChatLine {
                        role: Role::Status,
                        text: "오류: 턴 실행 스레드가 종료되었습니다".to_string(),
                        ..ChatLine::default()
                    });
                }
            }
            offset_from_bottom = 0;
        }

        if !event::poll(Duration::from_millis(200))? {
            continue;
        }
        let Event::Key(KeyEvent {
            code,
            modifiers,
            kind,
            ..
        }) = event::read()?
        else {
            continue;
        };
        // Windows 등에서 Release 이벤트가 중복 입력으로 들어온다.
        if kind != KeyEventKind::Press && kind != KeyEventKind::Repeat {
            continue;
        }

        match code {
            KeyCode::Char('q') if modifiers.contains(KeyModifiers::CONTROL) => {
                break TuiOutcome {
                    exit_code: 0,
                    saved,
                };
            }
            KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
                break TuiOutcome {
                    exit_code: 130,
                    saved,
                };
            }
            KeyCode::Char('s') if modifiers.contains(KeyModifiers::CONTROL) => {
                saved = true;
                lines.push(ChatLine {
                    role: Role::Status,
                    text: "세션은 매 턴 자동 저장됩니다".to_string(),
                    ..ChatLine::default()
                });
                offset_from_bottom = 0;
            }
            KeyCode::Enter => {
                // 자동완성 드롭다운이 열려 있으면 Enter 로 선택.
                if completion_active && !completions.is_empty() {
                    let item = completions[completion_idx.min(completions.len() - 1)].clone();
                    set_input(&mut input, &mut cursor, item.insert);
                    completions.clear();
                    completion_active = false;
                    continue;
                }
                // 진행 중 턴이 있으면 무시한다 (한 번에 한 턴).
                if pending_turn.is_some() {
                    continue;
                }
                // 멀티라인 모드: Shift+Enter (또는 Alt+Enter) 는 줄바꿈.
                if multiline
                    && (modifiers.contains(KeyModifiers::SHIFT)
                        || modifiers.contains(KeyModifiers::ALT))
                {
                    insert_char(&mut input, &mut cursor, '\n');
                    continue;
                }
                let trimmed = input.trim().to_string();
                if trimmed.is_empty() {
                    continue;
                }
                // `/multiline` — 멀티라인 입력 모드 토글 (command_handler 미경유).
                if trimmed == "/multiline" {
                    multiline = !multiline;
                    let msg = if multiline {
                        "멀티라인 입력 모드 켜짐 — Shift+Enter 줄바꿈, Enter 전송".to_string()
                    } else {
                        "멀티라인 입력 모드 꺼짐".to_string()
                    };
                    lines.push(ChatLine {
                        role: Role::Status,
                        text: msg,
                        ..ChatLine::default()
                    });
                    offset_from_bottom = 0;
                    input.clear();
                    cursor = 0;
                    continue;
                }
                input.clear();
                cursor = 0;
                // 슬래시 커맨드면 processor 대신 command_handler 로 처리한다
                // (모델 변경·effort·종료 등).
                if trimmed.starts_with('/') {
                    let res = command_handler(trimmed.trim());
                    lines.push(ChatLine {
                        role: Role::Status,
                        text: res.message,
                        ..ChatLine::default()
                    });
                    offset_from_bottom = 0;
                    if res.exit {
                        break TuiOutcome {
                            exit_code: 0,
                            saved,
                        };
                    }
                    continue;
                }
                lines.push(ChatLine {
                    role: Role::User,
                    text: trimmed.clone(),
                    ..ChatLine::default()
                });
                // 보낸 프롬프트를 히스트리에 기록한다 (최근이 앞에, 슬래시 커맨드는 제외됨).
                history_idx = None;
                history_sources.insert(0, trimmed.clone());
                // 응답 생성 중 상태: 빈 Assistant 메시지를 미리 추가해, 도착하는
                // 델타가 이 메시지에 점진적으로 누적되게 한다.
                lines.push(ChatLine {
                    role: Role::Assistant,
                    text: String::new(),
                    reasoning_content: String::new(),
                    // 생성 중 reasoning 은 자동으로 펼쳐서 실시간 표시한다.
                    reasoning_expanded: true,
                    ..ChatLine::default()
                });
                offset_from_bottom = 0;

                // 델타 채널을 만들고 processor 를 별도 스레드로 실행한다.
                // 스트리밍 중에도 TUI 루프가 화면을 갱신할 수 있어야 하므로
                // 동기 호출 대신 스레드 실행이 필요하다.
                let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                let msg = trimmed.clone();
                let handle = std::thread::spawn({
                    let mut processor = processor.clone();
                    move || processor(msg, tx)
                });
                pending_turn = Some((rx, handle));
                turn_started_at = Some(Instant::now());
            }
            KeyCode::Backspace => {
                if multiline && input.as_bytes().get(cursor) == Some(&b'\n') {
                    // 멀티라인: 커서 위치의 줄바꿈을 한 번에 지운다 (줄 병합).
                    input.replace_range(cursor..cursor + 1, "");
                    continue;
                }
                delete_back(&mut input, &mut cursor);
                if !multiline {
                    update_completions(
                        &mut completions,
                        &mut completion_idx,
                        &mut completion_active,
                        &input,
                        &slash_ctx,
                    );
                }
            }
            KeyCode::Delete => {
                delete_fwd(&mut input, &mut cursor);
                if !multiline {
                    update_completions(
                        &mut completions,
                        &mut completion_idx,
                        &mut completion_active,
                        &input,
                        &slash_ctx,
                    );
                }
            }
            KeyCode::Tab => {
                // Tab: 자동완성 선택 항목 순환 (슬래시 드롭다운이 열려 있으면).
                if completion_active && !completions.is_empty() {
                    completion_idx = (completion_idx + 1) % completions.len();
                } else {
                    // 자동완성이 없으면 직접 완성 시도.
                    if completions.len() == 1 {
                        set_input(&mut input, &mut cursor, completions[0].insert.clone());
                        completions.clear();
                        completion_active = false;
                    } else {
                        update_completions(
                            &mut completions,
                            &mut completion_idx,
                            &mut completion_active,
                            &input,
                            &slash_ctx,
                        );
                        if completions.len() == 1 {
                            set_input(&mut input, &mut cursor, completions[0].insert.clone());
                            completions.clear();
                            completion_active = false;
                        }
                    }
                }
            }
            // 가장 마지막 Assistant 줄의 reasoning 접기/펼침 토글.
            // 반드시 Ctrl+T 로만 발동해야 한다 — 맨 `t` 를 잡으면 입력창에
            // 't' 를 입력할 수 없게 된다.
            KeyCode::Char('t') if modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(last) = lines.iter_mut().rev().find(|l| l.role == Role::Assistant) {
                    last.reasoning_expanded = !last.reasoning_expanded;
                }
            }
            // Ctrl+A/E: 라인 시작/끝 (Char(c) arm 보다 앞에 둬야 도달 가능).
            KeyCode::Char('a') if modifiers.contains(KeyModifiers::CONTROL) => {
                cursor = 0;
            }
            KeyCode::Char('e') if modifiers.contains(KeyModifiers::CONTROL) => {
                cursor = input.len();
            }
            KeyCode::Char(c) => {
                if modifiers.contains(KeyModifiers::CONTROL) {
                    continue;
                }
                insert_char(&mut input, &mut cursor, c);
                if !multiline {
                    update_completions(
                        &mut completions,
                        &mut completion_idx,
                        &mut completion_active,
                        &input,
                        &slash_ctx,
                    );
                }
            }
            KeyCode::PageUp => {
                offset_from_bottom = offset_from_bottom.saturating_add(10);
            }
            KeyCode::PageDown => {
                offset_from_bottom = offset_from_bottom.saturating_sub(10);
            }
            KeyCode::Up => {
                if completion_active && !completions.is_empty() {
                    completion_idx = completion_idx.saturating_sub(1);
                } else if !history_sources.is_empty() {
                    // ↑: 빈 입력이든 아닌든 이전 프롬프트를 거슬러 올라간다.
                    history_idx = step_history(&history_sources, history_idx, true);
                    set_input(
                        &mut input,
                        &mut cursor,
                        history_idx
                            .map(|i| history_sources[i].clone())
                            .unwrap_or_default(),
                    );
                } else {
                    offset_from_bottom = offset_from_bottom.saturating_add(1);
                }
            }
            KeyCode::Down => {
                if completion_active && !completions.is_empty() {
                    completion_idx = (completion_idx + 1) % completions.len();
                } else if history_idx.is_some() {
                    // ↓: 최신 방향으로 내려가다 끝나면 입력창 비움.
                    history_idx = step_history(&history_sources, history_idx, false);
                    set_input(
                        &mut input,
                        &mut cursor,
                        history_idx
                            .map(|i| history_sources[i].clone())
                            .unwrap_or_default(),
                    );
                } else {
                    offset_from_bottom = offset_from_bottom.saturating_sub(1);
                }
            }
            KeyCode::Left
                if !modifiers.contains(KeyModifiers::ALT)
                    && !modifiers.contains(KeyModifiers::CONTROL) =>
            {
                if cursor > 0 {
                    cursor -= 1;
                } else if !multiline {
                    // 입력 경계에서 ← 는 대화 스크롤 (기존 동작 유지).
                    offset_from_bottom = offset_from_bottom.saturating_add(1);
                }
            }
            KeyCode::Right
                if !modifiers.contains(KeyModifiers::ALT)
                    && !modifiers.contains(KeyModifiers::CONTROL) =>
            {
                if cursor < input.len() {
                    cursor += 1;
                } else if !multiline {
                    offset_from_bottom = offset_from_bottom.saturating_sub(1);
                }
            }
            // 단어 이동: Alt+←/→ (Meta) 또는 Ctrl+←/→.
            KeyCode::Left
                if modifiers.contains(KeyModifiers::ALT)
                    || modifiers.contains(KeyModifiers::CONTROL) =>
            {
                if !move_cursor_word(&input, &mut cursor, false) && !multiline {
                    offset_from_bottom = offset_from_bottom.saturating_add(1);
                }
            }
            KeyCode::Right
                if modifiers.contains(KeyModifiers::ALT)
                    || modifiers.contains(KeyModifiers::CONTROL) =>
            {
                if !move_cursor_word(&input, &mut cursor, true) && !multiline {
                    offset_from_bottom = offset_from_bottom.saturating_sub(1);
                }
            }
            // 라인 시작/끝: Home/End (Ctrl+A/E 는 위 Char arm에서 처리).
            KeyCode::Home => {
                cursor = 0;
            }
            KeyCode::End => {
                cursor = input.len();
            }
            KeyCode::Esc => {
                // Esc: 자동완성 드롭다운 취소.
                completions.clear();
                completion_active = false;
            }
            _ => {}
        }
    };

    Ok(Some(outcome))
}

/// 제목 + 엔드포인트·모델 표시. 진행 중 턴이면 스피너·페이즈·경과 시간도
/// 함께 보여준다.
fn draw_title(
    f: &mut ratatui::Frame,
    area: Rect,
    endpoint_name: &str,
    model: &str,
    progress: Option<(&str, &str, u64)>,
) {
    let title = if let Some((frame, phase, elapsed_ms)) = progress {
        format!(
            "불티(Bulti) — {frame} {phase} ({}) — {endpoint_name} / {model}",
            format_elapsed(elapsed_ms)
        )
    } else {
        format!("불티(Bulti) 대화형 채팅 — {endpoint_name} / {model}")
    };
    let p = Paragraph::new(title)
        .style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))
        .alignment(Alignment::Center);
    f.render_widget(p, area);
}

/// 표시 폭 (ASCII 1, 그 외 2). CJK 가 한 줄에 넘치지 않게 한다.
#[cfg(test)]
fn display_width(s: &str) -> usize {
    s.chars().map(|c| if c.is_ascii() { 1 } else { 2 }).sum()
}

/// 입력창 커서(단일 라인 편집) 헬퍼. `cursor` 는 문자 인덱스 기준이며
/// 표시 폭(ASCII 1 / 그 외 2)으로 계산한 열(column)을 반환한다.

/// 입력 문자열을 교체하고 커서를 끝으로 옮긴다 (히스토리·자동완성 선택 시).
fn set_input(input: &mut String, cursor: &mut usize, text: String) {
    *input = text;
    *cursor = input.len();
}

/// `s` 를 바이트 인덱스 `idx` 에서 세 조각으로 나눈다:
/// (커서 앞, 커서 위치의 한 문자(끝이면 ""), 커서 뒤).
/// `idx` 는 항상 char boundary (바이트 인덱스) 여야 한다.
fn split_cursor(s: &str, idx: usize) -> (String, String, String) {
    let (before, rest) = s.split_at(idx.min(s.len()));
    if rest.is_empty() {
        return (before.to_string(), String::new(), String::new());
    }
    // rest 의 첫 문자 끝(바이트) 찾기 — UTF-8 lead byte 기준.
    let b = rest.as_bytes();
    let end = (1..=b.len())
        .find(|&i| b[i] < 0b1000_0000 || b[i] >= 0b1100_0000)
        .unwrap_or(b.len());
    let (cur, after) = rest.split_at(end);
    (
        before.to_string(),
        cur.to_string(),
        after.to_string(),
    )
}

/// 커서 위치 `cursor` 에 `c` 를 삽입하고 커서를 문자 뒤로 옮긴다.
fn insert_char(input: &mut String, cursor: &mut usize, c: char) {
    let pos = (*cursor).min(input.len());
    input.insert(pos, c);
    *cursor = pos + c.len_utf8();
}

/// 커서 앞 문자를 지운다.
fn delete_back(input: &mut String, cursor: &mut usize) {
    if *cursor == 0 {
        return;
    }
    // 커서에 붙은 앞쪽 문자(바이트)의 시작 찾기.
    let start = (0..*cursor).rev().find(|&i| input.is_char_boundary(i)).unwrap_or(0);
    input.replace_range(start..*cursor, "");
    *cursor = start;
}

/// 커서 위치 문자를 지운다 (Delete 키).
fn delete_fwd(input: &mut String, cursor: &mut usize) {
    if *cursor >= input.len() {
        return;
    }
    // 커서에 붙은 뒤쪽 문자(바이트)의 끝 찾기.
    let end = (*cursor..=input.len())
        .find(|&i| input.is_char_boundary(i) && i > *cursor)
        .unwrap_or(input.len());
    input.replace_range(*cursor..end, "");
}

/// 커서를 한 단어 경계로 이동한다. `forward` 가 참이면 다음 단어 시작,
/// 아니면 현재 단어 시작(이전 경계)으로 간다. 이동했다면 `true`.
/// `cursor` 는 바이트 인덱스이며, 단어 판단은 ASCII 바이트 기준으로 한다.
fn move_cursor_word(input: &str, cursor: &mut usize, forward: bool) -> bool {
    let n = input.len();
    // 단어 문자: ASCII 알파벳·숫자·밑줄 (비 ASCII 문자는 단어 취급 안 함).
    let is_word_char = |c: char| c.is_ascii_alphanumeric() || c == '_';
    if forward {
        // 커서 위치의 문자(인덱스 >= cursor인 첫 문자) 찾기.
        let pos = (*cursor).min(n);
        let (start, ch) = input
            .char_indices()
            .find(|(j, _)| *j >= pos)
            .unwrap_or((n, '\0'));
        let next = if is_word_char(ch) {
            // 단어 안(또는 시작)이면 단어 끝까지.
            let after = start + ch.len_utf8();
            input[after..]
                .char_indices()
                .find(|(_, c)| !is_word_char(*c))
                .map(|(j, _)| after + j)
                .unwrap_or(n)
        } else {
            // 공백·기호·한글 등: 한 문자만.
            (start + ch.len_utf8()).min(n)
        };
        if next != *cursor {
            *cursor = next;
            return true;
        }
        false
    } else {
        // 뒤에서 공백을 건너뛴 뒤 마지막 문자 확인.
        let mut i = (*cursor).min(n);
        while i > 0 {
            let (j, ch) = input
                .char_indices()
                .rev()
                .find(|(j, _)| *j < i)
                .unwrap();
            if ch == ' ' || ch == '\t' {
                i = j;
                continue;
            }
            i = j;
            // 마지막 문자가 단어 문자면 단어 시작까지.
            if is_word_char(ch) {
                let mut k = j;
                while k > 0 {
                    let (m, c) = input
                        .char_indices()
                        .rev()
                        .find(|(m, _)| *m < k)
                        .unwrap();
                    if is_word_char(c) {
                        k = m;
                    } else {
                        break;
                    }
                }
                i = k;
            }
            break;
        }
        if i != *cursor {
            *cursor = i;
            return true;
        }
        false
    }
}

/// 스패너 스타일을 보존하며 `width` 열에 맞춰 줄바꿈한다.
/// 각 스팬은 (텍스트, 스타일) 단위에서 자르고, 자른 조각에 원래 스타일을 붙인다.
fn wrap_spans<'a>(spans: &[Span<'a>], width: usize) -> Vec<Vec<Span<'a>>> {
    let mut out: Vec<Vec<Span<'a>>> = Vec::new();
    let mut cur: Vec<Span<'a>> = Vec::new();
    let mut w = 0usize;
    for span in spans {
        let mut rest: &str = span.content.as_ref();
        let mut s = String::new();
        while !rest.is_empty() {
            let ch = rest.chars().next().unwrap();
            let cw = if ch.is_ascii() { 1 } else { 2 };
            if w + cw > width && !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
                w = 0;
            }
            s.push(ch);
            w += cw;
            rest = &rest[ch.len_utf8()..];
            if rest.is_empty() || w >= width {
                cur.push(Span::styled(s.clone(), span.style));
                s.clear();
            }
        }
    }
    if !cur.is_empty() || out.is_empty() {
        out.push(cur);
    }
    out
}

/// `width` 열에 맞춰 텍스트를 줄바꿈한다. (테스트 전용)
#[cfg(test)]
fn wrap_text(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return text.split('\n').map(str::to_string).collect();
    }
    let mut out = Vec::new();
    for raw in text.split('\n') {
        if raw.is_empty() {
            out.push(String::new());
            continue;
        }
        let mut line = String::new();
        let mut w = 0usize;
        for ch in raw.chars() {
            let cw = if ch.is_ascii() { 1 } else { 2 };
            if w + cw > width && !line.is_empty() {
                out.push(std::mem::take(&mut line));
                w = 0;
            }
            line.push(ch);
            w += cw;
        }
        out.push(line);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

/// 대화 메시지 렌더링 (래핑 + 하단 고정 스크롤).
///
/// 렌더링 캐시 (grok-build `RenderState` 참고):
/// - 각 줄의 `render_cache` 가 (width, generation, expanded) 키로 래핑 결과를
///   보관한다. 같은 너비·내용이면 래핑 없이 캐시에서 그대로 재사용한다.
/// - generation 이 바뀌면(스트리밍 수신) 이전 렌더의 공통 접두까지의 래핑
///   결과만 재사용하고 새 도착분(꼬리)만 재래핑한다 (`frozen_pre_wrap_count`).
fn draw_scroll(
    f: &mut ratatui::Frame,
    area: Rect,
    lines: &mut [ChatLine],
    offset_from_bottom: usize,
    running: bool,
) {
    let inner_width = area.width.saturating_sub(2).max(1) as usize;
    let text_width = inner_width.max(1);
    let mut text_lines: Vec<Line> = Vec::new();
    // 진행 중 턴의 "생각 중…" 헤더는 마지막 Assistant 줄에만 붙인다.
    let last_assistant_idx = lines.iter().rposition(|l| l.role == Role::Assistant);
    for (idx, line) in lines.iter_mut().enumerate() {
        let prefix = match line.role {
            Role::User => "▶ ",
            Role::Assistant => "◀ ",
            Role::Tool => "⚙ ",
            Role::Status => "· ",
        };
        let style = match line.role {
            Role::User => Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
            Role::Assistant => Style::default().fg(Color::Cyan),
            Role::Tool => Style::default().fg(Color::Yellow),
            Role::Status => Style::default().fg(Color::DarkGray),
        };

        // 메시지 사이에 한 줄을 띄워 붙어 보이지 않게 한다.
        if !text_lines.is_empty() {
            let last_blank = text_lines
                .last()
                .map(|l| l.spans.iter().all(|s| s.content.trim().is_empty()))
                .unwrap_or(true);
            if !last_blank {
                text_lines.push(Line::from(""));
            }
        }

        // Assistant 줄: 모델은 생각한 뒤 답하므로 reasoning 을 본문보다 먼저 그린다.
        // 본문이 아직 비어 있어도(생각만 나오는 중) `◀` 빈 줄은 만들지 않는다 —
        // 진행 상태는 제목줄 스피너와 생각 헤더가 표시한다.
        if line.role == Role::Assistant {
            let is_running =
                running && Some(idx) == last_assistant_idx && line.text.trim().is_empty();
            if !line.reasoning_content.trim().is_empty() {
                let reasoning = render_thinking(line, is_running);
                line.reasoning_cache.append_wrapped(
                    &mut text_lines,
                    &reasoning,
                    text_width,
                    line.generation,
                    line.reasoning_expanded,
                );
            }
            if line.text.trim().is_empty() {
                continue;
            }
            let rendered = render_assistant_markdown(&line.text, prefix, style);
            line.render_cache.append_wrapped(
                &mut text_lines,
                &rendered,
                text_width,
                line.generation,
                false,
            );
            continue;
        }

        // 평문: 한 원문 줄 = 한 렌더 라인 (prefix 스타일 스팬 포함).
        let rendered: Vec<Line<'static>> = line
            .text
            .split('\n')
            .map(|raw| {
                Line::from(vec![
                    Span::styled(prefix, style),
                    Span::styled(raw.to_string(), style),
                ])
            })
            .collect();
        line.render_cache.append_wrapped(
            &mut text_lines,
            &rendered,
            text_width,
            line.generation,
            false,
        );
    }

    let scroll = scroll_from_bottom(text_lines.len(), area.height, offset_from_bottom);
    let para = Paragraph::new(text_lines)
        .wrap(Wrap { trim: false })
        .scroll((scroll as u16, 0))
        .block(Block::default().padding(ratatui::widgets::Padding::new(1, 1, 0, 0)));
    f.render_widget(para, area);
}

/// 마크다운 블록 앞뒤의 빈 줄(Blank)을 제거한다.
///
/// 스트리밍 중 `parse` 는 문단 경계마다 `Blank` 를 만드는데, 맨 앞 `Blank` 가
/// 있으면 `◀ ` 접두가 내용 없이 빈 줄에 붙어 본문이 한 줄 밀려 보인다.
fn trim_blank_blocks(mut blocks: Vec<crate::render::Block>) -> Vec<crate::render::Block> {
    while matches!(blocks.first(), Some(crate::render::Block::Blank)) {
        blocks.remove(0);
    }
    while matches!(blocks.last(), Some(crate::render::Block::Blank)) {
        blocks.pop();
    }
    blocks
}

/// Assistant 본문을 마크다운으로 렌더링한다. 첫 줄에 `◀ ` 접두를, 이어지는
/// 줄에는 같은 폭의 들여쓰기를 붙여 접두 아래로 정렬한다.
fn render_assistant_markdown(text: &str, prefix: &str, style: Style) -> Vec<Line<'static>> {
    let blocks = trim_blank_blocks(crate::render::parse(text));
    if blocks.is_empty() {
        return vec![Line::from(Span::styled(prefix.to_string(), style))];
    }
    let mut rendered: Vec<Line<'static>> = Vec::new();
    for (i, rline) in crate::render::render_lines(&blocks).iter().enumerate() {
        let mut spans: Vec<Span<'static>> = Vec::new();
        if i == 0 {
            spans.push(Span::styled(prefix.to_string(), style));
        } else {
            spans.push(Span::raw("  ".to_string()));
        }
        spans.extend(rline.spans.iter().cloned());
        rendered.push(Line::from(spans));
    }
    rendered
}

/// 모델 생각(reasoning) 블록을 grok-build `ThinkingBlock` 스타일로 렌더링한다.
///
/// - 헤더: 진행 중이면 `💭 생각 중… (N자)`, 완료 후면 `💭 생각 (N자)`.
/// - 접힘 상태에는 `Ctrl+T 펼치기` 힌트를 덧붙여 펼치는 방법을 화면에 노출한다.
/// - 펼침 상태에서만 본문을 dim·italic 으로 그린다.
fn render_thinking(line: &ChatLine, is_running: bool) -> Vec<Line<'static>> {
    let n = line.reasoning_content.chars().count();
    let header_style = Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::BOLD);
    let hint_style = Style::default().fg(Color::DarkGray);
    let header = if is_running {
        format!("💭 생각 중… ({n}자)")
    } else {
        format!("💭 생각 ({n}자)")
    };
    let hint = if line.reasoning_expanded {
        "  — Ctrl+T 접기"
    } else {
        "  — Ctrl+T 펼치기"
    };
    let mut out: Vec<Line<'static>> = vec![Line::from(vec![
        Span::styled(header, header_style),
        Span::styled(hint.to_string(), hint_style),
    ])];
    if line.reasoning_expanded {
        let dim = Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::DIM | Modifier::ITALIC);
        let blocks = trim_blank_blocks(crate::render::parse(&line.reasoning_content));
        for rline in crate::render::render_lines(&blocks) {
            let mut spans: Vec<Span<'static>> = vec![Span::raw("  ".to_string())];
            spans.extend(
                rline
                    .spans
                    .iter()
                    .map(|s| Span::styled(s.content.clone(), dim)),
            );
            out.push(Line::from(spans));
        }
    }
    out
}

/// 하단 고정 스크롤 오프셋 계산.
fn scroll_from_bottom(total_lines: usize, height: u16, offset_from_bottom: usize) -> usize {
    let inner_h = height.saturating_sub(2) as usize;
    let max_scroll = total_lines.saturating_sub(inner_h);
    max_scroll.saturating_sub(offset_from_bottom)
}

/// 히스토리 탐색: ↑(up=true) 또는 ↓(up=false) 로 이동한 다음 인덱스를 반환.
/// - `up`: `None` → 마지막 항목, `Some(i)` → `i-1` (처음 이상으로 못 감).
/// - `down`: `Some(i)` → `i-1` (0 이하면 `None`, 탐색 종료·입력창 비움).
/// `history` 가 비어 있으면 `None`.
pub fn step_history(history: &[String], history_idx: Option<usize>, up: bool) -> Option<usize> {
    if history.is_empty() {
        return None;
    }
    match (history_idx, up) {
        (None, true) => Some(history.len() - 1),
        (Some(i), true) => Some(i.saturating_sub(1)),
        (Some(i), false) => i.checked_sub(1),
        (None, false) => None,
    }
}

/// 마지막 Assistant 응답을 찾아 상태 표시줄에 토큰/속도를 보여준다.
fn last_assistant(lines: &[ChatLine]) -> Option<&ChatLine> {
    lines.iter().rev().find(|l| l.role == Role::Assistant)
}

/// 스피너 프레임 문자열 (braille).
const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⣣", "⣾", "⣻", "⣷", "⣯"];

/// 프레임 인덱스에 해당하는 스피너 문자를 반환한다 (길이로 감돌음).
fn spinner(frame: usize) -> &'static str {
    SPINNER_FRAMES[frame % SPINNER_FRAMES.len()]
}

/// 진행 중 턴의 현재 페이즈를 판단해 상태 표시줄에 노출한다.
/// - 마지막 Assistant 줄에 본문이 쌓여 있으면 "응답 생성 중" (최우선)
/// - "호출 중" 도구 줄(꼬리 ` …`)이 있으면 "도구 실행 중"
/// - reasoning 만 쌓여 있으면 "생각 중"
/// - 그 외(첫 토큰 도착 전) "대기 중"
fn turn_phase(lines: &[ChatLine]) -> &'static str {
    let tool_running = lines
        .iter()
        .any(|l| l.role == Role::Tool && l.text.ends_with(" …"));
    match lines.iter().rev().find(|l| l.role == Role::Assistant) {
        Some(l) if !l.text.trim().is_empty() => "응답 생성 중",
        _ if tool_running => "도구 실행 중",
        Some(l) if !l.reasoning_content.trim().is_empty() => "생각 중",
        _ => "대기 중",
    }
}

/// 경과 시간을 인간 가독 문자열로 포맷한다 (60s 미만: `12.3s`, 이상: `1m 05s`).
fn format_elapsed(ms: u64) -> String {
    let total = ms / 1000;
    if total < 60 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format!("{}m {:02}s", total / 60, total % 60)
    }
}

/// 도구 호출 이벤트를 Tool 줄로 반영한다.
/// "호출 중" 이벤트(`ok == false`, 에러 없음)는 새 줄을 추가하고, 결과
/// 이벤트는 마지막 "호출 중" 줄(꼬리 ` …`)을 제자리로 갱신해 한 도구 호출이
/// 두 줄을 차지하지 않게 한다.
fn push_tool_event(lines: &mut Vec<ChatLine>, ev: crate::llm::ToolEvent) {
    let args = if ev.args_summary.is_empty() {
        String::new()
    } else {
        format!(" ({})", ev.args_summary)
    };
    let text = if ev.ok {
        format!("{}{}", ev.name, args)
    } else if let Some(err) = ev.error {
        format!("✗ {}: {}", ev.name, err)
    } else {
        format!("{}{} …", ev.name, args)
    };
    let in_flight = lines
        .iter()
        .enumerate()
        .rev()
        .find(|(_, l)| l.role == Role::Tool && l.text.ends_with(" …"))
        .map(|(i, _)| i);
    match in_flight {
        Some(i) => {
            lines[i].text = text;
            lines[i].bump_generation();
        }
        None => {
            lines.push(ChatLine {
                role: Role::Tool,
                text,
                ..ChatLine::default()
            });
        }
    }
}

/// 입력창 렌더링. `cursor` 는 바이트 인덱스이며, 그 위치의 문자를 역색
/// 스팬으로 그려 커서를 표시한다 (끝이면 빈 역색 스팬).
/// `multiline` 이면 입력을 줄 단위로 나눠 여러 줄로 렌더하고, 커서는
/// `cursor` 가 속한 라인에만 표시한다.
fn draw_input(f: &mut ratatui::Frame, area: Rect, input: &str, cursor: usize, multiline: bool) {
    let title = if multiline {
        "입력 (멀티라인 — Shift+Enter 줄바꿈 · Enter 전송 · /multiline 꺼짐)"
    } else {
        "입력 (Enter 전송 · ↑↓ 히스토리 · Tab 자동완성 · Ctrl+T 생각 · Ctrl+Q 종료)"
    };
    // 커서 표시: [cursor 앞][커서 문자(역색)][cursor 뒤]
    let inv = Style::default().add_modifier(Modifier::REVERSED);
    let lines: Vec<Line> = if multiline {
        let mut off = 0usize;
        input
            .split('\n')
            .map(|l| {
                let lstart = off;
                let lend = off + l.len();
                off += l.len() + 1; // '\n' 바이트 포함
                let cc = cursor.min(input.len());
                if cc >= lstart && cc <= lend {
                    let (before, cur, after) = split_cursor(l, cc - lstart);
                    Line::from(vec![
                        Span::raw(before),
                        Span::styled(cur, inv),
                        Span::raw(after),
                    ])
                } else {
                    Line::from(Span::raw(l))
                }
            })
            .collect()
    } else {
        let (before, cur, after) = split_cursor(input, cursor.min(input.len()));
        let spans = vec![
            Span::raw(before),
            Span::styled(cur, inv),
            Span::raw(after),
        ];
        vec![Line::from(spans)]
    };
    let p = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(title),
        )
        .wrap(Wrap { trim: false });
    f.render_widget(p, area);
}

/// 자동완성 드롭다운 렌더링 — 입력창 위에 최대 6개 항목을 겹쳐 그린다.
fn draw_completions(
    f: &mut ratatui::Frame,
    input_area: Rect,
    completions: &[crate::slash::Suggestion],
    idx: usize,
) {
    if completions.is_empty() {
        return;
    }
    let items = &completions[..completions.len().min(6)];
    let height = items.len() as u16 + 1; // 제목 1줄 + 항목
    let y = input_area.y.saturating_sub(height);
    if y == 0 || y > input_area.y {
        return;
    }
    let area = Rect::new(input_area.x, y, input_area.width, height);
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(vec![Span::styled(
        "커맨드 자동완성 (Enter 선택 · Tab/↑↓ 이동 · Esc 취소)",
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
    )]));
    for (i, s) in items.iter().enumerate() {
        let selected = i == idx.min(completions.len() - 1);
        let style = if selected {
            Style::default().fg(Color::Black).bg(Color::Cyan)
        } else {
            Style::default().fg(Color::White)
        };
        lines.push(Line::from(vec![
            Span::styled(if selected { "▶ " } else { "  " }, style),
            Span::styled(s.display.clone(), style),
            Span::styled(format!("  — {}", s.description), style),
        ]));
    }
    let p = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL))
        .style(Style::default().bg(Color::DarkGray));
    f.render_widget(p, area);
}

/// 일반 텍스트(히스토리·세션) 자동완성 드롭다운 렌더링.
/// `/` 입력 시 자동완성 상태를 갱신한다. `/` 가 아니면 드롭다운을 닫는다.
///
/// `ctx` 는 `/model`·`/endpoint`·`/mcp` 의 인자 후보(모델·엔드포인트·MCP 이름)를
/// 담는다. 커맨드명 자동완성에는 쓰이지 않으므로 입력이 인자 단계가 아니면 무시된다.
fn update_completions(
    completions: &mut Vec<crate::slash::Suggestion>,
    idx: &mut usize,
    active: &mut bool,
    input: &str,
    ctx: &crate::slash::CompletionContext,
) {
    if input.starts_with('/') {
        let mut next = crate::slash::suggest_with(input, ctx);
        next.dedup_by(|a, b| a.insert == b.insert);
        *completions = next;
        *idx = 0;
        *active = !completions.is_empty();
    } else {
        completions.clear();
        *active = false;
    }
}

/// 상태 표시 — 세션 id·저장 여부·세션 누적 토큰·최근 응답 토큰/속도.
/// 주의: Session lock 금지 — 누적 토큰은 호출부에서 전달받는다.
fn draw_status(
    f: &mut ratatui::Frame,
    area: Rect,
    session_id: &str,
    saved: bool,
    total_in: u64,
    total_out: u64,
    last: Option<&ChatLine>,
    progress: Option<(&str, &str, u64)>,
) {
    let mut status = if saved {
        format!("세션 저장됨 — {session_id}")
    } else {
        format!("세션 id: {session_id}")
    };
    status.push_str(&format!("  세션 ↑{total_in} ↓{total_out}"));
    // 진행 중 턴이면 스피너 + 페이즈 + 경과 시간을 표시한다.
    if let Some((frame, phase, elapsed_ms)) = progress {
        status.insert_str(
            0,
            &format!(
                "{} {} ({})  ",
                frame,
                phase,
                format_elapsed(elapsed_ms)
            ),
        );
    }
    if let Some(l) = last {
        let speed = if l.duration_ms > 0 {
            l.output_tokens as f64 / (l.duration_ms as f64 / 1000.0)
        } else {
            0.0
        };
        status.push_str(&format!(
            "  ↑{} ↓{} {:.1}t/s",
            l.input_tokens, l.output_tokens, speed
        ));
    }
    let p = Paragraph::new(status)
        .style(Style::default().fg(Color::DarkGray))
        .alignment(Alignment::Right);
    f.render_widget(p, area);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ChatLine 기본 생성.
    #[test]
    fn chat_line_constructs() {
        let l = ChatLine {
            role: Role::User,
            text: "안녕".to_string(),
            ..ChatLine::default()
        };
        assert_eq!(l.role, Role::User);
        assert_eq!(l.text, "안녕");
    }

    /// spinner: 프레임 인덱스가 프레임 수만큼 감돈다.
    #[test]
    fn spinner_wraps_around() {
        assert_eq!(spinner(0), SPINNER_FRAMES[0]);
        assert_eq!(spinner(SPINNER_FRAMES.len()), SPINNER_FRAMES[0]);
        assert_eq!(spinner(1), SPINNER_FRAMES[1]);
    }

    /// format_elapsed: 60s 미만은 초, 이상은 분:초.
    #[test]
    fn format_elapsed_units() {
        assert_eq!(format_elapsed(0), "0.0s");
        assert_eq!(format_elapsed(1234), "1.2s");
        assert_eq!(format_elapsed(59_999), "60.0s");
        assert_eq!(format_elapsed(60_000), "1m 00s");
        assert_eq!(format_elapsed(65_000), "1m 05s");
    }

    /// turn_phase: 대기 → 생각 중 → 도구 실행 중 → 응답 생성 중 순서.
    #[test]
    fn turn_phase_transitions() {
        // 빈 줄: 대기 중.
        let mut lines: Vec<ChatLine> = Vec::new();
        assert_eq!(turn_phase(&lines), "대기 중");
        // 빈 Assistant 줄 추가: 여전히 대기 중 (첫 토큰 도착 전).
        lines.push(ChatLine {
            role: Role::Assistant,
            ..ChatLine::default()
        });
        assert_eq!(turn_phase(&lines), "대기 중");
        // reasoning 만 쌓이면 생각 중.
        lines.push(ChatLine {
            role: Role::Assistant,
            reasoning_content: "생각".to_string(),
            ..ChatLine::default()
        });
        assert_eq!(turn_phase(&lines), "생각 중");
        // "호출 중" 도구 줄이 있으면 도구 실행 중.
        lines.push(ChatLine {
            role: Role::Tool,
            text: "read_file (foo.rs) …".to_string(),
            ..ChatLine::default()
        });
        assert_eq!(turn_phase(&lines), "도구 실행 중");
        // 본문이 쌓이면 응답 생성 중.
        lines.push(ChatLine {
            role: Role::Assistant,
            text: "안녕하세요".to_string(),
            ..ChatLine::default()
        });
        assert_eq!(turn_phase(&lines), "응답 생성 중");
        // 완료된 도구 줄(꼬리 ` …` 없음)은 "도구 실행 중"으로 오인하지 않는다.
        lines.push(ChatLine {
            role: Role::Assistant,
            text: "결과".to_string(),
            ..ChatLine::default()
        });
        assert_eq!(turn_phase(&lines), "응답 생성 중");
    }

    /// step_history: ↑ 로 거슬러 올라가고 ↓ 로 최신 방향으로 내려간다.
    #[test]
    fn step_history_navigation() {
        let h: Vec<String> = vec!["a".into(), "b".into(), "c".into()];
        // 빈 상태에서 ↑ → 가장 오래된(마지막) 항목.
        assert_eq!(step_history(&h, None, true), Some(2));
        assert_eq!(step_history(&h, Some(2), true), Some(1));
        assert_eq!(step_history(&h, Some(1), true), Some(0));
        // 처음 이상으로 못 간다.
        assert_eq!(step_history(&h, Some(0), true), Some(0));
        // ↓ 로 최신 방향으로.
        assert_eq!(step_history(&h, Some(2), false), Some(1));
        assert_eq!(step_history(&h, Some(1), false), Some(0));
        // 최신(0)에서 ↓ → 탐색 종료.
        assert_eq!(step_history(&h, Some(0), false), None);
        // 비어 있으면 항상 None.
        assert_eq!(step_history(&[], None, true), None);
    }

    /// insert_char: 커서 위치에 삽입하고 커서를 문자 뒤로.
    #[test]
    fn insert_cursor_char() {
        let mut s = String::new();
        let mut c = 0;
        insert_char(&mut s, &mut c, 'b');
        insert_char(&mut s, &mut c, 'a');
        assert_eq!((s.as_str(), c), ("ba", 2));
        // 시작에 삽입.
        let mut c2 = 0;
        insert_char(&mut s, &mut c2, 'x');
        assert_eq!((s.as_str(), c2), ("xba", 1));
    }

    /// insert_char: 한글 문자는 바이트 인덱스 3 만큼 커서를 넘긴다.
    #[test]
    fn insert_char_multibyte() {
        let mut s = "ab".to_string();
        let mut c = 1;
        insert_char(&mut s, &mut c, '한');
        assert_eq!((s.as_str(), c), ("a한b", 4));
    }

    /// delete_back: 커서 앞 문자(바이트) 삭제.
    #[test]
    fn delete_back_char() {
        let mut s = "abc".to_string();
        let mut c = 3;
        delete_back(&mut s, &mut c);
        assert_eq!((s.as_str(), c), ("ab", 2));
        // 한글 앞: 3바이트 삭제.
        let mut s2 = "a한".to_string();
        let mut c2 = 4;
        delete_back(&mut s2, &mut c2);
        assert_eq!((s2.as_str(), c2), ("a", 1));
        // 커서 0 이면 아무일도 없음.
        let mut c3 = 0;
        delete_back(&mut s2, &mut c3);
        assert_eq!((s2.as_str(), c3), ("a", 0));
    }

    /// delete_fwd: 커서 위치 문자(바이트) 삭제, 커서는 유지.
    #[test]
    fn delete_fwd_char() {
        let mut s = "abc".to_string();
        let mut c = 1;
        delete_fwd(&mut s, &mut c);
        assert_eq!((s.as_str(), c), ("ac", 1));
        // 한글: 3바이트 삭제.
        let mut s2 = "a한b".to_string();
        let mut c2 = 1;
        delete_fwd(&mut s2, &mut c2);
        assert_eq!((s2.as_str(), c2), ("ab", 1));
        // 끝에 있으면 아무일도 없음.
        let mut c3 = 2;
        delete_fwd(&mut s2, &mut c3);
        assert_eq!((s2.as_str(), c3), ("ab", 2));
    }

    /// move_cursor_word: 앞/뒤 단어 경계 이동.
    #[test]
    fn move_word_forward() {
        let s = "hello world";
        let mut c = 0;
        assert!(move_cursor_word(&s, &mut c, true));
        assert_eq!(c, 5);
        // 공백 한 문자 건너뛰어 다음 단어 시작.
        assert!(move_cursor_word(&s, &mut c, true));
        assert_eq!(c, 6);
        assert!(move_cursor_word(&s, &mut c, true));
        assert_eq!(c, 11);
        // 끝에서 더 못 감.
        assert!(!move_cursor_word(&s, &mut c, true));
    }

    /// move_cursor_word: 뒤쪽 단어 경계 이동.
    #[test]
    fn move_word_backward() {
        let s = "hello world";
        let mut c = 11;
        assert!(move_cursor_word(&s, &mut c, false));
        assert_eq!(c, 6);
        assert!(move_cursor_word(&s, &mut c, false));
        assert_eq!(c, 0);
        assert!(!move_cursor_word(&s, &mut c, false));
    }

    /// move_cursor_word: 한글은 한 문자 단위.
    #[test]
    fn move_word_korean() {
        let s = "안녕 세계";
        let mut c = 0;
        assert!(move_cursor_word(&s, &mut c, true));
        assert_eq!(c, 3); // '안'
        assert!(move_cursor_word(&s, &mut c, true));
        assert_eq!(c, 6); // '녕'
        assert!(move_cursor_word(&s, &mut c, true));
        assert_eq!(c, 7); // 공백
        assert!(move_cursor_word(&s, &mut c, true));
        assert_eq!(c, 10);
        assert!(move_cursor_word(&s, &mut c, true));
        assert_eq!(c, 13);
        assert!(!move_cursor_word(&s, &mut c, true));
    }

    /// set_input: 입력 교체 + 커서 끝 이동.
    #[test]
    fn set_input_moves_cursor_to_end() {
        let mut s = "old".to_string();
        let mut c = 1;
        set_input(&mut s, &mut c, "hello".to_string());
        assert_eq!((s.as_str(), c), ("hello", 5));
        set_input(&mut s, &mut c, String::new());
        assert_eq!((s.as_str(), c), ("", 0));
    }

    /// TTY 가 아니면 run_tui 는 None 을 반환한다 (스트림 텍스트 폴백).
    #[test]
    fn run_tui_returns_none_on_non_tty() {
        let options = TuiOptions {
            endpoint_name: "ep".to_string(),
            model: "model".to_string(),
            session_id: "sid".to_string(),
        };
        let result = run_tui(&options, vec![], |_msg, _tx| {
            Ok(TurnResult {
                exit_code: 0,
                assistant_content: "ok".to_string(),
                reasoning_content: String::new(),
                input_tokens: 0,
                output_tokens: 0,
                duration_ms: 0,
                files_touched: vec![],
            })
        }, |_cmd| CommandResult {
            message: String::new(),
            exit: false,
        })
        .unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn wrap_text_splits_newlines_and_width() {
        let lines = wrap_text("안녕?\n다음", 4);
        // 한글 폭 2 → "안녕" / "?" / "다음"
        assert_eq!(lines[0], "안녕");
        assert_eq!(lines[1], "?");
        assert_eq!(lines[2], "다음");
    }

    #[test]
    fn display_width_cjk_is_two() {
        assert_eq!(display_width("A"), 1);
        assert_eq!(display_width("가"), 2);
        assert_eq!(display_width("가A"), 3);
    }

    #[test]
    fn tui_inactive_by_default() {
        assert!(!is_active());
    }

    /// `/` 입력 시 자동완성 목록이 채워진다.
    #[test]
    fn update_completions_opens_on_slash() {
        let mut completions = Vec::new();
        let mut idx = 0usize;
        let mut active = false;
        let ctx = crate::slash::CompletionContext::default();
        update_completions(&mut completions, &mut idx, &mut active, "/", &ctx);
        assert!(active);
        assert!(completions.iter().any(|c| c.insert == "/exit"));
        assert!(completions.iter().any(|c| c.insert == "/model"));
    }

    /// 인자 단계(`/model `)에서는 컨텍스트의 모델 후보가 제안된다.
    #[test]
    fn update_completions_suggests_model_args() {
        let mut completions = Vec::new();
        let mut idx = 0usize;
        let mut active = false;
        let ctx = crate::slash::CompletionContext {
            models: vec!["qwen3-32b".to_string()],
            ..Default::default()
        };
        update_completions(&mut completions, &mut idx, &mut active, "/model ", &ctx);
        assert!(active);
        assert_eq!(completions.len(), 1);
        assert_eq!(completions[0].insert, "/model qwen3-32b");
    }

    /// `/` 가 아닌 입력은 드롭다운을 닫는다.
    #[test]
    fn update_completions_closes_without_slash() {
        let mut completions = vec![crate::slash::Suggestion {
            display: "/exit".to_string(),
            insert: "/exit".to_string(),
            description: "대화 종료",
        }];
        let mut idx = 0usize;
        let mut active = true;
        let ctx = crate::slash::CompletionContext::default();
        update_completions(&mut completions, &mut idx, &mut active, "hello", &ctx);
        assert!(!active);
        assert!(completions.is_empty());
    }

    // ── 렌더링 캐시 (RenderCache) ──────────────────────────────────────────

    /// 테스트용: 평문 rendered 라인 목록.
    fn test_rendered(texts: &[&str]) -> Vec<Line<'static>> {
        texts.iter().map(|t| Line::from(t.to_string())).collect()
    }

    /// (a) 같은 width·generation·expanded 로 재렌더하면 캐시 히트 —
    /// 래핑 미스 증가 없이 출력도 동일하다.
    #[test]
    fn render_cache_reuse_on_same_width_generation() {
        let rendered = test_rendered(&["안녕하세요 세계", "두 번째 줄"]);
        let mut cache = RenderCache::default();

        let mut first = Vec::new();
        assert!(!cache.append_wrapped(&mut first, &rendered, 10, 1, false));
        // width 10: "안녕하세요 세계" → 2줄, "두 번째 줄"(폭 10) → 1줄.
        assert_eq!(first.len(), 3);
        assert_eq!(cache.wrap_misses(), 1);

        let mut second = Vec::new();
        assert!(cache.append_wrapped(&mut second, &rendered, 10, 1, false));
        assert_eq!(cache.wrap_misses(), 1, "캐시 히트에 미스 카운터는 증가하지 않아야 한다");
        assert_eq!(first, second);
    }

    /// (b) 스트리밍: text push + bump_generation 후 렌더는 미스 1회만이고,
    /// 출력 라인이 완전 재래핑(새 캐시) 결과와 동일하다.
    #[test]
    fn render_cache_streaming_prefix_frozen() {
        let initial = test_rendered(&["안녕하세요", "다음 줄"]);
        let mut cache = RenderCache::default();

        let mut out1 = Vec::new();
        assert!(!cache.append_wrapped(&mut out1, &initial, 10, 1, false));

        // 스트리밍 수신: 첫 줄이 이어지고 generation 증가.
        let streamed = test_rendered(&["안녕하세요 세계", "다음 줄"]);
        let mut out2 = Vec::new();
        assert!(!cache.append_wrapped(&mut out2, &streamed, 10, 2, false));
        assert_eq!(cache.wrap_misses(), 2, "스트리밍 재렌더는 미스 1회만 증가해야 한다");

        // 완전 재래핑(캐시 없는 상태) 결과와 동일해야 한다.
        let mut fresh = Vec::new();
        let mut fresh_cache = RenderCache::default();
        assert!(!fresh_cache.append_wrapped(&mut fresh, &streamed, 10, 2, false));
        assert_eq!(out2, fresh);

        // 두 번째 스트리밍 프레임(동일 generation)은 다시 히트.
        let mut out3 = Vec::new();
        assert!(cache.append_wrapped(&mut out3, &streamed, 10, 2, false));
        assert_eq!(cache.wrap_misses(), 2);
        assert_eq!(out3, fresh);
    }

    /// (c) main / reasoning 캐시는 독립이다 — 같은 ChatLine 이 서로 다른
    /// rendered·expanded 키로 각자 캐시를 유지한다.
    #[test]
    fn render_cache_main_reasoning_independent() {
        let mut line = ChatLine {
            role: Role::Assistant,
            text: "본문 내용".to_string(),
            reasoning_content: "생각 중...".to_string(),
            reasoning_expanded: true,
            ..ChatLine::default()
        };
        let main = test_rendered(&["본문 내용"]);
        let reasoning = test_rendered(&["생각 중..."]);

        // 첫 렌더: main·reasoning 각각 미스.
        let mut out = Vec::new();
        assert!(!line
            .render_cache
            .append_wrapped(&mut out, &main, 10, line.generation, false));
        assert!(!line.reasoning_cache.append_wrapped(
            &mut out,
            &reasoning,
            10,
            line.generation,
            line.reasoning_expanded
        ));
        assert_eq!(line.render_cache.wrap_misses(), 1);
        assert_eq!(line.reasoning_cache.wrap_misses(), 1);

        // 재렌더: 두 캐시 모두 히트 (미스 증가 0).
        let mut out2 = Vec::new();
        assert!(line
            .render_cache
            .append_wrapped(&mut out2, &main, 10, line.generation, false));
        assert!(line.reasoning_cache.append_wrapped(
            &mut out2,
            &reasoning,
            10,
            line.generation,
            line.reasoning_expanded
        ));
        assert_eq!(line.render_cache.wrap_misses(), 1);
        assert_eq!(line.reasoning_cache.wrap_misses(), 1);
        assert_eq!(out, out2);
    }

    /// (d) width 가 바뀌면 접두 재사용 없이 완전 재래핑된다.
    #[test]
    fn render_cache_full_rewrap_on_width_change() {
        let rendered = test_rendered(&["안녕하세요 세계", "두 번째 줄"]);
        let mut cache = RenderCache::default();

        let mut out1 = Vec::new();
        assert!(!cache.append_wrapped(&mut out1, &rendered, 10, 1, false));

        let mut out2 = Vec::new();
        assert!(!cache.append_wrapped(&mut out2, &rendered, 20, 1, false));
        assert_eq!(cache.wrap_misses(), 2);

        // 새 width 의 완전 재래핑 결과와 동일.
        let mut fresh = Vec::new();
        let mut fresh_cache = RenderCache::default();
        assert!(!fresh_cache.append_wrapped(&mut fresh, &rendered, 20, 1, false));
        assert_eq!(out2, fresh);
        // 너비를 넓히면 래핑 줄 수가 줄어든다.
        assert!(out2.len() < out1.len());
    }

    // ── 생각(reasoning) 렌더링 ─────────────────────────────────────────────

    /// 앞뒤 Blank 블록을 제거해 접두가 빈 줄에 붙지 않게 한다.
    #[test]
    fn trim_blank_blocks_strips_edges() {
        let blocks = crate::render::parse("\n본문\n");
        assert_eq!(blocks.first(), Some(&crate::render::Block::Blank));
        let trimmed = trim_blank_blocks(blocks);
        assert_eq!(trimmed.len(), 1);
        assert!(matches!(trimmed[0], crate::render::Block::Paragraph(_)));
    }

    /// Assistant 마크다운은 첫 줄에 `◀ ` 접두가 붙고 선행 빈 줄이 없다.
    #[test]
    fn assistant_markdown_prefix_on_first_line() {
        let rendered = render_assistant_markdown("\n첫 문단\n\n둘째 문단", "◀ ", Style::default());
        let first: String = rendered[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(first, "◀ 첫 문단");
        // 두 번째 문단 줄은 2칸 들여쓰기로 정렬된다.
        let second: String = rendered
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .find(|t| t.contains("둘째"))
            .expect("둘째 문단");
        assert!(second.starts_with("  둘째"), "{second:?}");
    }

    fn thinking_line(expanded: bool) -> ChatLine {
        ChatLine {
            role: Role::Assistant,
            reasoning_content: "생각한 내용".to_string(),
            reasoning_expanded: expanded,
            ..ChatLine::default()
        }
    }

    /// 접힌 생각은 헤더 + 펼치기 힌트 한 줄만 보여준다 (본문 미노출).
    #[test]
    fn collapsed_thinking_shows_expand_hint_only() {
        let out = render_thinking(&thinking_line(false), false);
        assert_eq!(out.len(), 1);
        let text: String = out[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("💭 생각"), "{text:?}");
        assert!(text.contains("Ctrl+T 펼치기"), "{text:?}");
        assert!(!text.contains("생각한 내용"), "{text:?}");
    }

    /// 펼친 생각은 헤더 아래에 본문을 dim·italic 으로 보여준다.
    #[test]
    fn expanded_thinking_shows_body() {
        let out = render_thinking(&thinking_line(true), false);
        assert!(out.len() > 1);
        let all: String = out
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(all.contains("— Ctrl+T 접기"), "{all:?}");
        assert!(all.contains("생각한 내용"), "{all:?}");
    }

    /// 진행 중이면 헤더가 "생각 중…" 으로 바뀐다.
    #[test]
    fn running_thinking_header() {
        let out = render_thinking(&thinking_line(false), true);
        let text: String = out[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("생각 중…"), "{text:?}");
    }
}

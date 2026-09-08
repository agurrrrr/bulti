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
use std::time::Duration;

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
    /// reasoning 접기/펼침 상태 (`t` 키로 토글, 기본 접힘).
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

    // 일반 텍스트(히스토리·세션 기반) 자동완성 상태.
    // 히스토리 후보를 미리 로드해 두고, 입력 중 fuzzy 매칭으로 후보를 뽑는다.
    let mut history_sources: Vec<String> = crate::completion::load_history_sources();
    // 일반 텍스트 자동완성 후보 (슬래시 커맨드와 별도로 관리).
    let mut text_completions: Vec<crate::completion::CompletionItem> = Vec::new();
    let mut text_completion_idx: usize = 0;
    let mut text_completion_active = false;

    // 프롬프트 히스토리 탐색 상태 (↑/↓ 키). 자동완성과 공용으로
    // `history_sources` 를 재사용한다 (최근이 앞에 정렬됨).
    let mut history_idx: Option<usize> = None;
    // 멀티라인 입력 모드: 켜져 있으면 Shift+Enter 로 줄바꿈, Enter 로 전송.
    let mut multiline = false;

    // 진행 중인 턴: 델타 수신 채널 + 작업 스레드 핸들.
    // `Some` 이면 응답 생성 중이며, TUI 루프가 채널을 폴링해 점진적으로 그린다.
    let mut pending_turn: PendingTurn = None;

    let outcome = loop {
        // 진행 중 턴의 델타를 폴링해 화면에 점진적으로 반영한다.
        // 마지막 Assistant 메시지(생성 중)에 content 를 누적한다.
        if let Some((ref mut rx, _)) = pending_turn {
            let mut got = false;
            while let Ok(delta) = rx.try_recv() {
                got = true;
                if let Some(text) = delta.content {
                    if let Some(last) = lines.last_mut() {
                        if last.role == Role::Assistant {
                            if !text.is_empty() {
                                last.text.push_str(&text);
                                last.bump_generation();
                            }
                        }
                    }
                }
                if let Some(think) = delta.reasoning_content {
                    if let Some(last) = lines.last_mut() {
                        if last.role == Role::Assistant {
                            if !think.is_empty() {
                                last.reasoning_content.push_str(&think);
                                last.bump_generation();
                            }
                        }
                    }
                }
            }
            if got {
                offset_from_bottom = 0;
            }
        }

        terminal.draw(|f| {
            let size = f.area();
            // 슬래시 드롭다운이 열려 있으면 텍스트 드롭다운은 겹치지 않게 닫는다.
            let text_dropdown_open = !completion_active && !text_completions.is_empty();
            // 멀티라인 모드면 입력창을 키운다 (최소 5줄).
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

            draw_title(f, chunks[0], &options.endpoint_name, &options.model);
            draw_scroll(f, chunks[1], &mut lines, offset_from_bottom);
            // 고스트 텍스트: 슬래시·텍스트 드롭다운이 열리지 않은 상태에서
            // 히스토리와 접두사 일치하는 최선 후보의 나머지 부분.
            let ghost = if completion_active || text_dropdown_open || multiline {
                String::new()
            } else {
                crate::completion::best_ghost(&input, &history_sources).unwrap_or_default()
            };
            draw_input(f, chunks[2], &input, cursor, &ghost, multiline);
            draw_completions(f, chunks[2], &completions, completion_idx);
            draw_text_completions(f, chunks[2], &text_completions, text_completion_idx);
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
            );
        })?;

        // 진행 중 턴이 끝났는지 확인한다. 끝났으면 최종 결과를 처리한다.
        if let Some((_, handle)) = pending_turn.take() {
            match handle.join() {
                Ok(Ok(turn)) => {
                    // 스트리밍 중 누적된 마지막 Assistant 줄의 콘텐츠가 최종
                    // 결과와 다르면(예: 도구 호출로 본문이 재구성됨) generation을
                    // 증가시켜 렌더링 캐시를 무효화한다.
                    if let Some(last) = lines.last_mut() {
                        if last.role == Role::Assistant
                            && (last.text != turn.assistant_content
                                || last.reasoning_content != turn.reasoning_content)
                        {
                            last.bump_generation();
                        }
                    }
                    lines.push(ChatLine {
                        role: Role::Assistant,
                        text: turn.assistant_content.clone(),
                        reasoning_content: turn.reasoning_content.clone(),
                        // 완료 후 reasoning 은 접힌 상태로 저장한다.
                        reasoning_expanded: false,
                        input_tokens: turn.input_tokens,
                        output_tokens: turn.output_tokens,
                        duration_ms: turn.duration_ms,
                        ..ChatLine::default()
                    });
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
                if text_completion_active && !text_completions.is_empty() {
                    let item =
                        text_completions[text_completion_idx.min(text_completions.len() - 1)]
                            .clone();
                    set_input(&mut input, &mut cursor, item.insert_text);
                    text_completions.clear();
                    text_completion_active = false;
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
            }
            KeyCode::Backspace => {
                if multiline && input.as_bytes().get(cursor) == Some(&b'\n') {
                    // 멀티라인: 커서 위치의 줄바꿈을 한 번에 지운다 (줄 병합).
                    input.replace_range(cursor..cursor + 1, "");
                    continue;
                }
                delete_back(&mut input, &mut cursor);
                if !multiline {
                    update_completions(&mut completions, &mut completion_idx, &mut completion_active, &input);
                    update_text_completions(&mut text_completions, &mut text_completion_idx, &mut text_completion_active, &input, &history_sources);
                }
            }
            KeyCode::Delete => {
                delete_fwd(&mut input, &mut cursor);
                if !multiline {
                    update_completions(&mut completions, &mut completion_idx, &mut completion_active, &input);
                    update_text_completions(&mut text_completions, &mut text_completion_idx, &mut text_completion_active, &input, &history_sources);
                }
            }
            KeyCode::Tab => {
                // Tab: 자동완성 선택 항목 순환 (슬래시 드롭다운이 열려 있으면
                // 슬래시 우선, 아니면 텍스트 후보 순환).
                if completion_active && !completions.is_empty() {
                    completion_idx = (completion_idx + 1) % completions.len();
                } else if text_completion_active && !text_completions.is_empty() {
                    text_completion_idx = (text_completion_idx + 1) % text_completions.len();
                } else {
                    // 자동완성이 없으면 직접 완성 시도.
                    if completions.len() == 1 {
                        set_input(&mut input, &mut cursor, completions[0].insert.clone());
                        completions.clear();
                        completion_active = false;
                    } else {
                        update_completions(&mut completions, &mut completion_idx, &mut completion_active, &input);
                        update_text_completions(&mut text_completions, &mut text_completion_idx, &mut text_completion_active, &input, &history_sources);
                        if completions.len() == 1 {
                            set_input(&mut input, &mut cursor, completions[0].insert.clone());
                            completions.clear();
                            completion_active = false;
                        } else if text_completions.len() == 1 {
                            set_input(&mut input, &mut cursor, text_completions[0].insert_text.clone());
                            text_completions.clear();
                            text_completion_active = false;
                        }
                    }
                }
            }
            // 가장 마지막 Assistant 줄의 reasoning 접기/펼침 토글.
            // `Char(c)` arm 보다 앞에 두면 't' 가 입력창에 들어가지 않는다.
            KeyCode::Char('t') if !modifiers.contains(KeyModifiers::CONTROL) => {
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
                    update_completions(&mut completions, &mut completion_idx, &mut completion_active, &input);
                    update_text_completions(&mut text_completions, &mut text_completion_idx, &mut text_completion_active, &input, &history_sources);
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
                } else if text_completion_active && !text_completions.is_empty() {
                    text_completion_idx = text_completion_idx.saturating_sub(1);
                } else if input.trim().is_empty() && !history_sources.is_empty() {
                    // 히스토리 탐색: 빈 입력에서 ↑ 로 이전 프롬프트를 거슬러 올라간다.
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
                } else if text_completion_active && !text_completions.is_empty() {
                    text_completion_idx = (text_completion_idx + 1) % text_completions.len();
                } else if input.trim().is_empty() && history_idx.is_some() {
                    // 히스토리 탐색 종료: ↓ 로 최신 방향으로 내려가다 끝나면 입력창 비움.
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
                text_completions.clear();
                text_completion_active = false;
            }
            _ => {}
        }
    };

    Ok(Some(outcome))
}

/// 제목 + 엔드포인트·모델 표시.
fn draw_title(f: &mut ratatui::Frame, area: Rect, endpoint_name: &str, model: &str) {
    let title = format!("불티(Bulti) 대화형 채팅 — {endpoint_name} / {model}");
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
fn draw_scroll(f: &mut ratatui::Frame, area: Rect, lines: &mut [ChatLine], offset_from_bottom: usize) {
    let inner_width = area.width.saturating_sub(2).max(1) as usize;
    let text_width = inner_width.max(1);
    let mut text_lines: Vec<Line> = Vec::new();
    for line in lines.iter_mut() {
        let prefix = match line.role {
            Role::User => "▶ ",
            Role::Assistant => "◀ ",
            Role::Status => "· ",
        };
        let style = match line.role {
            Role::User => Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
            Role::Assistant => Style::default().fg(Color::Cyan),
            Role::Status => Style::default().fg(Color::DarkGray),
        };
        // Assistant 응답은 마크다운 렌더링을 적용한다 (코드블록·인라인 코드·
        // 테이블·리스트·헤딩). User/Status 는 기존 평문 래핑을 유지한다.
        // 접두/인덴트 스팬을 rendered 라인에 직접 적용해, 캐시 비교·재래핑이
        // 라인 단위에서 자기 완결적으로 되도록 한다.
        let rendered: Vec<Line<'static>> = if line.role == Role::Assistant {
            let blocks = crate::render::parse(&line.text);
            let mut rendered: Vec<Line<'static>> = Vec::new();
            for (i, rline) in crate::render::render_lines(&blocks).iter().enumerate() {
                let mut spans: Vec<Span<'static>> = Vec::new();
                if i == 0 {
                    spans.push(Span::styled(prefix, style));
                } else {
                    spans.push(Span::raw("  ".to_string()));
                }
                spans.extend(rline.spans.iter().cloned());
                rendered.push(Line::from(spans));
            }
            rendered
        } else {
            // 평문: 한 원문 줄 = 한 렌더 라인 (prefix 스타일 스팬 포함).
            line.text
                .split('\n')
                .map(|raw| {
                    Line::from(vec![
                        Span::styled(prefix, style),
                        Span::styled(raw.to_string(), style),
                    ])
                })
                .collect()
        };
        line.render_cache.append_wrapped(&mut text_lines, &rendered, text_width, line.generation, false);
        // 모델 생각(reasoning) — 생성 중에는 펼쳐서 실시간 표시하고, 완료 후엔
        // "🤔 생각" 한 줄로 접는다. `t` 키로 접기/펼침 토글.
        if !line.reasoning_content.trim().is_empty() {
            let dim = Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM | Modifier::ITALIC);
            let reasoning: Vec<Line<'static>> = if line.reasoning_expanded {
                let blocks = crate::render::parse(&line.reasoning_content);
                crate::render::render_lines(&blocks)
                    .iter()
                    .map(|rline| {
                        Line::from(
                            rline
                                .spans
                                .iter()
                                .map(|s| Span::styled(s.content.clone(), dim))
                                .collect::<Vec<_>>(),
                        )
                    })
                    .collect()
            } else {
                vec![Line::from(vec![Span::styled(
                    format!("🤔 생각 ({}자) — 펼치기", line.reasoning_content.chars().count()),
                    Style::default().fg(Color::DarkGray),
                )])]
            };
            line
                .reasoning_cache
                .append_wrapped(&mut text_lines, &reasoning, text_width, line.generation, line.reasoning_expanded);
        }
    }

    let scroll = scroll_from_bottom(text_lines.len(), area.height, offset_from_bottom);
    let para = Paragraph::new(text_lines)
        .wrap(Wrap { trim: false })
        .scroll((scroll as u16, 0))
        .block(Block::default().padding(ratatui::widgets::Padding::new(1, 1, 0, 0)));
    f.render_widget(para, area);
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

/// 입력창 렌더링. `ghost` 가 비어 있지 않으면 커서(입력 문자열) 뒤에
/// dim+italic 고스트 텍스트를 연결해 표시한다.
/// `cursor` 는 바이트 인덱스이며, 그 위치의 문자를 역색 스팬으로 그려 커서
/// 를 표시한다 (끝이면 빈 역색 스팬).
/// `multiline` 이면 입력을 줄 단위로 나눠 여러 줄로 렌더하고, 커서는
/// `cursor` 가 속한 라인에만 표시한다 (고스트 스킵).
fn draw_input(f: &mut ratatui::Frame, area: Rect, input: &str, cursor: usize, ghost: &str, multiline: bool) {
    let title = if multiline {
        "입력 (멀티라인 — Shift+Enter 줄바꿈 · Enter 전송 · /multiline 꺼짐)"
    } else {
        "입력 (Enter 전송 · Tab 자동완성 · Ctrl+S 저장 · Ctrl+Q 종료)"
    };
    // 커서 표시: [cursor 앞][커서 문자(역색)][cursor 뒤] + (단일라인) 고스트.
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
        let mut spans = vec![
            Span::raw(before),
            Span::styled(cur, inv),
            Span::raw(after),
        ];
        if !ghost.is_empty() {
            spans.push(Span::styled(
                ghost.to_string(),
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::DIM | Modifier::ITALIC),
            ));
        }
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
fn draw_text_completions(
    f: &mut ratatui::Frame,
    input_area: Rect,
    completions: &[crate::completion::CompletionItem],
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
        "히스토리 자동완성 (Enter 선택 · Tab/↑↓ 이동 · Esc 취소)",
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
    )]));
    for (i, it) in items.iter().enumerate() {
        let selected = i == idx.min(completions.len() - 1);
        let style = if selected {
            Style::default().fg(Color::Black).bg(Color::Cyan)
        } else {
            Style::default().fg(Color::White)
        };
        lines.push(Line::from(vec![
            Span::styled(if selected { "▶ " } else { "  " }, style),
            Span::styled(it.display.clone(), style),
            Span::styled(format!("  — {}", it.description), style),
        ]));
    }
    let p = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL))
        .style(Style::default().bg(Color::DarkGray));
    f.render_widget(p, area);
}

/// `/` 입력 시 자동완성 상태를 갱신한다. `/` 가 아니면 드롭다운을 닫는다.
fn update_completions(
    completions: &mut Vec<crate::slash::Suggestion>,
    idx: &mut usize,
    active: &mut bool,
    input: &str,
) {
    if let Some(query) = input.strip_prefix('/') {
        let mut next = crate::slash::suggest(query);
        next.dedup_by(|a, b| a.insert == b.insert);
        *completions = next;
        *idx = 0;
        *active = !completions.is_empty();
    } else {
        completions.clear();
        *active = false;
    }
}

/// 일반 텍스트 자동완성 후보를 갱신한다.
/// 슬래시 입력이면(슬래시 드롭다운이 담당) 후보를 비우고,
/// 아니면 fuzzy 매칭으로 히스토리 후보를 채운다.
fn update_text_completions(
    completions: &mut Vec<crate::completion::CompletionItem>,
    idx: &mut usize,
    active: &mut bool,
    input: &str,
    sources: &[String],
) {
    let trimmed = input.trim();
    if input.starts_with('/') || trimmed.is_empty() || trimmed.chars().count() < 2 {
        completions.clear();
        *active = false;
        return;
    }
    *completions = crate::completion::find_matches(input, sources, 6);
    *idx = 0;
    *active = !completions.is_empty();
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
) {
    let mut status = if saved {
        format!("세션 저장됨 — {session_id}")
    } else {
        format!("세션 id: {session_id}")
    };
    status.push_str(&format!("  세션 ↑{total_in} ↓{total_out}"));
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
        update_completions(&mut completions, &mut idx, &mut active, "/");
        assert!(active);
        assert!(completions.iter().any(|c| c.insert == "/exit"));
        assert!(completions.iter().any(|c| c.insert == "/model"));
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
        update_completions(&mut completions, &mut idx, &mut active, "hello");
        assert!(!active);
        assert!(completions.is_empty());
    }

    /// 일반 텍스트 자동완성: 2자 이상 입력 시 fuzzy 후보가 채워진다.
    #[test]
    fn update_text_completions_finds_history_matches() {
        let sources = vec![
            "안녕하세요 반갑습니다".to_string(),
            "오늘 날씨 알려줘".to_string(),
        ];
        let mut completions = Vec::new();
        let mut idx = 0usize;
        let mut active = false;
        update_text_completions(&mut completions, &mut idx, &mut active, "안녕", &sources);
        assert!(active);
        assert!(completions.iter().any(|c| c.insert_text == "안녕하세요 반갑습니다"));
    }

    /// 슬래시 입력이면 텍스트 자동완성 후보를 비운다.
    #[test]
    fn update_text_completions_clears_on_slash() {
        let sources = vec!["안녕하세요".to_string()];
        let mut completions = vec![crate::completion::CompletionItem {
            display: "안녕하세요".to_string(),
            description: "히스토리".to_string(),
            insert_text: "안녕하세요".to_string(),
            replace_range: None,
        }];
        let mut idx = 0usize;
        let mut active = true;
        update_text_completions(&mut completions, &mut idx, &mut active, "/ex", &sources);
        assert!(!active);
        assert!(completions.is_empty());
    }

    /// 1자 이하 입력은 후보를 채우지 않는다.
    #[test]
    fn update_text_completions_ignores_single_char() {
        let sources = vec!["안녕하세요".to_string()];
        let mut completions = Vec::new();
        let mut idx = 0usize;
        let mut active = false;
        update_text_completions(&mut completions, &mut idx, &mut active, "안", &sources);
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
}

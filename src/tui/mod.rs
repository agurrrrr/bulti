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
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub duration_ms: u64,
}

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
/// `processor` 는 사용자 메시지를 받아 한 턴(세그먼트 체인)을 실행하고
/// `TurnResult` 를 반환하는 클로저다. TUI 루프는 Enter 로 메시지를
/// 전송할 때마다 processor 를 호출해 응답·생각·토큰·속도를 대화 리스트에
/// 추가한다.
pub fn run_tui<F>(
    options: &TuiOptions,
    initial_lines: Vec<ChatLine>,
    mut processor: F,
) -> Result<Option<TuiOutcome>, Box<dyn std::error::Error>>
where
    F: FnMut(String) -> Result<TurnResult, Box<dyn std::error::Error>>,
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
    let mut offset_from_bottom: usize = 0;
    let mut saved = false;

    let outcome = loop {
        terminal.draw(|f| {
            let size = f.area();
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(1), // 제목
                    Constraint::Min(3),    // 대화 리스트
                    Constraint::Length(3), // 입력창
                    Constraint::Length(1), // 상태 표시
                ])
                .split(size);

            draw_title(f, chunks[0], &options.endpoint_name, &options.model);
            draw_scroll(f, chunks[1], &lines, offset_from_bottom);
            draw_input(f, chunks[2], &input);
            draw_status(f, chunks[3], &options.session_id, saved, last_assistant(&lines));
        })?;

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
                let trimmed = input.trim().to_string();
                if trimmed.is_empty() {
                    continue;
                }
                input.clear();
                lines.push(ChatLine {
                    role: Role::User,
                    text: trimmed.clone(),
                    ..ChatLine::default()
                });
                // 응답 생성 중 상태를 표시한다. processor 는 동기 실행되므로
                // reasoning 은 완료 후 접힌 채 저장된다.
                lines.push(ChatLine {
                    role: Role::Status,
                    text: "응답 생성 중...".to_string(),
                    ..ChatLine::default()
                });
                terminal.draw(|f| {
                    let size = f.area();
                    let chunks = Layout::default()
                        .direction(Direction::Vertical)
                        .constraints([
                            Constraint::Length(1),
                            Constraint::Min(3),
                            Constraint::Length(3),
                            Constraint::Length(1),
                        ])
                        .split(size);
                    draw_title(f, chunks[0], &options.endpoint_name, &options.model);
                    draw_scroll(f, chunks[1], &lines, 0);
                    draw_input(f, chunks[2], &input);
                    draw_status(f, chunks[3], &options.session_id, saved, last_assistant(&lines));
                })?;

                let response = processor(trimmed);
                let _ = lines.pop(); // "응답 생성 중..."
                match response {
                    Ok(turn) => {
                        lines.push(ChatLine {
                            role: Role::Assistant,
                            text: turn.assistant_content.clone(),
                            reasoning_content: turn.reasoning_content.clone(),
                            input_tokens: turn.input_tokens,
                            output_tokens: turn.output_tokens,
                            duration_ms: turn.duration_ms,
                        });
                    }
                    Err(e) => {
                        lines.push(ChatLine {
                            role: Role::Status,
                            text: format!("오류: {e}"),
                            ..ChatLine::default()
                        });
                    }
                }
                offset_from_bottom = 0;
            }
            KeyCode::Backspace => {
                input.pop();
            }
            KeyCode::Char(c) => {
                if modifiers.contains(KeyModifiers::CONTROL) {
                    continue;
                }
                input.push(c);
            }
            KeyCode::PageUp => {
                offset_from_bottom = offset_from_bottom.saturating_add(10);
            }
            KeyCode::PageDown => {
                offset_from_bottom = offset_from_bottom.saturating_sub(10);
            }
            KeyCode::Up => {
                offset_from_bottom = offset_from_bottom.saturating_add(1);
            }
            KeyCode::Down => {
                offset_from_bottom = offset_from_bottom.saturating_sub(1);
            }
            KeyCode::Esc => {}
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

/// `width` 열에 맞춰 텍스트를 줄바꿈한다.
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
fn draw_scroll(f: &mut ratatui::Frame, area: Rect, lines: &[ChatLine], offset_from_bottom: usize) {
    let inner_width = area.width.saturating_sub(2).max(1) as usize;
    let text_width = inner_width.max(1);
    let mut text_lines: Vec<Line> = Vec::new();
    for l in lines {
        let prefix = match l.role {
            Role::User => "▶ ",
            Role::Assistant => "◀ ",
            Role::Status => "· ",
        };
        let style = match l.role {
            Role::User => Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
            Role::Assistant => Style::default().fg(Color::Cyan),
            Role::Status => Style::default().fg(Color::DarkGray),
        };
        let wrapped = wrap_text(&l.text, text_width);
        for (i, wline) in wrapped.into_iter().enumerate() {
            let p = if i == 0 { prefix } else { "  " };
            text_lines.push(Line::from(vec![
                Span::styled(p, style),
                Span::styled(wline, style),
            ]));
        }
        // 모델 생각(reasoning) — 응답 생성 중에만 펼쳐 표시하고, 완료된 응답은
        // "🤔 생각 보기" 한 줄로 접어서 저장한다.
        if !l.reasoning_content.trim().is_empty() {
            text_lines.push(Line::from(vec![Span::styled(
                format!("🤔 생각 ({}자)", l.reasoning_content.chars().count()),
                Style::default().fg(Color::DarkGray),
            )]));
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

/// 마지막 Assistant 응답을 찾아 상태 표시줄에 토큰/속도를 보여준다.
fn last_assistant(lines: &[ChatLine]) -> Option<&ChatLine> {
    lines.iter().rev().find(|l| l.role == Role::Assistant)
}

/// 입력창 렌더링.
fn draw_input(f: &mut ratatui::Frame, area: Rect, input: &str) {
    let p = Paragraph::new(input)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("입력 (Enter 전송 · Ctrl+S 저장 · Ctrl+Q 종료)"),
        )
        .wrap(Wrap { trim: false });
    f.render_widget(p, area);
}

/// 상태 표시 — 세션 id·저장 여부·최근 응답 토큰/속도.
fn draw_status(
    f: &mut ratatui::Frame,
    area: Rect,
    session_id: &str,
    saved: bool,
    last: Option<&ChatLine>,
) {
    let mut status = if saved {
        format!("세션 저장됨 — {session_id}")
    } else {
        format!("세션 id: {session_id}")
    };
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

    /// TTY 가 아니면 run_tui 는 None 을 반환한다 (스트림 텍스트 폴백).
    #[test]
    fn run_tui_returns_none_on_non_tty() {
        let options = TuiOptions {
            endpoint_name: "ep".to_string(),
            model: "model".to_string(),
            session_id: "sid".to_string(),
        };
        let result = run_tui(&options, vec![], |_| {
            Ok(TurnResult {
                exit_code: 0,
                assistant_content: "ok".to_string(),
                reasoning_content: String::new(),
                input_tokens: 0,
                output_tokens: 0,
                duration_ms: 0,
                files_touched: vec![],
            })
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
}

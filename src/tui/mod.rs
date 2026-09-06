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
#[derive(Debug, Clone)]
pub struct ChatLine {
    pub role: Role,
    pub text: String,
}

/// 메시지 역할.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Role {
    User,
    Assistant,
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
/// 모델 응답 문자열을 반환하는 클로저다. TUI 루프는 Enter 로 메시지를
/// 전송할 때마다 processor 를 호출해 응답을 대화 리스트에 추가한다.
pub fn run_tui<F>(
    options: &TuiOptions,
    initial_lines: Vec<ChatLine>,
    mut processor: F,
) -> Result<Option<TuiOutcome>, Box<dyn std::error::Error>>
where
    F: FnMut(String) -> Result<String, Box<dyn std::error::Error>>,
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
            draw_status(f, chunks[3], &options.session_id, saved);
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
                });
                lines.push(ChatLine {
                    role: Role::Status,
                    text: "응답 생성 중...".to_string(),
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
                    draw_status(f, chunks[3], &options.session_id, saved);
                })?;

                let response = processor(trimmed);
                let _ = lines.pop(); // "응답 생성 중..."
                match response {
                    Ok(assistant) => {
                        lines.push(ChatLine {
                            role: Role::Assistant,
                            text: assistant,
                        });
                    }
                    Err(e) => {
                        lines.push(ChatLine {
                            role: Role::Status,
                            text: format!("오류: {e}"),
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
    let inner_width = area.width.saturating_sub(4).max(1) as usize;
    let text_width = inner_width.saturating_sub(2).max(1);
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
    }

    let inner_h = area.height.saturating_sub(2) as usize;
    let max_scroll = text_lines.len().saturating_sub(inner_h);
    let scroll = max_scroll.saturating_sub(offset_from_bottom);

    let para = Paragraph::new(text_lines)
        .block(Block::default().borders(Borders::ALL).title("대화"))
        .scroll((scroll as u16, 0));
    f.render_widget(para, area);
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

/// 상태 표시.
fn draw_status(f: &mut ratatui::Frame, area: Rect, session_id: &str, saved: bool) {
    let status = if saved {
        format!("세션 저장됨 — {session_id}")
    } else {
        format!("세션 id: {session_id}")
    };
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
        let result = run_tui(&options, vec![], |_| Ok("ok".to_string())).unwrap();
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

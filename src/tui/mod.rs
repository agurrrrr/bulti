//! 대화형 TUI 렌더링 (DESIGN.md §4.13.3).
//!
//! ratatui + crossterm 으로 대화·도구 호출·결과를 화면에 렌더링한다.
//! - 스크롤백: 대화 메시지 리스트 (사용자·모델 턴)
//! - 입력 라인: 하단 입력창
//! - 상태 표시: 엔드포인트·모델·토큰·세션 id
//!
//! 키 바인딩:
//! - `Enter`           — 대화 전송
//! - `Ctrl+S`          — 세션 저장
//! - `Ctrl+Q`          — 종료 (exit 0)
//! - `Ctrl+C`          — 즉시 종료 (SIGINT 규약 130)
//! - `PgUp` / `PgDn`   — 대화 스크롤
//! - `Esc`             — 입력 모드 → 결과 화면 (무시)

use std::io::{self, IsTerminal};
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};
use ratatui::Terminal;

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

    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    terminal.clear()?;

    crossterm::terminal::enable_raw_mode()?;
    let _ = crossterm::execute!(terminal.backend_mut(), crossterm::terminal::EnterAlternateScreen);

    let mut lines = initial_lines;
    let mut input = String::new();
    let mut scroll: usize = 0;
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
            draw_scroll(f, chunks[1], &lines, scroll);
            draw_input(f, chunks[2], &input);
            draw_status(f, chunks[3], &options.session_id, saved);
        })?;

        if event::poll(Duration::from_millis(200))? {
            if let Event::Key(KeyEvent { code, modifiers, .. }) = event::read()? {
                match code {
                    // 종료: Ctrl+Q
                    KeyCode::Char('q') if modifiers.contains(KeyModifiers::CONTROL) => {
                        break TuiOutcome {
                            exit_code: 0,
                            saved,
                        };
                    }
                    // 종료: Ctrl+C (SIGINT 규약 130)
                    KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
                        break TuiOutcome {
                            exit_code: 130,
                            saved,
                        };
                    }
                    // 세션 저장: Ctrl+S
                    KeyCode::Char('s') if modifiers.contains(KeyModifiers::CONTROL) => {
                        saved = true;
                        lines.push(ChatLine {
                            role: Role::Status,
                            text: "세션 저장됨".to_string(),
                        });
                    }
                    // 대화 전송: Enter
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
                        // 한 턴 실행 (모델 응답 생성).
                        let response = processor(trimmed.clone());
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
                        scroll = lines.len().saturating_sub(1);
                    }
                    KeyCode::Backspace => {
                        input.pop();
                    }
                    KeyCode::Char(c) => {
                        input.push(c);
                    }
                    KeyCode::PageUp => {
                        scroll = scroll.saturating_sub(10);
                    }
                    KeyCode::PageDown => {
                        scroll = scroll.saturating_add(10).min(lines.len().saturating_sub(1));
                    }
                    KeyCode::Esc => {}
                    _ => {}
                }
            }
        }
    };

    crossterm::terminal::disable_raw_mode()?;
    terminal.show_cursor()?;
    let _ = crossterm::execute!(terminal.backend_mut(), crossterm::terminal::LeaveAlternateScreen);
    terminal.flush()?;

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

/// 대화 메시지 리스트 렌더링 (스크롤백).
fn draw_scroll(f: &mut ratatui::Frame, area: Rect, lines: &[ChatLine], scroll: usize) {
    let items: Vec<ListItem> = lines
        .iter()
        .map(|l| {
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
            ListItem::new(Line::from(vec![
                Span::styled(prefix, style),
                Span::styled(l.text.clone(), style),
            ]))
        })
        .collect();

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title("대화"))
        .highlight_style(Style::default().add_modifier(Modifier::BOLD));
    let viewport = Rect::new(area.x, area.y, area.width, area.height.saturating_sub(2).max(1));
    f.render_stateful_widget(
        list,
        viewport,
        &mut ratatui::widgets::ListState::default().with_offset(scroll),
    );
}

/// 입력창 렌더링.
fn draw_input(f: &mut ratatui::Frame, area: Rect, input: &str) {
    let p = Paragraph::new(input).block(
        Block::default()
            .borders(Borders::ALL)
            .title("입력 (Enter 전송 · Ctrl+S 저장 · Ctrl+Q 종료)"),
    );
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
        // 테스트 환경은 TTY 가 아니므로 None 이어야 한다.
        let options = TuiOptions {
            endpoint_name: "ep".to_string(),
            model: "model".to_string(),
            session_id: "sid".to_string(),
        };
        let result = run_tui(&options, vec![], |_| Ok("ok".to_string())).unwrap();
        assert!(result.is_none());
    }
}
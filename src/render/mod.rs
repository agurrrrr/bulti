//! 마크다운 렌더링 (DESIGN.md §4.13.3 개선).
//!
//! 외부 크레이트 없이 자체 경량 파서로 코드블록·인라인 코드·테이블·리스트·헤딩을
//! 구조화된 블록으로 변환한다. 스트리밍 중에도 불완전한 블록(닫히지 않은 ```)을
//! 부분 렌더링할 수 있어 화면이 깨지지 않는다.
//!
//! - `parse`        : 마크다운 텍스트 → 구조화된 블록 시퀀스 (스트리밍 안전)
//! - `render_ansi`  : 블록 → ANSI 컬러 문자열 (chat_cmd 스트림 텍스트 모드)
//! - `render_lines` : 블록 → ratatui `Line` 벡터 (TUI draw_scroll)

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// 렌더링 가능한 블록 종류.
#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    /// 일반 문단 (인라인 코드·강조 포함).
    Paragraph(Vec<Inline>),
    /// 코드블록 (``` 로 감싼 부분). `open` 이면 닫히지 않은 스트리밍 블록.
    Code {
        lang: String,
        lines: Vec<String>,
        open: bool,
    },
    /// 테이블 (GFM 파이프 테이블).
    Table {
        header: Vec<String>,
        rows: Vec<Vec<String>>,
    },
    /// 리스트 (-, *, 1. 등). 항목은 인라인 구문 포함.
    List { items: Vec<Vec<Inline>> },
    /// 헤딩 (# ~ ######).
    Heading { level: u8, text: Vec<Inline> },
    /// 인용구 (>).
    Quote { text: Vec<Inline> },
    /// 빈 줄.
    Blank,
}

/// 문단 내 인라인 요소.
#[derive(Debug, Clone, PartialEq)]
pub enum Inline {
    Text(String),
    /// 인라인 코드 (`...`).
    Code(String),
    /// 볼드 (**...**)
    Bold(String),
    /// 이탤릭 (*...*)
    Italic(String),
}

/// 마크다운 텍스트를 블록 시퀀스로 파싱한다. 스트리밍 도중 불완전한 블록도
/// 부분 렌더링할 수 있도록 열린 상태(`open`)를 보존한다.
pub fn parse(text: &str) -> Vec<Block> {
    let lines: Vec<&str> = text.split('\n').collect();
    let mut out: Vec<Block> = Vec::new();
    let mut i = 0usize;
    while i < lines.len() {
        let line = lines[i];

        // 1. 코드블록 펜스 (``` 또는 ~~~)
        if is_fence(line) {
            let lang = fence_lang(line);
            let mut code_lines: Vec<String> = Vec::new();
            let mut closed = false;
            i += 1;
            while i < lines.len() {
                if is_fence(lines[i]) {
                    closed = true;
                    i += 1;
                    break;
                }
                code_lines.push(lines[i].to_string());
                i += 1;
            }
            out.push(Block::Code {
                lang,
                lines: code_lines,
                open: !closed,
            });
            continue;
        }

        // 2. 헤딩
        if let Some((level, content)) = heading(line) {
            out.push(Block::Heading {
                level,
                text: parse_inline(content),
            });
            i += 1;
            continue;
        }

        // 3. 인용구
        if let Some(content) = quote(line) {
            out.push(Block::Quote {
                text: parse_inline(content),
            });
            i += 1;
            continue;
        }

        // 4. 리스트
        if let Some(item_text) = list_item(line) {
            let mut items = vec![parse_inline(item_text)];
            i += 1;
            while i < lines.len() {
                if let Some(item2) = list_item(lines[i]) {
                    items.push(parse_inline(item2));
                    i += 1;
                } else if lines[i].trim().is_empty() {
                    // 빈 줄 뒤에 리스트 항목이 계속되면 이어 받는다.
                    let mut j = i + 1;
                    while j < lines.len() && lines[j].trim().is_empty() {
                        j += 1;
                    }
                    if j < lines.len() && list_item(lines[j]).is_some() {
                        i = j;
                    } else {
                        break;
                    }
                } else {
                    break;
                }
            }
            out.push(Block::List { items });
            continue;
        }

        // 5. 테이블 (현재 줄 = 헤더, 다음 줄 = 구분선, 이후 = 데이터 행)
        //    헤더 줄이 단계 7(문단)에 먼저 소비되지 않도록 문단 수집 전에 처리한다.
        if i + 1 < lines.len() && is_table_sep(lines[i + 1]) && line.contains('|') {
            let header = split_table(line);
            let mut rows: Vec<Vec<String>> = Vec::new();
            let mut r = i + 2;
            while r < lines.len() {
                let l = lines[r].trim();
                if l.is_empty() || is_table_sep(l) {
                    break;
                }
                if l.contains('|') {
                    rows.push(split_table(l));
                    r += 1;
                } else {
                    break;
                }
            }
            // 헤더가 유효한 셀로 파싱됐을 때만 테이블로 인정.
            if !header.is_empty() && header.iter().all(|c| !c.trim().is_empty()) {
                out.push(Block::Table { header, rows });
                i = r;
                continue;
            }
        }

        // 6. 빈 줄
        if line.trim().is_empty() {
            out.push(Block::Blank);
            i += 1;
            continue;
        }

        // 7. 일반 문단 — 연속된 일반 줄을 모은다.
        let mut para: Vec<String> = Vec::new();
        while i < lines.len() {
            let l = lines[i];
            if l.trim().is_empty()
                || is_fence(l)
                || heading(l).is_some()
                || quote(l).is_some()
                || list_item(l).is_some()
                || (i + 1 < lines.len() && is_table_sep(lines[i + 1]) && l.contains('|'))
            {
                break;
            }
            para.push(l.to_string());
            i += 1;
        }
        out.push(Block::Paragraph(parse_inline(&para.join("\n"))));
        continue;
    }
    out
}

/// 인라인 구문(코드·볼드·이탤릭)을 파싱한다. 닫히지 않은 코드(`)는 스트리밍
/// 중에도 부분 렌더링을 위해 `Code` 로 처리한다.
pub fn parse_inline(text: &str) -> Vec<Inline> {
    let chars: Vec<char> = text.chars().collect();
    let mut out: Vec<Inline> = Vec::new();
    let mut buf = String::new();
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];
        // 인라인 코드
        if c == '`' {
            let mut j = i + 1;
            let mut code = String::new();
            while j < chars.len() && chars[j] != '`' {
                code.push(chars[j]);
                j += 1;
            }
            // 닫히지 않았어도(스트리밍) 코드로 렌더링
            flush_text(&mut buf, &mut out);
            out.push(Inline::Code(code));
            i = j + 1; // 닫히면 그 뒤, 아니면 끝(스트리밍)
            continue;
        }
        // 볼드 **..**
        if c == '*' && i + 1 < chars.len() && chars[i + 1] == '*' {
            let mut j = i + 2;
            let mut b = String::new();
            let mut closed = false;
            while j + 1 < chars.len() {
                if chars[j] == '*' && chars[j + 1] == '*' {
                    closed = true;
                    break;
                }
                b.push(chars[j]);
                j += 1;
            }
            if closed {
                flush_text(&mut buf, &mut out);
                out.push(Inline::Bold(b));
                i = j + 2;
                continue;
            }
        }
        // 이탤릭 *..*
        if c == '*' && i + 1 < chars.len() {
            let mut j = i + 1;
            let mut it = String::new();
            let mut closed = false;
            while j < chars.len() {
                if chars[j] == '*' {
                    closed = true;
                    break;
                }
                it.push(chars[j]);
                j += 1;
            }
            if closed {
                flush_text(&mut buf, &mut out);
                out.push(Inline::Italic(it));
                i = j + 1;
                continue;
            }
        }
        buf.push(c);
        i += 1;
    }
    flush_text(&mut buf, &mut out);
    out
}

fn flush_text(buf: &mut String, out: &mut Vec<Inline>) {
    if !buf.is_empty() {
        out.push(Inline::Text(std::mem::take(buf)));
    }
}

// ---- 보조 함수 --------------------------------------------------------------

fn is_fence(line: &str) -> bool {
    let t = line.trim();
    t.starts_with("```") || t.starts_with("~~~")
}

fn fence_lang(line: &str) -> String {
    let t = line.trim();
    t.trim_start_matches('`')
        .trim_start_matches('~')
        .trim()
        .to_string()
}

fn heading(line: &str) -> Option<(u8, &str)> {
    let t = line.trim();
    if !t.starts_with('#') {
        return None;
    }
    let mut level = 0u8;
    for c in t.chars() {
        if c == '#' {
            level += 1;
        } else {
            break;
        }
    }
    if level == 0 || level > 6 {
        return None;
    }
    let content = t[level as usize..].trim();
    if content.is_empty() {
        return None;
    }
    Some((level, content))
}

fn quote(line: &str) -> Option<&str> {
    let t = line.trim();
    if let Some(rest) = t.strip_prefix('>') {
        let rest = rest.trim();
        return Some(rest);
    }
    None
}

fn list_item(line: &str) -> Option<&str> {
    let t = line.trim();
    // 테이블 구분선(--|---)은 리스트 항목으로 오인하지 않는다.
    if is_table_sep(t) {
        return None;
    }
    if t.starts_with('-') || t.starts_with('*') || t.starts_with('+') {
        let rest = t[1..].trim();
        if rest.is_empty() {
            return None;
        }
        return Some(rest);
    }
    // 순서 리스트: 1. 2. 10. 등
    let bytes = t.as_bytes();
    let mut idx = 0usize;
    while idx < bytes.len() && bytes[idx].is_ascii_digit() {
        idx += 1;
    }
    if idx > 0 && idx < bytes.len() && bytes[idx] == b'.' {
        let rest = t[idx + 1..].trim();
        if rest.is_empty() {
            return None;
        }
        return Some(rest);
    }
    None
}

/// GFM 테이블 구분선(---|---) 여부.
fn is_table_sep(line: &str) -> bool {
    let t = line.trim();
    if !t.contains('|') {
        return false;
    }
    for cell in t.split('|') {
        let c = cell.trim();
        if c.is_empty() {
            continue;
        }
        if !c.chars().all(|ch| ch == '-' || ch == ':' || ch == ' ') {
            return false;
        }
    }
    true
}

/// 파이프 테이블 행을 셀로 분할한다.
fn split_table(line: &str) -> Vec<String> {
    line.trim()
        .trim_matches('|')
        .split('|')
        .map(|c| c.trim().to_string())
        .collect()
}

// ---- ANSI 렌더링 ------------------------------------------------------------

/// 블록 시퀀스를 ANSI 컬러 문자열로 렌더링한다 (chat_cmd 스트림 텍스트 모드).
pub fn render_ansi(blocks: &[Block]) -> String {
    let mut out = String::new();
    for b in blocks {
        match b {
            Block::Paragraph(inlines) => {
                out.push_str(&render_inline_ansi(inlines));
                out.push('\n');
            }
            Block::Code { lines, open, .. } => {
                out.push_str("\x1b[48;2;30;30;30m\x1b[38;2;200;200;200m");
                for l in lines {
                    out.push_str(l);
                    out.push('\n');
                }
                if *open {
                    out.push('▌');
                } else {
                    out.push_str("```");
                }
                out.push_str("\x1b[0m\n");
            }
            Block::Table { header, rows } => {
                let widths = table_widths(header, rows);
                for row in std::iter::once(header).chain(rows.iter()) {
                    out.push_str("│ ");
                    for (i, cell) in row.iter().enumerate() {
                        let w = widths.get(i).copied().unwrap_or(0);
                        out.push_str(&format!("{cell:<w$}"));
                        if i + 1 < row.len() {
                            out.push_str(" │ ");
                        }
                    }
                    out.push_str(" │\n");
                }
            }
            Block::List { items } => {
                for item in items.iter() {
                    out.push_str(&format!("• {}\n", render_inline_ansi(item)));
                }
            }
            Block::Heading { level, text } => {
                let color = match level {
                    1 => "\x1b[1;36m",
                    2 => "\x1b[1;35m",
                    3 => "\x1b[1;34m",
                    _ => "\x1b[1;33m",
                };
                out.push_str(color);
                out.push_str(&"#".repeat(*level as usize));
                out.push(' ');
                out.push_str(&render_inline_ansi(text));
                out.push_str("\x1b[0m\n");
            }
            Block::Quote { text } => {
                out.push_str("\x1b[38;2;100;150;100m> ");
                out.push_str(&render_inline_ansi(text));
                out.push_str("\x1b[0m\n");
            }
            Block::Blank => out.push('\n'),
        }
    }
    out
}

fn render_inline_ansi(inlines: &[Inline]) -> String {
    let mut out = String::new();
    for inl in inlines {
        match inl {
            Inline::Text(t) => out.push_str(t),
            Inline::Code(c) => {
                out.push_str("\x1b[48;2;40;40;40m\x1b[38;2;220;120;120m");
                out.push_str(c);
                out.push_str("\x1b[0m");
            }
            Inline::Bold(b) => {
                out.push_str("\x1b[1m");
                out.push_str(b);
                out.push_str("\x1b[0m");
            }
            Inline::Italic(i) => {
                out.push_str("\x1b[3m");
                out.push_str(i);
                out.push_str("\x1b[0m");
            }
        }
    }
    out
}

// ---- ratatui 렌더링 ---------------------------------------------------------

/// 블록 시퀀스를 ratatui `Line` 벡터로 렌더링한다 (TUI draw_scroll).
pub fn render_lines(blocks: &[Block]) -> Vec<Line<'static>> {
    let mut out: Vec<Line> = Vec::new();
    for b in blocks {
        match b {
            Block::Paragraph(inlines) => {
                out.push(Line::from(inline_spans(inlines)));
            }
            Block::Code { lines, open, lang } => {
                // 코드블록: 배경색 + 테두리(상단/하단 구분선)
                let lang_disp = if lang.is_empty() { "code" } else { lang };
                out.push(Line::from(vec![Span::styled(
                    format!("┌─ {lang_disp} "),
                    code_box_style(),
                )]));
                for l in lines {
                    out.push(Line::from(vec![Span::styled(
                        format!("│ {l}"),
                        code_style(),
                    )]));
                }
                let bottom = if *open { "▌" } else { "└──────" };
                out.push(Line::from(vec![Span::styled(bottom, code_box_style())]));
            }
            Block::Table { header, rows } => {
                let widths = table_widths(header, rows);
                // 상단 구분선
                let total: usize = widths.iter().sum::<usize>() + (3 * widths.len().saturating_sub(1)) + 2;
                let top_sep: String = "─".repeat(total);
                out.push(Line::from(vec![Span::styled(format!("┌{top_sep}┐"), table_style())]));
                // 헤더
                out.push(Line::from(vec![Span::styled(
                    format_table_row(header, &widths),
                    table_style(),
                )]));
                // 헤더/데이터 구분선
                let mid_sep: String = "─".repeat(total);
                out.push(Line::from(vec![Span::styled(format!("├{mid_sep}┤"), table_style())]));
                for row in rows {
                    out.push(Line::from(vec![Span::styled(
                        format_table_row(row, &widths),
                        table_style(),
                    )]));
                }
                // 하단 구분선
                let bot_sep: String = "─".repeat(total);
                out.push(Line::from(vec![Span::styled(format!("└{bot_sep}┘"), table_style())]));
            }
            Block::List { items } => {
                for item in items.iter() {
                    let mut spans = vec![Span::styled("•", Style::default().fg(Color::Yellow)), Span::raw(" ".to_string())];
                    spans.extend(inline_spans(item));
                    out.push(Line::from(spans));
                }
            }
            Block::Heading { level, text } => {
                let style = match level {
                    1 => Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                    2 => Style::default()
                        .fg(Color::Magenta)
                        .add_modifier(Modifier::BOLD),
                    3 => Style::default()
                        .fg(Color::Blue)
                        .add_modifier(Modifier::BOLD),
                    _ => Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                };
                let joined = inline_plain(text);
                let prefix = "#".repeat(*level as usize);
                out.push(Line::from(vec![Span::styled(
                    format!("{prefix} {joined}"),
                    style,
                )]));
            }
            Block::Quote { text } => {
                out.push(Line::from(vec![Span::styled(
                    format!("> {}", inline_plain(text)),
                    quote_style(),
                )]));
            }
            Block::Blank => out.push(Line::from(Span::raw(""))),
        }
    }
    out
}

/// 인라인 요소를 ratatui Span 벡터로 변환한다.
fn inline_spans(inlines: &[Inline]) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for inl in inlines {
        match inl {
            Inline::Text(t) => spans.push(Span::raw(t.clone())),
            Inline::Code(c) => spans.push(Span::styled(c.clone(), inline_code_style())),
            Inline::Bold(b) => spans.push(Span::styled(
                b.clone(),
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Inline::Italic(i) => spans.push(Span::styled(
                i.clone(),
                Style::default().add_modifier(Modifier::ITALIC),
            )),
        }
    }
    spans
}

/// 인라인 요소를 평문으로 합친다.
fn inline_plain(inlines: &[Inline]) -> String {
    let mut out = String::new();
    for inl in inlines {
        match inl {
            Inline::Text(t) | Inline::Code(t) | Inline::Bold(t) | Inline::Italic(t) => {
                out.push_str(t);
            }
        }
    }
    out
}

// ---- 스타일 ------------------------------------------------------------------

fn code_style() -> Style {
    Style::default().fg(Color::Rgb(200, 200, 200))
}

fn code_box_style() -> Style {
    Style::default()
        .fg(Color::Rgb(120, 120, 120))
        .add_modifier(Modifier::DIM)
}

fn inline_code_style() -> Style {
    Style::default()
        .fg(Color::Rgb(220, 120, 120))
        .bg(Color::Rgb(40, 40, 40))
}

fn table_style() -> Style {
    Style::default().fg(Color::Cyan)
}

fn quote_style() -> Style {
    Style::default().fg(Color::Rgb(100, 150, 100))
}

/// 테이블 셀 폭 (CJK 2, ASCII 1) 계산.
fn cell_width(s: &str) -> usize {
    s.chars().map(|c| if c.is_ascii() { 1 } else { 2 }).sum()
}

fn table_widths(header: &[String], rows: &[Vec<String>]) -> Vec<usize> {
    let mut widths: Vec<usize> = header.iter().map(|h| cell_width(h)).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if i < widths.len() {
                widths[i] = widths[i].max(cell_width(cell));
            } else {
                widths.push(cell_width(cell));
            }
        }
    }
    widths
}

fn format_table_row(row: &[String], widths: &[usize]) -> String {
    let mut s = String::from("│ ");
    for (i, cell) in row.iter().enumerate() {
        let w = widths.get(i).copied().unwrap_or(0);
        let pad = w.saturating_sub(cell_width(cell));
        s.push_str(cell);
        s.push_str(&" ".repeat(pad));
        if i + 1 < row.len() {
            s.push_str(" │ ");
        }
    }
    s.push_str(" │");
    s
}

// ---- 테스트 ------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_code_block() {
        let blocks = parse("```rust\nfn main() {}\n```");
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            Block::Code { lang, lines, open } => {
                assert_eq!(lang, "rust");
                assert_eq!(lines, &["fn main() {}"]);
                assert!(!open);
            }
            other => panic!("expected code, got {other:?}"),
        }
    }

    #[test]
    fn unclosed_code_block_is_open() {
        let blocks = parse("```rust\nfn main() {");
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            Block::Code { open, .. } => assert!(*open),
            other => panic!("expected open code, got {other:?}"),
        }
    }

    #[test]
    fn parses_inline_code() {
        let inlines = parse_inline("use `std::io` here");
        assert!(inlines.iter().any(|i| matches!(i, Inline::Code(c) if c == "std::io")));
    }

    #[test]
    fn unclosed_inline_code_still_renders() {
        let inlines = parse_inline("use `std");
        assert!(inlines.iter().any(|i| matches!(i, Inline::Code(c) if c == "std")));
    }

    #[test]
    fn parses_table() {
        let blocks = parse("a | b\n--|---\n1 | 2");
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            Block::Table { header, rows } => {
                assert_eq!(header, &["a", "b"]);
                assert_eq!(rows, &[vec!["1".to_string(), "2".to_string()]]);
            }
            other => panic!("expected table, got {other:?}"),
        }
    }

    #[test]
    fn parses_list() {
        let blocks = parse("- one\n- two");
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            Block::List { items } => {
                assert_eq!(items.len(), 2);
                assert_eq!(
                    items[0],
                    vec![Inline::Text("one".to_string())]
                );
                assert_eq!(
                    items[1],
                    vec![Inline::Text("two".to_string())]
                );
            }
            other => panic!("expected list, got {other:?}"),
        }
    }

    #[test]
    fn parses_heading() {
        let blocks = parse("## 제목");
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            Block::Heading { level, .. } => assert_eq!(*level, 2),
            other => panic!("expected heading, got {other:?}"),
        }
    }

    #[test]
    fn render_ansi_contains_code_highlight() {
        let blocks = parse("```js\nlet x = 1;\n```\n`inline`");
        let out = render_ansi(&blocks);
        assert!(out.contains("let x = 1;"));
        assert!(out.contains("inline"));
    }

    #[test]
    fn render_lines_has_code_frame() {
        let blocks = parse("```\ncode\n```");
        let lines = render_lines(&blocks);
        let all_text: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert!(all_text.iter().any(|t| t.contains("┌")));
        assert!(all_text.iter().any(|t| t.contains("code")));
    }
}

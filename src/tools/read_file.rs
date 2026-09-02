//! `read_file` 네이티브 툴 (DESIGN.md §4.4, §4.5.3).
//!
//! - 줄 번호 프리픽스(`N→`)를 붙여 반환 (edit_file의 find에 정확한 줄 참조용).
//! - 기본 200줄 창 페이징. 푸터에 다음 offset 명시.
//! - auto-advance: 같은 path를 offset 없이 다시 읽으면 직전 끝줄 다음부터 이어서 반환.
//! - 파일 끝까지 읽은 뒤 offset 없는 재호출은 "이미 전체를 읽었다" 고정 메시지.
//! - 비전 엔드포인트: 이미지 파일을 base64 `image_url` content로 반환.
//! - 출력 상한(6,000자)을 줄 경계에서 지키고 푸터를 붙인다.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::tools::util::{int_arg, str_arg};
use crate::tools::{ToolHandler, ToolRegistry};

/// 기본 창 크기 (줄 수).
const DEFAULT_LIMIT: u64 = 200;
/// 출력 상한 (자). 푸터 예산 2,000자 제외.
const OUTPUT_CHAR_LIMIT: usize = 6_000;
/// auto-advance 상태 저장.
pub struct ReadState {
    /// path → 다음에 읽을 offset (auto-advance용).
    next_offset: HashMap<String, u64>,
    /// path → 전체 줄 수 (끝까지 읽었는지 판정용).
    total_lines: HashMap<String, u64>,
}

impl ReadState {
    fn new() -> Self {
        Self {
            next_offset: HashMap::new(),
            total_lines: HashMap::new(),
        }
    }
}

/// 공유 상태 (auto-advance).
static STATE: Mutex<Option<ReadState>> = Mutex::new(None);

fn state() -> &'static Mutex<Option<ReadState>> {
    &STATE
}

/// read_file 도구 스키마.
pub fn schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "description": "읽을 파일 경로 (프로젝트 루트 기준 상대경로)"
            },
            "offset": {
                "type": "integer",
                "description": "시작 줄 번호 (1-based). 생략 시 auto-advance"
            },
            "limit": {
                "type": "integer",
                "description": "읽을 줄 수, 기본 200"
            }
        },
        "required": ["path"]
    })
}

/// 이미지 확장자 판별.
fn is_image(path: &std::path::Path) -> bool {
    match path.extension().and_then(|e| e.to_str()) {
        Some(e) => matches!(e.to_ascii_lowercase().as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp"),
        None => false,
    }
}

/// 이미지를 base64 `image_url` content로 반환한다 (비전 엔드포인트).
fn image_content(path: &std::path::Path) -> Result<String, String> {
    use std::io::Read;
    let mut buf = Vec::new();
    std::fs::File::open(path)
        .and_then(|mut f| {
            f.read_to_end(&mut buf)?;
            Ok(())
        })
        .map_err(|e| format!("read_file: 이미지 읽기 실패: {e}"))?;
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&buf);
    // image_url content 반환 (DESIGN.md §4.4).
    Ok(format!(
        "![image](data:image/{};base64,{})",
        path.extension().and_then(|e| e.to_str()).unwrap_or("png"),
        b64
    ))
}

/// read_file 툴을 레지스트리에 등록한다.
pub fn register(reg: &mut ToolRegistry) {
    let vision = reg.vision();
    let handler: ToolHandler = std::sync::Arc::new(move |args| {
        let vision = vision;
        Box::pin(async move {
            let path_str = match str_arg(&args, "path") {
                Some(p) if !p.trim().is_empty() => p,
                _ => return Err("read_file: path 인자가 필요합니다".to_string()),
            };
            let offset_arg = args.get("offset").and_then(|v| v.as_u64());
            let limit = int_arg(&args, "limit", DEFAULT_LIMIT);

            let path = std::path::Path::new(&path_str);
            if !path.exists() {
                return Err(format!("read_file: 파일을 찾을 수 없습니다: {path_str}"));
            }
            if path.is_dir() {
                return Err(format!("read_file: 디렉터리는 읽을 수 없습니다: {path_str}"));
            }

            // 비전 엔드포인트 + 이미지 → base64 image_url content.
            if vision && is_image(path) {
                return image_content(path);
            }

            let content = std::fs::read_to_string(path)
                .map_err(|e| format!("read_file: 읽기 실패: {e}"))?;
            let lines: Vec<&str> = content.lines().collect();
            let total = lines.len() as u64;

            // offset 결정: 명시 or auto-advance.
            let offset = match offset_arg {
                Some(o) => o,
                None => {
                    let st = state().lock().unwrap();
                    let next = st.as_ref().and_then(|s| s.next_offset.get(&path_str).copied());
                    next.unwrap_or(1)
                }
            };

            // 이미 마지막까지 읽었고 offset 없이 재호출 → 고정 메시지 (page 1 wrap 방지).
            if offset_arg.is_none() {
                let st = state().lock().unwrap();
                let total_known = st.as_ref().and_then(|s| s.total_lines.get(&path_str).copied());
                if let Some(t) = total_known {
                    if offset > 1 && offset >= t {
                        return Ok("(이미 파일 전체를 읽었습니다. 편집 후 재읽기는 offset을 명시하세요.)".to_string());
                    }
                }
            }

            // offset 정규화 (1-based, 1 이상).
            let offset = offset.max(1);

            // 출력 상한을 줄 경계에서 지킨다 (푸터 예산 2,000자 제외).
            // limit(줄 수)와 char_budget(자 수)을 함께 적용한다.
            let char_budget = OUTPUT_CHAR_LIMIT as u64;
            let mut last_printed_line = offset;
            let mut buf = String::new();

            let start_idx = (offset - 1) as usize;
            for (i, line) in lines.iter().enumerate().skip(start_idx).take(limit as usize) {
                let line_no = (i as u64) + 1;
                let prefix_len = line_no.to_string().len() + 1; // "N→" 길이
                let line_char = prefix_len as u64 + line.chars().count() as u64;
                if buf.chars().count() as u64 + line_char > char_budget && !buf.is_empty() {
                    break;
                }
                buf.push_str(&format!("{line_no}→{line}\n"));
                last_printed_line = line_no;
            }

            // auto-advance 상태 갱신.
            {
                let mut st = state().lock().unwrap();
                let st = st.get_or_insert_with(ReadState::new);
                st.next_offset.insert(path_str.clone(), last_printed_line + 1);
                st.total_lines.insert(path_str.clone(), total);
            }

            // 푸터 (절단 상한보다 작은 예산 안에서).
            let footer = if last_printed_line >= total {
                format!(
                    "[File has {total} lines. Showing lines {offset}-{total}]"
                )
            } else {
                format!(
                    "[File has {total} lines. Showing lines {offset}-{last_printed_line}. Call read_file with offset={} to read more.]",
                    last_printed_line + 1
                )
            };
            buf.push_str(&footer);
            Ok(buf)
        })
    });
    reg.register(
        "read_file",
        "파일 읽기 (줄 번호 프리픽스, 200줄 페이징, offset으로 계속 읽기, 비전 시 이미지 반환)",
        schema(),
        handler,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dispatch(args: serde_json::Value) -> Result<String, String> {
        let mut reg = ToolRegistry::new(false);
        register(&mut reg);
        tokio::runtime::Runtime::new().unwrap().block_on(async move {
            reg.dispatch("read_file", args).await
        })
    }

    fn write_test_file(dir: &std::path::Path, name: &str, lines: usize) -> String {
        let content: String = (1..=lines).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let path = dir.join(name);
        std::fs::write(&path, content).unwrap();
        path.to_string_lossy().to_string()
    }

    #[test]
    fn lines_are_prefixed_with_numbers() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_test_file(dir.path(), "a.txt", 5);
        let out = dispatch(serde_json::json!({"path": p})).unwrap();
        assert!(out.contains("1→line 1"));
        assert!(out.contains("5→line 5"));
    }

    #[test]
    fn pagination_footer_suggests_offset() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_test_file(dir.path(), "b.txt", 300);
        let out = dispatch(serde_json::json!({"path": p})).unwrap();
        println!("PAGINATION OUTPUT:\n{out}");
        assert!(out.contains("Call read_file with offset=201 to read more"));
    }

    #[test]
    fn offset_reads_from_line() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_test_file(dir.path(), "c.txt", 300);
        let out = dispatch(serde_json::json!({"path": p, "offset": 201, "limit": 10})).unwrap();
        assert!(out.contains("201→line 201"));
        assert!(!out.contains("1→line 1"));
    }

    #[test]
    fn missing_file_is_error() {
        let res = dispatch(serde_json::json!({"path": "/nonexistent/xyz"}));
        assert!(res.is_err());
    }

    #[test]
    fn directory_is_error() {
        let dir = tempfile::tempdir().unwrap();
        let res = dispatch(serde_json::json!({"path": dir.path().to_string_lossy()}));
        assert!(res.is_err());
    }

    #[test]
    fn auto_advance_continues() {
        // 다른 테스트의 잔여 auto-advance 상태가 남아 있으면 경로 충돌이 생기므로
        // static STATE 를 초기화한다 (테스트 간 격리).
        *state().lock().unwrap() = None;
        let dir = tempfile::tempdir().unwrap();
        let p = write_test_file(dir.path(), "d.txt", 300);
        let first = dispatch(serde_json::json!({"path": p, "limit": 10})).unwrap();
        assert!(first.contains("1→line 1"));
        // offset 없이 다시 → auto-advance로 11부터.
        let second = dispatch(serde_json::json!({"path": p, "limit": 10})).unwrap();
        // 두 번째 호출은 11부터 시작해야 한다 (auto-advance). "1→line 1"은
        // "11→line 11"의 부분 문자열이므로, 대신 첫 줄이 11부터인지 검사한다.
        assert!(second.contains("11→line 11"));
        assert!(!second.contains("1→line 1\n"), "두 번째 호출이 1부터 다시 시작함");
        assert!(second.trim().starts_with("11→line 11"));
    }

    #[test]
    fn vision_image_returns_base64() {
        let mut reg = ToolRegistry::new(true);
        register(&mut reg);
        let dir = tempfile::tempdir().unwrap();
        // 1x1 PNG 바이트.
        let png: [u8; 164] = [
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00,
            0x00, 0x90, 0xBB, 0x33, 0x7A, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, 0x08,
            0xD7, 0x63, 0xF8, 0xCF, 0xC0, 0x00, 0x00, 0x00, 0x03, 0x00, 0x01, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        let img_path = dir.path().join("img.png");
        std::fs::write(&img_path, png).unwrap();
        let res = tokio::runtime::Runtime::new().unwrap().block_on(
            async move { reg.dispatch("read_file", serde_json::json!({"path": img_path.to_string_lossy()})).await },
        );
        assert!(res.is_ok());
        let out = res.unwrap();
        assert!(out.contains("data:image/png;base64,"));
    }
}
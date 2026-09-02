//! `bash` 네이티브 툴 (DESIGN.md §4.4).
//!
//! cwd는 프로젝트 루트로 고정. 셸 상태 유지 없음(세션 없음 원칙).
//! 출력 64KB 상한은 룬 경계에서 절단 (§4.5).

use std::process::Stdio;

use tokio::process::Command;

use crate::tools::util::{bool_arg, int_arg, str_arg, truncate_tool_result};
use crate::tools::{ToolHandler, ToolRegistry};

/// bash 도구 스키마.
pub fn schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "command": {
                "type": "string",
                "description": "실행할 셸 명령 (cwd는 프로젝트 루트 고정)"
            },
            "timeout": {
                "type": "integer",
                "description": "타임아웃(초), 기본 120"
            }
        },
        "required": ["command"]
    })
}

/// 출력 64KB 상한.
const MAX_OUTPUT_BYTES: usize = 64 * 1024;
/// 기본 타임아웃 (초).
const DEFAULT_TIMEOUT_SECS: u64 = 120;

/// bash 툴을 레지스트리에 등록한다.
pub fn register(reg: &mut ToolRegistry) {
    let handler: ToolHandler = std::sync::Arc::new(|args| {
        Box::pin(async move {
            let command = match str_arg(&args, "command") {
                Some(c) if !c.trim().is_empty() => c,
                _ => return Err("bash: command 인자가 필요합니다".to_string()),
            };
            let timeout = int_arg(&args, "timeout", DEFAULT_TIMEOUT_SECS);
            let _quiet = bool_arg(&args, "quiet", false);

            // cwd는 프로젝트 루트(현재 디렉터리)로 고정.
            let cwd = std::env::current_dir().map_err(|e| e.to_string())?;

            // Option<Child>로 감싸 move 문제를 회피한다.
            // `wait_with_output(self)`는 child를 move시키므로, 타임아웃 시
            // future가 취소되면 내부 child가 drop되고 kill_on_drop(true)로 자동 kill된다.
            let mut child_opt: Option<tokio::process::Child> = Some(
                Command::new("sh")
                    .arg("-c")
                    .arg(&command)
                    .current_dir(&cwd)
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .kill_on_drop(true)
                    .spawn()
                    .map_err(|e| format!("bash: 실행 실패: {e}"))?,
            );

            // 타임아웃: tokio::time::timeout으로 감싼다.
            let timed = tokio::time::timeout(
                std::time::Duration::from_secs(timeout),
                async {
                    let c = child_opt.take().unwrap();
                    c.wait_with_output().await
                },
            )
            .await;

            match timed {
                Err(_) => {
                    // 타임아웃 — async 블록 취소로 child가 drop되어
                    // kill_on_drop(true)가 프로세스를 정리한다.
                    Err(format!(
                        "bash: 타임아웃 ({timeout}초). 명령이 너무 오래 걸립니다: {command}"
                    ))
                }
                Ok(Err(e)) => Err(format!("bash: 실행 실패: {e}")),
                Ok(Ok(output)) => {
                    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

                    let mut body = String::new();
                    if !stdout.is_empty() {
                        body.push_str(&stdout);
                    }
                    if !stderr.is_empty() {
                        if !body.is_empty() {
                            body.push('\n');
                        }
                        body.push_str("[stderr]\n");
                        body.push_str(&stderr);
                    }
                    if let Some(code) = output.status.code() {
                        if code != 0 {
                            body.push_str(&format!("\n[exit code: {code}]"));
                        }
                    }
                    if body.is_empty() {
                        body.push_str("(명령이 출력을 반환하지 않았습니다)");
                    }

                    // 64KB 상한 — 룬 경계에서 절단.
                    let body = truncate_tool_result(&body, "bash", MAX_OUTPUT_BYTES);
                    Ok(body)
                }
            }
        })
    });
    reg.register("bash", "셸 명령 실행 (cwd: 프로젝트 루트, 세션 없음, 출력 64KB 상한)", schema(), handler);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dispatch(args: serde_json::Value) -> String {
        let mut reg = ToolRegistry::new(false);
        register(&mut reg);
        tokio::runtime::Runtime::new().unwrap().block_on(
            async move { reg.dispatch("bash", args).await.unwrap() },
        )
    }

    #[test]
    fn echo_works() {
        let out = dispatch(serde_json::json!({"command": "echo hello"}));
        assert!(out.contains("hello"));
    }

    #[test]
    fn stderr_is_captured() {
        let out = dispatch(serde_json::json!({"command": "echo err >&2"}));
        assert!(out.contains("[stderr]"));
        assert!(out.contains("err"));
    }

    #[test]
    fn exit_code_reported() {
        let out = dispatch(serde_json::json!({"command": "exit 3"}));
        assert!(out.contains("[exit code: 3]"));
    }

    #[test]
    fn missing_command_is_error() {
        let mut reg = ToolRegistry::new(false);
        register(&mut reg);
        let res = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(async move { reg.dispatch("bash", serde_json::json!({})).await });
        assert!(res.is_err());
    }

    #[test]
    fn cwd_is_project_root() {
        let out = dispatch(serde_json::json!({"command": "pwd"}));
        let cwd = std::env::current_dir().unwrap();
        assert!(out.contains(cwd.to_string_lossy().as_ref()));
    }
}
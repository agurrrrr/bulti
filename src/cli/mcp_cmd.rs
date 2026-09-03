//! `bulti mcp` 서브커맨드 구현 (list).
//!
//! - `mcp list`: config.toml `[mcp.*]` 에 등록된 MCP 서버 인덱스(이름+설명)를 출력.

use super::{McpArgs, McpCommand};
use crate::config::Config;

/// MCP 서브커맨드를 실행한다.
pub fn run(args: McpArgs, cfg: &Config) -> Result<i32, Box<dyn std::error::Error>> {
    match args.command {
        McpCommand::List => {
            if cfg.mcp.is_empty() {
                println!("(MCP 서버 없음)");
            } else {
                for (name, m) in &cfg.mcp {
                    let desc = m.description.as_deref().unwrap_or("(설명 없음)");
                    println!("{name} — {desc}");
                }
            }
            Ok(0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_args_list() {
        let args = McpArgs {
            command: McpCommand::List,
        };
        assert!(matches!(args.command, McpCommand::List));
    }
}
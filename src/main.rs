//! 불티(Bulti) — 컨텍스트 핸드오프 체인으로 긴 작업을 끝까지 완결하는 CLI 에이전트.
//!
//! 진입점: clap 파싱, 서브커맨드 디스패치, exit code 매핑 (DESIGN.md §4.12).

#![deny(clippy::all)]
#![deny(unsafe_code)]

mod agent;
mod cli;
mod config;
mod endpoint;
mod history;
mod llm;
mod mcp;
mod prompt;
mod skills;
mod tools;
mod update;

use std::process::ExitCode;

use clap::Parser;

use crate::cli::Cli;
use crate::config::Config;

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let mut cfg = match Config::load() {
        Ok(cfg) => cfg,
        Err(e) => {
            tracing::error!("설정 로드 실패: {e}");
            return ExitCode::from(1);
        }
    };

    match cli::dispatch(cli, &mut cfg) {
        Ok(code) => ExitCode::from(code as u8),
        Err(e) => {
            tracing::error!("실행 오류: {e}");
            ExitCode::from(1)
        }
    }
}

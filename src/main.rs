//! 불티(Bulti) — 컨텍스트 핸드오프 체인으로 긴 작업을 끝까지 완결하는 CLI 에이전트.
//!
//! 진입점: clap 파싱, 서브커맨드 디스패치, exit code 매핑 (DESIGN.md §4.12).
//! 로직은 `bulti` 라이브러리 크레이트에 있고, 여기서는 CLI 파싱과 디스패치만 담당한다.

use std::process::ExitCode;

use bulti::cli::{self, Cli};
use bulti::config::Config;
use clap::Parser;

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
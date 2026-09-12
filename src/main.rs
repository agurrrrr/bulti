//! 불티(Bulti) — 컨텍스트 핸드오프 체인으로 긴 작업을 끝까지 완결하는 CLI 에이전트.
//!
//! 진입점: clap 파싱, 서브커맨드 디스패치, exit code 매핑 (DESIGN.md §4.12).
//! 로직은 `bulti` 라이브러리 크레이트에 있고, 여기서는 CLI 파싱과 디스패치만 담당한다.

use std::io::Write;
use std::process::ExitCode;

use bulti::cli::{self, Cli};
use bulti::config::Config;
use clap::Parser;
use tracing_subscriber::fmt::MakeWriter;

/// TUI 가 켜져 있으면 tracing 을 버린다. raw mode 에서 stderr 로그가 화면을 깨뜨린다.
#[derive(Clone, Copy)]
struct TuiAwareWriter;

impl Write for TuiAwareWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if bulti::tui::is_active() {
            return Ok(buf.len());
        }
        std::io::stderr().write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if bulti::tui::is_active() {
            return Ok(());
        }
        std::io::stderr().flush()
    }
}

impl<'a> MakeWriter<'a> for TuiAwareWriter {
    type Writer = TuiAwareWriter;
    fn make_writer(&'a self) -> Self::Writer {
        *self
    }
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_writer(TuiAwareWriter)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let mut cfg = match Config::load() {
        Ok(cfg) => cfg,
        Err(e) => {
            tracing::error!(
                "{}",
                bulti::i18n::tr_fmt("Settings load failed: {e}", &[&e.to_string()])
            );
            return ExitCode::from(1);
        }
    };
    // 설정에 저장된 언어를 전역으로 적용한다 (기본 영어).
    bulti::i18n::set_language(cfg.language);

    match cli::dispatch(cli, &mut cfg) {
        Ok(code) => ExitCode::from(code as u8),
        Err(e) => {
            tracing::error!(
                "{}",
                bulti::i18n::tr_fmt("Execution error: {e}", &[&e.to_string()])
            );
            ExitCode::from(1)
        }
    }
}

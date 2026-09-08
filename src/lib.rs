//! 불티(Bulti) — 컨텍스트 핸드오프 체인으로 긴 작업을 끝까지 완결하는 CLI 에이전트.
//!
//! 라이브러리 크레이트: 모든 모듈을 `pub`으로 노출해 통합 테스트(`tests/`)와
//! 외부 오케스트레이션에서 재사용할 수 있게 한다 (DESIGN.md §4.12).

#![deny(clippy::all)]
#![deny(unsafe_code)]

pub mod agent;
pub mod cli;
pub mod completion;
pub mod config;
pub mod endpoint;
pub mod history;
pub mod llm;
pub mod mcp;
pub mod prompt;
pub mod render;
pub mod session;
pub mod skills;
pub mod slash;
pub mod tools;
pub mod tui;
pub mod update;
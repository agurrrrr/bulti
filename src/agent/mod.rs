//! run 오케스트레이션, 체인·세그먼트 관리 (DESIGN.md §4.3).
//!
//! - `context`: 토큰 추정·트리밍·절단 (§4.5)
//! - `guards`: 퇴행·거짓 완료·stuck 가드 (§5)

pub mod context;
pub mod guards;
pub mod handoff;
//! 서브커맨드 구현 (run, endpoint, history, skill, mcp, prompt, config, update, version).
//!
//! DESIGN.md §4.12 CLI 인터페이스를 따른다.

pub mod config_cmd;
pub mod endpoint_cmd;
pub mod history_cmd;
pub mod prompt_cmd;
pub mod skill_cmd;
pub mod version;

use clap::{Parser, Subcommand};

use crate::config::Config;

/// 불티(Bulti) — 컨텍스트 핸드오프 체인으로 긴 작업을 끝까지 완결하는 CLI 에이전트.
#[derive(Debug, Parser)]
#[command(
    name = "bulti",
    version,
    about = "불티(Bulti) CLI — 컨텍스트 핸드오프 체인 에이전트",
    subcommand_negates_reqs = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

/// 서브커맨드.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// 에이전트 실행 (체인 실행).
    Run(RunArgs),
    /// 엔드포인트 관리.
    Endpoint(EndpointArgs),
    /// 작업 히스토리 조회.
    History(HistoryArgs),
    /// 스킬 목록·조회.
    Skill(SkillArgs),
    /// MCP 서버 목록.
    Mcp(McpArgs),
    /// 시스템 프롬프트 관리.
    Prompt(PromptArgs),
    /// 설정 조회·수정.
    Config(ConfigArgs),
    /// GitHub 릴리즈 자동 업데이트.
    Update(UpdateArgs),
    /// 버전 출력.
    Version(VersionArgs),
}

#[derive(Debug, clap::Args)]
pub struct RunArgs {
    /// 프롬프트 (또는 `-` 로 stdin).
    #[arg(required = true)]
    pub prompt: String,
    /// 엔드포인트 이름.
    #[arg(long)]
    pub endpoint: Option<String>,
    /// 모델 이름 오버라이드.
    #[arg(long)]
    pub model: Option<String>,
    /// 시스템 프롬프트 파일.
    #[arg(long)]
    pub system_file: Option<String>,
    /// 인라인 시스템 프롬프트.
    #[arg(long)]
    pub system: Option<String>,
    /// JSON 보고서 출력.
    #[arg(long)]
    pub json: bool,
    /// 진행 출력 억제.
    #[arg(long)]
    pub quiet: bool,
    /// 색상 비활성화.
    #[arg(long)]
    pub no_color: bool,
    /// 최대 실행 시간(초).
    #[arg(long)]
    pub max_time: Option<u64>,
    /// 최대 핸드오프 깊이.
    #[arg(long)]
    pub max_handoff_depth: Option<u32>,
}

#[derive(Debug, clap::Args)]
pub struct EndpointArgs {
    #[command(subcommand)]
    pub command: EndpointCommand,
}

#[derive(Debug, clap::Subcommand)]
pub enum EndpointCommand {
    /// 엔드포인트 등록.
    Add(EndpointAddArgs),
    /// 엔드포인트 목록.
    List,
    /// 활성 엔드포인트 전환.
    Use { name: String },
    /// 엔드포인트 제거.
    Remove { name: String },
    /// 엔드포인트 필드 수정.
    Set(EndpointSetArgs),
    /// 연결·인증 확인.
    Test { name: String },
    /// 컨텍스트 길이 프로브.
    Probe { name: String },
}

#[derive(Debug, clap::Args)]
pub struct EndpointAddArgs {
    pub name: String,
    #[arg(long)]
    pub url: String,
    #[arg(long)]
    pub api_key: Option<String>,
    #[arg(long)]
    pub model: String,
    #[arg(long)]
    pub context_tokens: Option<u64>,
    #[arg(long)]
    pub vision: bool,
    #[arg(long)]
    pub thinking: bool,
}

#[derive(Debug, clap::Args)]
pub struct EndpointSetArgs {
    pub name: String,
    /// `key=value` 형태.
    pub field: String,
}

#[derive(Debug, clap::Args)]
pub struct HistoryArgs {
    #[command(subcommand)]
    pub command: HistoryCommand,
}

#[derive(Debug, clap::Subcommand)]
pub enum HistoryCommand {
    /// 최근 작업 목록.
    List(HistoryListArgs),
    /// 작업 상세 조회.
    Show { id: String },
    /// 마지막 작업 조회.
    Last(HistoryLastArgs),
}

#[derive(Debug, clap::Args)]
pub struct HistoryListArgs {
    /// 조회할 최근 개수.
    #[arg(short = 'n', long)]
    pub n: Option<u64>,
    /// 상태 필터 (running|completed|failed|incomplete|interrupted).
    #[arg(long)]
    pub status: Option<String>,
    /// 체인 ID 필터.
    #[arg(long)]
    pub chain: Option<String>,
}

#[derive(Debug, clap::Args)]
pub struct HistoryLastArgs {
    /// 해당 체인의 마지막 작업 조회.
    #[arg(long)]
    pub chain: bool,
}

#[derive(Debug, clap::Args)]
pub struct SkillArgs {
    #[command(subcommand)]
    pub command: SkillCommand,
}

#[derive(Debug, clap::Subcommand)]
pub enum SkillCommand {
    /// 스킬 목록.
    List,
    /// 스킬 상세 조회.
    Show { name: String },
}

#[derive(Debug, clap::Args)]
pub struct McpArgs {
    /// MCP 서버 목록.
    #[command(subcommand)]
    pub command: McpCommand,
}

#[derive(Debug, clap::Subcommand)]
pub enum McpCommand {
    /// MCP 서버 목록.
    List,
}

#[derive(Debug, clap::Args)]
pub struct PromptArgs {
    #[command(subcommand)]
    pub command: PromptCommand,
}

#[derive(Debug, clap::Subcommand)]
pub enum PromptCommand {
    /// 프롬프트 표시.
    Show,
    /// 프롬프트 편집.
    Edit,
}

#[derive(Debug, clap::Args)]
pub struct ConfigArgs {
    #[command(subcommand)]
    pub command: ConfigCommand,
}

#[derive(Debug, clap::Subcommand)]
pub enum ConfigCommand {
    /// 설정 값 조회.
    Get { key: String },
    /// 설정 값 수정.
    Set { key: String, value: String },
    /// 설정 전체 목록.
    List,
}

#[derive(Debug, clap::Args)]
pub struct UpdateArgs {
    /// 업데이트 확인만 수행.
    #[arg(long)]
    pub check: bool,
}

#[derive(Debug, clap::Args)]
pub struct VersionArgs {
    /// JSON 형태 버전 출력.
    #[arg(long)]
    pub json: bool,
}

/// 서브커맨드를 실제 동작으로 연결한다.
pub fn dispatch(cli: Cli, cfg: &mut Config) -> Result<i32, Box<dyn std::error::Error>> {
    match cli.command {
        Command::Version(args) => version::run(args),
        Command::Config(args) => config_cmd::run(args, cfg),
        // 이후 단계에서 구현할 서브커맨드. 단계 0 에서는 아직 미구현 안내.
        Command::Run(args) => {
            tracing::warn!("run 서브커맨드는 아직 구현되지 않았습니다 (단계 0)");
            let _ = args;
            Ok(1)
        }
        Command::Endpoint(args) => endpoint_cmd::run(args, cfg),
        Command::History(args) => history_cmd::run(args),
        Command::Skill(args) => skill_cmd::run(args),
        Command::Mcp(args) => {
            tracing::warn!("mcp 서브커맨드는 아직 구현되지 않았습니다 (단계 0)");
            let _ = args;
            Ok(1)
        }
        Command::Prompt(args) => prompt_cmd::run(args, cfg),
        Command::Update(args) => {
            tracing::warn!("update 서브커맨드는 아직 구현되지 않았습니다 (단계 0)");
            let _ = args;
            Ok(1)
        }
    }
}

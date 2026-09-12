//! 서브커맨드 구현 (run, endpoint, history, skill, mcp, prompt, config, update, version).
//!
//! DESIGN.md §4.12 CLI 인터페이스를 따른다.

pub mod chat_cmd;
pub mod config_cmd;
pub mod endpoint_cmd;
pub mod history_cmd;
pub mod mcp_cmd;
pub mod prompt_cmd;
pub mod run_cmd;
pub mod session_cmd;
pub mod skill_cmd;
pub mod update_cmd;
pub mod version;

use clap::{Parser, Subcommand};

use crate::config::Config;

/// bulti — a CLI agent that completes long tasks via a context handoff chain.
#[derive(Debug, Parser)]
#[command(
    name = "bulti",
    version,
    about = "bulti CLI — context handoff chain agent",
    subcommand_negates_reqs = true
)]
pub struct Cli {
    /// Subcommand. If omitted, enters interactive (chat) mode directly.
    #[command(subcommand)]
    pub command: Option<Command>,
    /// Interactive (chat) options — also usable at the top level without a subcommand.
    #[command(flatten)]
    pub chat: ChatArgs,
}

/// Subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Interactive chat/TUI mode.
    Chat(ChatArgs),
    /// Manage saved/resumed sessions.
    Session(SessionArgs),
    /// Run the agent (chain execution).
    Run(RunArgs),
    /// Manage endpoints.
    Endpoint(EndpointArgs),
    /// Query task history.
    History(HistoryArgs),
    /// List and view skills.
    Skill(SkillArgs),
    /// List MCP servers.
    Mcp(McpArgs),
    /// Manage the system prompt.
    Prompt(PromptArgs),
    /// Get and set configuration.
    Config(ConfigArgs),
    /// Automatic updates from GitHub releases.
    Update(UpdateArgs),
    /// Print the version.
    Version(VersionArgs),
}

#[derive(Debug, clap::Args, Default)]
pub struct ChatArgs {
    /// Endpoint name.
    #[arg(long)]
    pub endpoint: Option<String>,
    /// Model name override.
    #[arg(long)]
    pub model: Option<String>,
    /// System prompt file.
    #[arg(long)]
    pub system_file: Option<String>,
    /// Inline system prompt.
    #[arg(long)]
    pub system: Option<String>,
    /// Disable color.
    #[arg(long)]
    pub no_color: bool,
    /// Converse in stream text mode instead of ratatui (DESIGN.md §4.13.1).
    #[arg(long)]
    pub no_tui: bool,
    /// First prompt at startup (for non-interactive pipes).
    #[arg(long)]
    pub first: Option<String>,
    /// Session id to resume.
    #[arg(long)]
    pub resume: Option<String>,
}

#[derive(Debug, clap::Args)]
pub struct SessionArgs {
    #[command(subcommand)]
    pub command: SessionCommand,
}

#[derive(Debug, clap::Subcommand)]
pub enum SessionCommand {
    /// List sessions.
    List,
    /// Delete a session.
    Delete { id: String },
}

#[derive(Debug, clap::Args)]
pub struct RunArgs {
    /// Prompt (or `-` for stdin).
    #[arg(required = true)]
    pub prompt: String,
    /// Endpoint name.
    #[arg(long)]
    pub endpoint: Option<String>,
    /// Model name override.
    #[arg(long)]
    pub model: Option<String>,
    /// System prompt file.
    #[arg(long)]
    pub system_file: Option<String>,
    /// Inline system prompt.
    #[arg(long)]
    pub system: Option<String>,
    /// Output a JSON report.
    #[arg(long)]
    pub json: bool,
    /// Suppress progress output.
    #[arg(long)]
    pub quiet: bool,
    /// Disable color.
    #[arg(long)]
    pub no_color: bool,
    /// Maximum run time (seconds).
    #[arg(long)]
    pub max_time: Option<u64>,
    /// Maximum handoff depth.
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
    /// Register an endpoint.
    Add(EndpointAddArgs),
    /// List endpoints.
    List,
    /// Activate an endpoint.
    Use { name: String },
    /// Remove an endpoint.
    Remove { name: String },
    /// Modify an endpoint field.
    Set(EndpointSetArgs),
    /// Check connectivity and authentication.
    Test { name: String },
    /// Probe the context length.
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
    /// In `key=value` form.
    pub field: String,
}

#[derive(Debug, clap::Args)]
pub struct HistoryArgs {
    #[command(subcommand)]
    pub command: HistoryCommand,
}

#[derive(Debug, clap::Subcommand)]
pub enum HistoryCommand {
    /// List recent tasks.
    List(HistoryListArgs),
    /// Show task details.
    Show { id: String },
    /// Show the last task.
    Last(HistoryLastArgs),
}

#[derive(Debug, clap::Args)]
pub struct HistoryListArgs {
    /// Number of recent tasks to show.
    #[arg(short = 'n', long)]
    pub n: Option<u64>,
    /// Status filter (running|completed|failed|incomplete|interrupted).
    #[arg(long)]
    pub status: Option<String>,
    /// Chain id filter.
    #[arg(long)]
    pub chain: Option<String>,
}

#[derive(Debug, clap::Args)]
pub struct HistoryLastArgs {
    /// Show the last task of the chain.
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
    /// List skills.
    List,
    /// Show skill details.
    Show { name: String },
}

#[derive(Debug, clap::Args)]
pub struct McpArgs {
    /// List MCP servers.
    #[command(subcommand)]
    pub command: McpCommand,
}

#[derive(Debug, clap::Subcommand)]
pub enum McpCommand {
    /// List MCP servers.
    List,
}

#[derive(Debug, clap::Args)]
pub struct PromptArgs {
    #[command(subcommand)]
    pub command: PromptCommand,
}

#[derive(Debug, clap::Subcommand)]
pub enum PromptCommand {
    /// Show the prompt.
    Show,
    /// Edit the prompt.
    Edit,
}

#[derive(Debug, clap::Args)]
pub struct ConfigArgs {
    #[command(subcommand)]
    pub command: ConfigCommand,
}

#[derive(Debug, clap::Subcommand)]
pub enum ConfigCommand {
    /// Get a config value.
    Get { key: String },
    /// Set a config value.
    Set { key: String, value: String },
    /// List all config values.
    List,
}

#[derive(Debug, clap::Args)]
pub struct UpdateArgs {
    /// Only check for updates.
    #[arg(long)]
    pub check: bool,
}

#[derive(Debug, clap::Args)]
pub struct VersionArgs {
    /// Print the version as JSON.
    #[arg(long)]
    pub json: bool,
}

/// 서브커맨드를 실제 동작으로 연결한다. 서브커맨드가 없으면 대화형(chat) 모드로 진입한다.
pub fn dispatch(cli: Cli, cfg: &mut Config) -> Result<i32, Box<dyn std::error::Error>> {
    match cli.command {
        // 서브커맨드 없음 → 대화형 모드로 바로 진입.
        None => chat_cmd::run(cli.chat, cfg),
        Some(Command::Version(args)) => version::run(args),
        Some(Command::Config(args)) => config_cmd::run(args, cfg),
        // 이후 단계에서 구현할 서브커맨드. 단계 0 에서는 아직 미구현 안내.
        // run 시작 시 백그라운드 업데이트 확인 → stderr 알림 (DESIGN.md §4.11).
        Some(Command::Chat(args)) => chat_cmd::run(args, cfg),
        Some(Command::Session(args)) => session_cmd::run(args),
        Some(Command::Run(args)) => run_cmd::run(args, cfg),
        Some(Command::Endpoint(args)) => endpoint_cmd::run(args, cfg),
        Some(Command::History(args)) => history_cmd::run(args),
        Some(Command::Skill(args)) => skill_cmd::run(args),
        Some(Command::Mcp(args)) => mcp_cmd::run(args, cfg),
        Some(Command::Prompt(args)) => prompt_cmd::run(args, cfg),
        Some(Command::Update(args)) => update_cmd::run(args, cfg),
    }
}

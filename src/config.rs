//! 설정 시스템 — `~/.bulti/config.toml` 로드·저장 (serde + toml).
//!
//! 스키마는 DESIGN.md §3.1 을 따른다:
//! `version`, `active_endpoint`, `[endpoints.*]`, `[mcp.*]`, `[context]`, `[update]`

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// 설정 파일 버전.
pub const CONFIG_VERSION: u32 = 1;

/// 설정 파일 이름.
pub const CONFIG_FILENAME: &str = "config.toml";

/// 설정 디렉터리 이름.
pub const CONFIG_DIR_NAME: &str = ".bulti";

/// 로드·저장 오류.
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("설정 디렉터리를 찾을 수 없습니다: {0}")]
    HomeDirNotFound(String),
    #[error("설정 파일을 읽는 데 실패했습니다: {0}")]
    Io(#[from] std::io::Error),
    #[error("TOML 파싱에 실패했습니다: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("TOML 직렬화에 실패했습니다: {0}")]
    TomlSer(#[from] toml::ser::Error),
}

/// 엔드포인트 설정 (§3.1 `[endpoints.*]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointConfig {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    pub model: String,
    /// 0 이면 자동 프로브 (§4.1.2).
    #[serde(default = "default_context_tokens")]
    pub context_tokens: u64,
    /// 비전 가능 모델 토글.
    #[serde(default)]
    pub vision: bool,
    /// reasoning_content 표시·기록 여부.
    #[serde(default)]
    pub thinking: bool,
    /// 세그먼트당 도구 호출 턴 상한.
    #[serde(default = "default_max_iterations")]
    pub max_iterations: u32,
}

fn default_context_tokens() -> u64 {
    0
}

fn default_max_iterations() -> u32 {
    200
}

/// MCP 서버 설정 (§3.1 `[mcp.*]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// 컨텍스트 핸드오프 설정 (§3.1 `[context]`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextConfig {
    /// 추정 토큰이 ctx 의 이 비율을 넘으면 핸드오프 트리거.
    #[serde(default = "default_handoff_threshold_pct")]
    pub handoff_threshold_pct: u8,
    /// 체인 깊이 상한 (런어웨이 가드).
    #[serde(default = "default_max_handoff_depth")]
    pub max_handoff_depth: u32,
    /// 경고 시작 깊이.
    #[serde(default = "default_handoff_warn_depth")]
    pub handoff_warn_depth: u32,
}

fn default_handoff_threshold_pct() -> u8 {
    75
}

fn default_max_handoff_depth() -> u32 {
    12
}

fn default_handoff_warn_depth() -> u32 {
    8
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            handoff_threshold_pct: default_handoff_threshold_pct(),
            max_handoff_depth: default_max_handoff_depth(),
            handoff_warn_depth: default_handoff_warn_depth(),
        }
    }
}

/// GitHub 릴리즈 자동 업데이트 설정 (§3.1 `[update]`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateConfig {
    pub repo: String,
    /// check(알림만) | download(자동 다운로드+교체) | off
    pub mode: UpdateMode,
}

/// 업데이트 모드.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UpdateMode {
    /// 알림만.
    #[default]
    Check,
    /// 자동 다운로드+교체.
    Download,
    /// 비활성화.
    Off,
}

impl std::fmt::Display for UpdateMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Check => "check",
            Self::Download => "download",
            Self::Off => "off",
        };
        f.write_str(s)
    }
}

/// 전체 설정 루트.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "config_version")]
    pub version: u32,
    #[serde(default)]
    pub active_endpoint: Option<String>,
    #[serde(default)]
    pub endpoints: std::collections::BTreeMap<String, EndpointConfig>,
    #[serde(default)]
    pub mcp: std::collections::BTreeMap<String, McpConfig>,
    #[serde(default)]
    pub context: ContextConfig,
    #[serde(default)]
    pub update: Option<UpdateConfig>,
}

fn config_version() -> u32 {
    CONFIG_VERSION
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            active_endpoint: None,
            endpoints: std::collections::BTreeMap::new(),
            mcp: std::collections::BTreeMap::new(),
            context: ContextConfig::default(),
            update: None,
        }
    }
}

impl Config {
    /// 기본 설정 인스턴스를 만든다.
    #[allow(dead_code)]
    pub fn new() -> Self {
        Self::default()
    }

    /// `~/.bulti` 설정 디렉터리 경로를 반환한다.
    pub fn config_dir() -> Result<PathBuf, ConfigError> {
        dirs::home_dir()
            .map(|home| home.join(CONFIG_DIR_NAME))
            .ok_or_else(|| ConfigError::HomeDirNotFound("홈 디렉터리 미해석".into()))
    }

    /// `~/.bulti/config.toml` 경로를 반환한다.
    pub fn config_path() -> Result<PathBuf, ConfigError> {
        Ok(Self::config_dir()?.join(CONFIG_FILENAME))
    }

    /// 설정 파일을 로드한다. 파일이 없으면 기본 설정을 반환한다.
    pub fn load() -> Result<Self, ConfigError> {
        let path = Self::config_path()?;
        Self::load_from(&path)
    }

    /// 지정된 경로에서 설정을 로드한다. 파일이 없으면 기본 설정을 반환한다.
    pub fn load_from(path: &Path) -> Result<Self, ConfigError> {
        match fs::read_to_string(path) {
            Ok(text) => {
                let cfg: Self = toml::from_str(&text)?;
                Ok(cfg)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(ConfigError::Io(e)),
        }
    }

    /// 설정을 기본 경로(`~/.bulti/config.toml`)에 저장한다.
    pub fn save(&self) -> Result<PathBuf, ConfigError> {
        let path = Self::config_path()?;
        self.save_to(&path)?;
        Ok(path)
    }

    /// 설정을 지정된 경로에 저장한다. 디렉터리가 없으면 생성한다.
    pub fn save_to(&self, path: &Path) -> Result<(), ConfigError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let text = toml::to_string(self)?;
        fs::write(path, text)?;
        Ok(())
    }

    /// TOML 문자열로 직렬화한다.
    #[allow(dead_code)]
    pub fn to_toml(&self) -> Result<String, ConfigError> {
        Ok(toml::to_string(self)?)
    }

    /// TOML 문자열에서 역직렬화한다.
    #[allow(dead_code)]
    pub fn from_toml(text: &str) -> Result<Self, ConfigError> {
        Ok(toml::from_str(text)?)
    }

    /// 활성 엔드포인트 설정을 반환한다.
    #[allow(dead_code)]
    pub fn active_endpoint_config(&self) -> Option<(&str, &EndpointConfig)> {
        let name = self.active_endpoint.as_deref()?;
        self.endpoints.get(name).map(|cfg| (name, cfg))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn sample_config() -> Config {
        Config {
            version: 1,
            active_endpoint: Some("main".to_string()),
            endpoints: std::collections::BTreeMap::from([(
                "main".to_string(),
                EndpointConfig {
                    url: "http://127.0.0.1:8084/v1".to_string(),
                    api_key: Some("sk-test".to_string()),
                    model: "qwen3.8-27b-q2".to_string(),
                    context_tokens: 0,
                    vision: true,
                    thinking: true,
                    max_iterations: 200,
                },
            )]),
            mcp: std::collections::BTreeMap::from([(
                "files".to_string(),
                McpConfig {
                    command: "npx".to_string(),
                    args: vec![
                        "-y".to_string(),
                        "@modelcontextprotocol/server-filesystem".to_string(),
                        "/home/me".to_string(),
                    ],
                    description: Some("파일시스템 접근".to_string()),
                },
            )]),
            context: ContextConfig {
                handoff_threshold_pct: 75,
                max_handoff_depth: 12,
                handoff_warn_depth: 8,
            },
            update: Some(UpdateConfig {
                repo: "agurrrrr/bulti".to_string(),
                mode: UpdateMode::Check,
            }),
        }
    }

    #[test]
    fn roundtrip_toml_string() {
        let cfg = sample_config();
        let text = cfg.to_toml().expect("직렬화");
        let parsed = Config::from_toml(&text).expect("역직렬화");
        assert_eq!(toml::to_string(&parsed).unwrap(), text);
        assert_eq!(parsed.version, CONFIG_VERSION);
        assert_eq!(parsed.active_endpoint.as_deref(), Some("main"));
        assert_eq!(parsed.endpoints["main"].model, "qwen3.8-27b-q2");
        assert_eq!(parsed.endpoints["main"].api_key.as_deref(), Some("sk-test"));
        assert_eq!(parsed.mcp["files"].command, "npx");
        assert_eq!(parsed.context.handoff_threshold_pct, 75);
        assert_eq!(parsed.update.as_ref().unwrap().repo, "agurrrrr/bulti");
    }

    #[test]
    fn roundtrip_file_save_load() {
        let dir = tempfile::tempdir().expect("임시 디렉터리");
        let path = dir.path().join("config.toml");
        let cfg = sample_config();
        cfg.save_to(&path).expect("저장");
        assert!(path.exists());

        let loaded = Config::load_from(&path).expect("로드");
        assert_eq!(loaded.version, cfg.version);
        assert_eq!(loaded.active_endpoint, cfg.active_endpoint);
        assert_eq!(loaded.endpoints, cfg.endpoints);
        assert_eq!(loaded.mcp, cfg.mcp);
        assert_eq!(
            loaded.context.handoff_threshold_pct,
            cfg.context.handoff_threshold_pct
        );
        assert_eq!(
            loaded.update.as_ref().unwrap().mode,
            cfg.update.as_ref().unwrap().mode
        );
    }

    #[test]
    fn load_missing_returns_default() {
        let dir = tempfile::tempdir().expect("임시 디렉터리");
        let path = dir.path().join("nope.toml");
        let cfg = Config::load_from(&path).expect("기본 반환");
        assert_eq!(cfg.version, CONFIG_VERSION);
        assert!(cfg.endpoints.is_empty());
    }

    #[test]
    fn default_has_sane_values() {
        let cfg = Config::new();
        assert_eq!(cfg.version, CONFIG_VERSION);
        assert_eq!(cfg.context.handoff_threshold_pct, 75);
        assert_eq!(cfg.context.max_handoff_depth, 12);
        assert_eq!(cfg.context.handoff_warn_depth, 8);
        assert!(cfg.active_endpoint.is_none());
    }

    #[test]
    fn active_endpoint_config_returns_none_when_unset() {
        let cfg = Config::new();
        assert!(cfg.active_endpoint_config().is_none());
    }
}

//! `bulti prompt` 서브커맨드 구현 (show/edit).
//!
//! - `prompt show`: 최종 조립 결과를 그대로 출력 (디버깅·검증).
//! - `prompt edit`: 글로벌 파일(`~/.bulti/prompts/default.md`)을 $EDITOR로 연다.

use std::path::PathBuf;

use super::{PromptArgs, PromptCommand};
use crate::config::Config;
use crate::prompt;

/// 프롬프트 서브커맨드를 실행한다.
pub fn run(args: PromptArgs, cfg: &Config) -> Result<i32, Box<dyn std::error::Error>> {
    let cwd = std::env::current_dir()?;
    let project_root = cwd.clone();
    let global_dir = Config::config_dir().map_err(|e| e.to_string())?;
    let skills = crate::skills::discover(&project_root, &global_dir)?;
    let ctx = prompt::context_from_config(cfg, cwd, project_root, skills, vec![])?;

    match args.command {
        PromptCommand::Show => {
            let out = prompt::show(&ctx, None)?;
            print!("{out}");
            Ok(0)
        }
        PromptCommand::Edit => {
            prompt::edit(&ctx)?;
            println!("글로벌 프롬프트 편집 완료");
            Ok(0)
        }
    }
}

/// RunArgs 의 `--system-file` / `--system` 을 `prompt::Override` 로 변환한다.
pub fn override_from_run_args(
    system_file: &Option<String>,
    system: &Option<String>,
) -> Option<prompt::Override> {
    match (system_file, system) {
        (Some(path), _) => Some(prompt::Override::File(PathBuf::from(path))),
        (None, Some(text)) => Some(prompt::Override::Inline(text.clone())),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn override_prefers_file_over_inline() {
        let o = override_from_run_args(&Some("f.md".to_string()), &Some("text".to_string()));
        match o {
            Some(prompt::Override::File(p)) => assert_eq!(p, PathBuf::from("f.md")),
            _ => panic!("파일 우선이어야 함"),
        }
    }

    #[test]
    fn override_inline_only() {
        let o = override_from_run_args(&None, &Some("text".to_string()));
        match o {
            Some(prompt::Override::Inline(t)) => assert_eq!(t, "text"),
            _ => panic!("인라인"),
        }
    }

    #[test]
    fn override_none() {
        assert!(override_from_run_args(&None, &None).is_none());
    }
}
//! `bulti skill` 서브커맨드 구현 (list/show).
//!
//! - `skill list`: 프로젝트 → 글로벌 → 번들 순으로 발견된 스킬 인덱스를 출력.
//! - `skill show <name>`: 스킬 본문 전체를 출력.

use super::{SkillArgs, SkillCommand};
use crate::config::Config;
use crate::skills;

/// 스킬 서브커맨드를 실행한다.
pub fn run(args: SkillArgs) -> Result<i32, Box<dyn std::error::Error>> {
    let cwd = std::env::current_dir()?;
    let global_dir = Config::config_dir().map_err(|e| e.to_string())?;

    match args.command {
        SkillCommand::List => {
            let indices = skills::discover(&cwd, &global_dir)?;
            if indices.is_empty() {
                println!("(스킬 없음)");
            } else {
                for s in &indices {
                    println!("{} — {}", s.name, s.description);
                }
            }
            Ok(0)
        }
        SkillCommand::Show { name } => {
            let body = skills::load(&name, &cwd, &global_dir)?;
            print!("{body}");
            Ok(0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_args_list() {
        let args = SkillArgs {
            command: SkillCommand::List,
        };
        assert!(matches!(args.command, SkillCommand::List));
    }

    #[test]
    fn skill_args_show() {
        let args = SkillArgs {
            command: SkillCommand::Show {
                name: "korean-report".to_string(),
            },
        };
        match args.command {
            SkillCommand::Show { name } => assert_eq!(name, "korean-report"),
            _ => panic!("show"),
        }
    }
}
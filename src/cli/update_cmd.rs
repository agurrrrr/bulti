//! `bulti update` 서브커맨드 구현 (DESIGN.md §4.11).

use super::UpdateArgs;
use crate::config::{Config, UpdateMode};
use crate::update;

/// update 서브커맨드를 실행한다.
pub fn run(args: UpdateArgs, cfg: &Config) -> Result<i32, Box<dyn std::error::Error>> {
    let (repo, mode) = update_config(cfg);
    if args.check {
        update::check_only(&repo, &mode).map_err(|e| e.into())
    } else {
        update::run_update(&repo, &mode).map_err(|e| e.into())
    }
}

/// 설정에서 repo·mode 를 가져온다. 기본값은 빌드 시 주입된 저장소와 check 모드.
pub fn update_config(cfg: &Config) -> (String, UpdateMode) {
    match &cfg.update {
        Some(u) => (u.repo.clone(), u.mode.clone()),
        None => (update::DEFAULT_REPO.to_string(), UpdateMode::Check),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_uses_builtin_repo() {
        let cfg = Config::new();
        let (repo, mode) = update_config(&cfg);
        assert_eq!(repo, update::DEFAULT_REPO);
        assert_eq!(mode, UpdateMode::Check);
    }

    #[test]
    fn configured_repo_is_used() {
        let mut cfg = Config::new();
        cfg.update = Some(crate::config::UpdateConfig {
            repo: "owner/repo".to_string(),
            mode: UpdateMode::Download,
        });
        let (repo, mode) = update_config(&cfg);
        assert_eq!(repo, "owner/repo");
        assert_eq!(mode, UpdateMode::Download);
    }
}
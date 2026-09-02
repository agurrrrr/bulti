//! 레이지 스킬 발견·로딩 (DESIGN.md §4.8).
//!
//! - **발견 순서**: 프로젝트 `.bulti/skills/` → 글로벌 `~/.bulti/skills/`.
//!   동명이면 프로젝트가 우선한다. 번들 스킬은 항상 인덱스에 포함되며
//!   프로젝트·글로벌보다 낮은 우선순위를 가진다.
//! - **형식**: 마크다운 + YAML frontmatter(`name`, `description`).
//!   단일 파일 `<name>.md` 또는 디렉터리 `<name>/SKILL.md`.
//! - **시스템 프롬프트에는 인덱스만** 들어간다 (이름 + 설명 1행/스킬).
//!   본문은 절대 자동 주입하지 않는다.
//! - **`skill_load(name)` 도구**: 본문 전체를 반환한다. 모델이 필요할 때만 로딩.

use std::fs;
use std::path::{Path, PathBuf};

use thiserror::Error;

/// 스킬 디렉터리 이름 (프로젝트·글로벌 공통).
pub const SKILLS_DIR_NAME: &str = "skills";

/// 디렉터리 스킬의 진입 파일 이름.
pub const SKILL_FILENAME: &str = "SKILL.md";

/// 스킬 발견·로딩 오류.
#[derive(Debug, Error)]
pub enum SkillError {
    #[error("스킬 디렉터리를 읽는 데 실패했습니다: {0}")]
    Io(#[from] std::io::Error),
    #[error("스킬을 찾을 수 없습니다: {name}")]
    NotFound { name: String },
    #[error("스킬 파일에 frontmatter가 없습니다: {0}")]
    MissingFrontmatter(String),
    #[error("스킬 frontmatter에 name/description이 없습니다: {0}")]
    MissingField(String),
}

/// 발견된 스킬 하나의 인덱스 항목 (이름 + 설명).
#[derive(Debug, Clone)]
pub struct SkillIndex {
    pub name: String,
    pub description: String,
}

/// 스킬 본문의 출처.
#[derive(Debug, Clone)]
enum SkillSource {
    /// 파일 시스템 스킬 (프로젝트 또는 글로벌).
    File(PathBuf),
    /// 번들 스킬 (바이너리 내장).
    Bundle(&'static str),
}

/// 발견된 스킬 (인덱스 + 본문 출처).
#[derive(Debug, Clone)]
struct Skill {
    index: SkillIndex,
    source: SkillSource,
}

/// 번들 스킬 파일 모음 (include_str! 로 내장).
const BUNDLED: &[(&str, &str)] = &[
    ("korean-report", include_str!("bundled/korean-report.md")),
    ("commit-message", include_str!("bundled/commit-message.md")),
];

/// 프로젝트 스킬 디렉터리 경로 (`<project_root>/.bulti/skills`).
pub fn project_skills_dir(project_root: &Path) -> PathBuf {
    project_root.join(".bulti").join(SKILLS_DIR_NAME)
}

/// 글로벌 스킬 디렉터리 경로 (`<global_dir>/skills`).
pub fn global_skills_dir(global_dir: &Path) -> PathBuf {
    global_dir.join(SKILLS_DIR_NAME)
}

/// 디렉터리에서 스킬을 발견한다. 파일·디렉터리 형식을 모두 지원한다.
///
/// - 단일 파일: `<dir>/<name>.md`
/// - 디렉터리: `<dir>/<name>/SKILL.md`
fn discover_in_dir(dir: &Path) -> Result<Vec<Skill>, SkillError> {
    let mut skills = Vec::new();
    if !dir.is_dir() {
        return Ok(skills);
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let file_name = entry.file_name().to_string_lossy().to_string();

        // 스킬 진입 파일 경로를 결정한다.
        let entry_path = if path.is_file() && file_name.ends_with(".md") {
            // 단일 파일 스킬: `<name>.md` → 이름은 확장자 제거.
            let name = file_name.trim_end_matches(".md").to_string();
            Some((name, path))
        } else if path.is_dir() {
            // 디렉터리 스킬: `<name>/SKILL.md` → 이름은 디렉터리 이름.
            let skill_file = path.join(SKILL_FILENAME);
            if skill_file.is_file() {
                Some((file_name, skill_file))
            } else {
                None
            }
        } else {
            None
        };

        if let Some((name, skill_file)) = entry_path {
            if let Some(index) = parse_skill_file(&name, &skill_file)? {
                skills.push(Skill {
                    index,
                    source: SkillSource::File(skill_file),
                });
            }
        }
    }
    Ok(skills)
}

/// 스킬 파일을 파싱해 인덱스 항목을 만든다. frontmatter가 없으면 건너뛴다.
fn parse_skill_file(name: &str, path: &Path) -> Result<Option<SkillIndex>, SkillError> {
    let body = fs::read_to_string(path)?;
    match parse_frontmatter(&body) {
        Ok((n, desc)) => Ok(Some(SkillIndex {
            name: n,
            description: desc,
        })),
        Err(e) => {
            tracing::debug!("스킬 파일 파싱 실패 ({}): {name} — {e}", path.display());
            Ok(None)
        }
    }
}

/// 전체 스킬 발견 (프로젝트 → 글로벌 → 번들). 동명이면 프로젝트가 우선한다.
pub fn discover(project_root: &Path, global_dir: &Path) -> Result<Vec<SkillIndex>, SkillError> {
    let mut skills: Vec<Skill> = Vec::new();

    // 프로젝트 스킬.
    skills.extend(discover_in_dir(&project_skills_dir(project_root))?);
    // 글로벌 스킬.
    skills.extend(discover_in_dir(&global_skills_dir(global_dir))?);

    // 동명 중복 제거: 프로젝트가 먼저 발견되므로 앞 항목을 유지한다.
    let mut seen = std::collections::HashSet::new();
    skills.retain(|s| seen.insert(s.index.name.clone()));

    // 번들 스킬 (프로젝트·글로벌에 동명이 없을 때만 추가).
    for (name, body) in BUNDLED {
        if let Ok((n, desc)) = parse_frontmatter(body) {
            if !seen.contains(&n) {
                skills.push(Skill {
                    index: SkillIndex {
                        name: n.to_string(),
                        description: desc,
                    },
                    source: SkillSource::Bundle(name),
                });
                seen.insert(n);
            }
        }
    }

    // 인덱스만 추출해 이름 순으로 정렬.
    let mut out: Vec<SkillIndex> = skills.into_iter().map(|s| s.index).collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// 스킬 본문 전체를 로드한다. 없는 스킬이면 `NotFound` 오류를 반환한다.
pub fn load(name: &str, project_root: &Path, global_dir: &Path) -> Result<String, SkillError> {
    // 프로젝트 → 글로벌 → 번들 순으로 찾는다.
    for dir in [
        project_skills_dir(project_root),
        global_skills_dir(global_dir),
    ] {
        // 단일 파일: `<dir>/<name>.md`
        let single = dir.join(format!("{name}.md"));
        if single.is_file() {
            return fs::read_to_string(&single).map_err(SkillError::from);
        }
        // 디렉터리: `<dir>/<name>/SKILL.md`
        let dir_skill = dir.join(name).join(SKILL_FILENAME);
        if dir_skill.is_file() {
            return fs::read_to_string(&dir_skill).map_err(SkillError::from);
        }
    }
    // 번들 스킬.
    if let Some((_, body)) = BUNDLED.iter().find(|(n, _)| *n == name) {
        return Ok((*body).to_string());
    }
    Err(SkillError::NotFound {
        name: name.to_string(),
    })
}

/// 마크다운 본문에서 YAML frontmatter(`name`, `description`)를 파싱한다.
///
/// `---` 로 감싼 블록에서 `name:`과 `description:` 값을 추출한다.
pub fn parse_frontmatter(body: &str) -> Result<(String, String), SkillError> {
    let body = body.strip_prefix('\u{feff}').unwrap_or(body); // BOM 제거.
    let trimmed = body.trim_start();
    if !trimmed.starts_with("---") {
        return Err(SkillError::MissingFrontmatter("--- 로 시작해야 함".into()));
    }
    // 첫 `---` 이후부터 다음 `---` 까지를 frontmatter 블록으로 본다.
    let after_open = &trimmed[3..];
    let close_pos = after_open.find("\n---").ok_or_else(|| {
        SkillError::MissingFrontmatter("닫는 --- 를 찾을 수 없음".into())
    })?;
    let block = &after_open[..close_pos];

    let mut name: Option<String> = None;
    let mut description: Option<String> = None;
    for line in block.lines() {
        let line = line.trim();
        if let Some(v) = line.strip_prefix("name:") {
            name = Some(v.trim().trim_matches('"').to_string());
        } else if let Some(v) = line.strip_prefix("description:") {
            description = Some(v.trim().trim_matches('"').to_string());
        }
    }
    let name = name.ok_or_else(|| SkillError::MissingField("name".into()))?;
    let description =
        description.ok_or_else(|| SkillError::MissingField("description".into()))?;
    Ok((name, description))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill(dir: &Path, name: &str, front: &str, body: &str) -> PathBuf {
        let path = dir.join(format!("{name}.md"));
        fs::write(&path, format!("{front}\n{body}")).unwrap();
        path
    }

    /// 테스트별 고유 임시 디렉터리. 병렬 실행 시 테스트·역할(root/global)별로
    /// 서로 다른 디렉터리를 사용해 충돌을 방지한다.
    fn temp_dir(label: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "bulti_skill_test_{}_{}",
            std::process::id(),
            label
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn parses_frontmatter() {
        let body = "---\nname: korean-report\ndescription: 한국어 보고 스타일\n---\n본문";
        let (name, desc) = parse_frontmatter(body).unwrap();
        assert_eq!(name, "korean-report");
        assert_eq!(desc, "한국어 보고 스타일");
    }

    #[test]
    fn frontmatter_quotes_trimmed() {
        let body = "---\nname: \"commit-message\"\ndescription: \"커밋 메시지 규약\"\n---\n본문";
        let (name, desc) = parse_frontmatter(body).unwrap();
        assert_eq!(name, "commit-message");
        assert_eq!(desc, "커밋 메시지 규약");
    }

    #[test]
    fn missing_frontmatter_errors() {
        let body = "본문만 있는 스킬";
        assert!(parse_frontmatter(body).is_err());
    }

    #[test]
    fn missing_field_errors() {
        let body = "---\nname: only-name\n---\n본문";
        assert!(parse_frontmatter(body).is_err());
    }

    #[test]
    fn discover_project_then_global() {
        let root = temp_dir("discover_root");
        let global = temp_dir("discover_global");
        let proj = project_skills_dir(&root);
        let glob = global_skills_dir(&global);
        fs::create_dir_all(&proj).unwrap();
        fs::create_dir_all(&glob).unwrap();

        // 글로벌에만 있는 스킬.
        write_skill(
            &glob,
            "global-only",
            "---\nname: global-only\ndescription: 글로벌 전용\n---",
            "글로벌 본문",
        );
        // 프로젝트·글로벌에 동명으로 있는 스킬.
        write_skill(
            &proj,
            "same-name",
            "---\nname: same-name\ndescription: 프로젝트 버전\n---",
            "프로젝트 본문",
        );
        write_skill(
            &glob,
            "same-name",
            "---\nname: same-name\ndescription: 글로벌 버전\n---",
            "글로벌 본문",
        );

        let indices = discover(&root, &global).unwrap();
        let names: Vec<&str> = indices.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"global-only"));
        assert!(names.contains(&"same-name"));

        // 동명 스킬은 프로젝트가 우선한다.
        let same = indices.iter().find(|s| s.name == "same-name").unwrap();
        assert_eq!(same.description, "프로젝트 버전");

        // 로드도 프로젝트 우선.
        let loaded = load("same-name", &root, &global).unwrap();
        assert!(loaded.contains("프로젝트 본문"));

        // 번들 스킬이 포함된다.
        assert!(names.contains(&"korean-report"));
        assert!(names.contains(&"commit-message"));

        fs::remove_dir_all(&root).unwrap();
        fs::remove_dir_all(&global).unwrap();
    }

    #[test]
    fn load_bundled_skill() {
        let root = temp_dir("bundled_root");
        let global = temp_dir("bundled_global");
        let loaded = load("korean-report", &root, &global).unwrap();
        assert!(loaded.contains("한국어 보고"));
        fs::remove_dir_all(&root).unwrap();
        fs::remove_dir_all(&global).unwrap();
    }

    #[test]
    fn load_missing_skill_errors() {
        let root = temp_dir("missing_root");
        let global = temp_dir("missing_global");
        let err = load("nonexistent", &root, &global).unwrap_err();
        assert!(matches!(err, SkillError::NotFound { .. }));
        fs::remove_dir_all(&root).unwrap();
        fs::remove_dir_all(&global).unwrap();
    }

    #[test]
    fn directory_skill_discovered() {
        let root = temp_dir("directory_root");
        let proj = project_skills_dir(&root);
        let dir_skill = proj.join("res-skill");
        fs::create_dir_all(&dir_skill).unwrap();
        fs::write(
            dir_skill.join(SKILL_FILENAME),
            "---\nname: res-skill\ndescription: 리소스 동반 스킬\n---\n본문",
        )
        .unwrap();
        // 리소스 파일.
        fs::write(dir_skill.join("asset.txt"), "asset").unwrap();

        let indices = discover(&root, &root).unwrap();
        let skill = indices.iter().find(|s| s.name == "res-skill").unwrap();
        assert_eq!(skill.description, "리소스 동반 스킬");

        let loaded = load("res-skill", &root, &root).unwrap();
        assert!(loaded.contains("본문"));

        fs::remove_dir_all(&root).unwrap();
    }
}
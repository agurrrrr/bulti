//! GitHub 릴리즈 확인·자가 교체 (DESIGN.md §4.11).
//!
//! - `check`: latest 릴리즈를 조회해 현재 버전과 semver 비교, 알림만.
//! - `download`: asset 다운로드 → sha256 검증 → 임시 해제 → exit-time self-replace.
//! - `off`: 비활성화.
//! - `run` 시작 시 백그라운드 확인 → stderr 한 줄 알림.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context as _, Result};
use serde::{Deserialize, Serialize};

use crate::config::{UpdateMode, CONFIG_DIR_NAME};

pub mod semver_util;
pub mod self_replace;

/// 빌드 시 주입되는 기본 저장소 (DESIGN.md §4.11). `BULTI_REPO` env 가 있으면 그 값을 사용한다.
pub const DEFAULT_REPO: &str = match option_env!("BULTI_REPO") {
    Some(r) => r,
    None => "agurrrrr/bulti",
};

/// 릴리즈 확인 캐시 파일 이름 (`~/.bulti/update.json`).
pub const UPDATE_CACHE_FILENAME: &str = "update.json";

/// 재확인 주기 (24시간).
pub const CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// 릴리즈 확인 캐시 구조.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateCache {
    pub etag: String,
    pub checked_at: String,
    pub latest_tag: String,
}

/// GitHub Releases API 응답 (최소 필드).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Release {
    pub tag_name: String,
    pub assets: Vec<Asset>,
}

/// 릴리즈 asset.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Asset {
    pub name: String,
    pub browser_download_url: String,
}

/// 확인 결과.
#[derive(Debug, Clone)]
pub enum CheckResult {
    /// 최신 버전이 이미 설치됨.
    UpToDate,
    /// 새 버전 사용 가능.
    UpdateAvailable { latest: String },
    /// 확인 실패 (네트워크 등) — 조용히 무시.
    Failed,
}

/// update.json 캐시 경로.
pub fn cache_path() -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("홈 디렉터리 미해석"))?;
    Ok(home.join(CONFIG_DIR_NAME).join(UPDATE_CACHE_FILENAME))
}

/// 캐시를 읽는다. 없거나 깨졌으면 None.
pub fn load_cache() -> Option<UpdateCache> {
    let path = cache_path().ok()?;
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// 캐시를 저장한다.
pub fn save_cache(cache: &UpdateCache) {
    if let Ok(path) = cache_path() {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(text) = serde_json::to_string_pretty(cache) {
            let _ = fs::write(path, text);
        }
    }
}

/// 24시간 이내에 확인한 캐시가 있으면 그 etag 로 재사용 가능한지 판단한다.
pub fn is_cache_fresh(cache: &UpdateCache) -> bool {
    match cache.checked_at.parse::<i64>() {
        Ok(ts) => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            now - ts < CHECK_INTERVAL.as_secs() as i64
        }
        Err(_) => false,
    }
}

/// 릴리즈 조회 URL 을 결정한다.
/// - repo 가 `http://` 또는 `https://` 로 시작하면 그대로 사용한다.
/// - 그 외에는 `https://api.github.com/repos/{repo}/releases/latest` 를 반환한다.
pub fn release_url(repo: &str) -> String {
    if repo.starts_with("http://") || repo.starts_with("https://") {
        repo.to_string()
    } else {
        format!("https://api.github.com/repos/{repo}/releases/latest")
    }
}

/// 릴리즈 확인을 수행한다. HTTP 요청은 동기 reqwest blocking 클라이언트를 사용한다.
/// 모드가 `off` 면 실패로 처리한다.
pub fn check(repo: &str, mode: &UpdateMode, force: bool) -> Result<CheckResult> {
    if *mode == UpdateMode::Off {
        return Ok(CheckResult::Failed);
    }

    // 캐시가 최신이면 etag 조건부 요청으로 304 → 그대로 유지.
    let cache = load_cache();
    if !force && cache.as_ref().map(is_cache_fresh).unwrap_or(false) {
        // 최신 버전이 이미 확인된 상태이므로 결과를 캐시에서 재구성.
        if let Some(c) = &cache {
            if semver_util::is_newer(&c.latest_tag) {
                return Ok(CheckResult::UpdateAvailable {
                    latest: c.latest_tag.clone(),
                });
            }
            return Ok(CheckResult::UpToDate);
        }
    }

    let client = reqwest::blocking::Client::builder()
        .user_agent("bulti-update")
        .build()
        .context("HTTP 클라이언트 생성 실패")?;

    let url = release_url(repo);
    let mut req = client.get(&url);
    if let Some(c) = &cache {
        req = req.header("If-None-Match", &c.etag);
    }

    let resp = req
        .send()
        .with_context(|| format!("GitHub Releases 조회 실패: {url}"))?;

    // 304 → 변경 없음, 캐시 유지.
    if resp.status() == reqwest::StatusCode::NOT_MODIFIED {
        if let Some(c) = &cache {
            if semver_util::is_newer(&c.latest_tag) {
                return Ok(CheckResult::UpdateAvailable {
                    latest: c.latest_tag.clone(),
                });
            }
        }
        return Ok(CheckResult::UpToDate);
    }

    if !resp.status().is_success() {
        // 404 (릴리즈 없음) 등은 확인 불가로 처리 — 백그라운드 알림에서 조용히 무시.
        if resp.status().as_u16() == 404 {
            return Ok(CheckResult::Failed);
        }
        bail!("GitHub API 응답 오류: {}", resp.status());
    }

    let etag = resp
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();

    let release: Release = resp
        .json()
        .with_context(|| format!("릴리즈 JSON 파싱 실패: {url}"))?;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let cache = UpdateCache {
        etag,
        checked_at: now.to_string(),
        latest_tag: release.tag_name.clone(),
    };
    save_cache(&cache);

    if semver_util::is_newer(&release.tag_name) {
        Ok(CheckResult::UpdateAvailable {
            latest: release.tag_name.clone(),
        })
    } else {
        Ok(CheckResult::UpToDate)
    }
}

/// `bulti update --check` — 확인만 하고 아무것도 바꾸지 않는다.
pub fn check_only(repo: &str, mode: &UpdateMode) -> Result<i32> {
    match check(repo, mode, true)? {
        CheckResult::UpdateAvailable { latest } => {
            println!("새 버전 {latest} 사용 가능 — bulti update");
            Ok(0)
        }
        CheckResult::UpToDate => {
            println!("최신 버전입니다. (v{})", env!("CARGO_PKG_VERSION"));
            Ok(0)
        }
        CheckResult::Failed => {
            println!("업데이트 확인 불가 (mode=off 또는 네트워크 오류).");
            Ok(1)
        }
    }
}

/// `bulti update` — 모드에 따라 다운로드·교체까지 수행한다.
pub fn run_update(repo: &str, mode: &UpdateMode) -> Result<i32> {
    if *mode == UpdateMode::Off {
        println!("업데이트가 off 상태입니다.");
        return Ok(1);
    }

    match check(repo, mode, true)? {
        CheckResult::UpdateAvailable { latest } => {
            if *mode == UpdateMode::Check {
                println!("새 버전 {latest} 사용 가능 — bulti update");
                return Ok(0);
            }
            // download 모드: 다운로드·검증·교체.
            println!("새 버전 {latest} 다운로드 중...");
            match download_and_install(repo, &latest) {
                Ok(path) => {
                    self_replace::replace(&path)?;
                    Ok(0)
                }
                Err(e) => {
                    tracing::error!("업데이트 실패: {e:#}");
                    println!("업데이트 실패: {e}");
                    Ok(1)
                }
            }
        }
        CheckResult::UpToDate => {
            println!("최신 버전입니다. (v{})", env!("CARGO_PKG_VERSION"));
            Ok(0)
        }
        CheckResult::Failed => {
            println!("업데이트 확인 불가 (mode=off 또는 네트워크 오류).");
            Ok(1)
        }
    }
}

/// 릴리즈 태그를 현재 버전과 비교해 새 버전인지 판단한다.
#[allow(dead_code)]
pub fn is_newer(tag: &str) -> bool {
    semver_util::is_newer(tag)
}

/// asset 매칭: 빌드 타깃 트리플을 포함하는 asset 이름을 찾는다.
fn find_asset<'a>(release: &'a Release, triple: &str) -> Option<&'a Asset> {
    release
        .assets
        .iter()
        .find(|a| a.name.contains(triple))
}

/// 다운로드 대상 파일이 존재하는지 확인하고, 존재하면 다운로드한다.
fn download(url: &str, dest: &Path) -> Result<()> {
    let client = reqwest::blocking::Client::builder()
        .user_agent("bulti-update")
        .build()
        .context("HTTP 클라이언트 생성 실패")?;

    let mut resp = client
        .get(url)
        .send()
        .with_context(|| format!("다운로드 실패: {url}"))?;
    if !resp.status().is_success() {
        bail!("다운로드 응답 오류: {}", resp.status());
    }
    let mut f = fs::File::create(dest).with_context(|| format!("파일 생성 실패: {}", dest.display()))?;
    std::io::copy(&mut resp, &mut f).with_context(|| format!("파일 저장 실패: {}", dest.display()))?;
    Ok(())
}

/// sha256 검증: 파일의 해시가 기대값과 일치하는지 확인한다.
fn verify_sha256(path: &Path, expected: &str) -> Result<()> {
    use sha2::{Digest, Sha256};

    let mut file = fs::File::open(path).with_context(|| format!("해시 검증 파일 열기 실패: {}", path.display()))?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher).with_context(|| "해시 계산 실패")?;
    let actual = hasher.finalize();
    let actual_hex = format!("{actual:x}");
    let expected_trim = expected.trim().to_lowercase();
    if actual_hex != expected_trim {
        bail!(
            "sha256 불일치: 기대 {expected_trim}, 실제 {actual_hex} (파일: {})",
            path.display()
        );
    }
    Ok(())
}

/// 다운로드·검증·임시 해제·실행 비트 설정까지 수행하고, 교체 대상 바이너리 경로를 반환한다.
fn download_and_install(repo: &str, tag: &str) -> Result<PathBuf> {
    // 빌드 타깃 트리플. cfg 기반으로 결정.
    let triple = target_triple();
    let asset_name = format!("bulti-{triple}.tar.gz");

    // 릴리즈 정보 조회.
    let client = reqwest::blocking::Client::builder()
        .user_agent("bulti-update")
        .build()
        .context("HTTP 클라이언트 생성 실패")?;
    let url = release_url(repo);
    let resp = client
        .get(&url)
        .send()
        .with_context(|| format!("릴리즈 조회 실패: {url}"))?;
    if !resp.status().is_success() {
        bail!("릴리즈 조회 응답 오류: {}", resp.status());
    }
    let release: Release = resp
        .json()
        .with_context(|| "릴리즈 JSON 파싱 실패".to_string())?;

    // asset 매칭.
    let asset = find_asset(&release, &triple)
        .filter(|a| a.name.ends_with(".tar.gz"))
        .ok_or_else(|| anyhow!("asset {asset_name} 을 찾을 수 없습니다."))?;

    // 임시 디렉터리.
    let tmp_dir = tempfile::tempdir().context("임시 디렉터리 생성 실패")?;
    let archive_path = tmp_dir.path().join(&asset.name);
    let extract_dir = tmp_dir.path().join("extract");
    fs::create_dir_all(&extract_dir).context("해제 디렉터리 생성 실패")?;

    // 다운로드.
    download(&asset.browser_download_url, &archive_path)?;

    // checksums.txt 검증.
    let checksum_asset = release
        .assets
        .iter()
        .find(|a| a.name == "checksums.txt");
    if let Some(cs) = checksum_asset {
        let cs_path = tmp_dir.path().join("checksums.txt");
        download(&cs.browser_download_url, &cs_path)?;
        let text = fs::read_to_string(&cs_path).context("checksums.txt 읽기 실패")?;
        let expected = text
            .lines()
            .find(|l| l.contains(&asset.name))
            .and_then(|l| l.split_whitespace().next())
            .ok_or_else(|| anyhow!("checksums.txt 에 {} 항목이 없습니다.", asset.name))?;
        verify_sha256(&archive_path, expected)?;
    }

    // 해제 + 실행 비트.
    extract_tar_gz(&archive_path, &extract_dir)?;

    // 바이너리 경로: 해제된 디렉터리에서 bulti 실행 파일.
    let bin_path = find_binary(&extract_dir).ok_or_else(|| anyhow!("해제된 아카이브에서 bulti 바이너리를 찾지 못했습니다."))?;
    set_executable(&bin_path)?;

    Ok(bin_path.to_path_buf())
}

/// tar.gz 아카이브를 해제한다.
fn extract_tar_gz(archive: &Path, dest: &Path) -> Result<()> {
    let file = fs::File::open(archive).with_context(|| format!("아카이브 열기 실패: {}", archive.display()))?;
    let gz = flate2::read::GzDecoder::new(file);
    let mut tar = tar::Archive::new(gz);
    tar.unpack(dest)
        .with_context(|| format!("아카이브 해제 실패: {}", archive.display()))?;
    Ok(())
}

/// 해제된 디렉터리에서 bulti 실행 파일을 찾는다.
fn find_binary(dir: &Path) -> Option<PathBuf> {
    let entries = fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name == "bulti" || name.starts_with("bulti") {
                return Some(path);
            }
        }
    }
    None
}

/// 실행 비트를 설정한다 (unix). Windows 는 no-op.
fn set_executable(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perm = fs::metadata(path).context("메타데이터 읽기 실패")?.permissions();
        perm.set_mode(0o755);
        fs::set_permissions(path, perm).context("실행 비트 설정 실패")?;
    }
    Ok(())
}

/// 빌드 타깃 트리플 (예: `x86_64-unknown-linux-gnu`).
fn target_triple() -> String {
    let arch = std::env::consts::ARCH;
    let os = std::env::consts::OS;
    let env = "gnu";
    format!("{arch}-{os}-{env}")
}

/// run 시작 시 백그라운드 확인 → stderr 한 줄 알림.
pub fn notify_background(repo: &str, mode: &UpdateMode) {
    if *mode == UpdateMode::Off {
        return;
    }
    let repo = repo.to_string();
    let mode = mode.clone();
    std::thread::spawn(move || {
        match check(&repo, &mode, false) {
            Ok(CheckResult::UpdateAvailable { latest }) => {
                eprintln!("새 버전 {latest} 사용 가능 — bulti update");
            }
            _ => {}
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_path_is_under_home() {
        let p = cache_path().unwrap();
        assert!(p.to_string_lossy().contains(".bulti"));
        assert!(p.to_string_lossy().contains("update.json"));
    }

    #[test]
    fn cache_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        // 임시 경로 직접 사용.
        let cache = UpdateCache {
            etag: "\"abc\"".to_string(),
            checked_at: "123".to_string(),
            latest_tag: "v0.2.0".to_string(),
        };
        let text = serde_json::to_string(&cache).unwrap();
        let parsed: UpdateCache = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed.etag, "\"abc\"");
        assert_eq!(parsed.latest_tag, "v0.2.0");
    }

    #[test]
    fn is_cache_fresh_works() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let fresh = UpdateCache {
            etag: "e".into(),
            checked_at: now.to_string(),
            latest_tag: "v0.2.0".into(),
        };
        assert!(is_cache_fresh(&fresh));

        let stale = UpdateCache {
            etag: "e".into(),
            checked_at: (now - 100_000).to_string(),
            latest_tag: "v0.2.0".into(),
        };
        assert!(!is_cache_fresh(&stale));
    }

    #[test]
    fn find_asset_matches_triple() {
        let rel = Release {
            tag_name: "v0.2.0".into(),
            assets: vec![
                Asset {
                    name: "bulti-x86_64-unknown-linux-gnu.tar.gz".into(),
                    browser_download_url: "https://example.com/a".into(),
                },
                Asset {
                    name: "checksums.txt".into(),
                    browser_download_url: "https://example.com/c".into(),
                },
            ],
        };
        let a = find_asset(&rel, "x86_64-unknown-linux-gnu").unwrap();
        assert!(a.name.ends_with(".tar.gz"));
    }

    #[test]
    fn verify_sha256_matches() {
        // 빈 파일의 sha256.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("empty");
        fs::write(&p, b"").unwrap();
        let expected = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        verify_sha256(&p, expected).unwrap();

        // 불일치.
        let bad = "0000000000000000000000000000000000000000000000000000000000000000";
        assert!(verify_sha256(&p, bad).is_err());
    }

    #[test]
    fn target_triple_is_nonempty() {
        assert!(!target_triple().is_empty());
    }

    #[test]
    fn release_url_uses_full_url_when_http() {
        assert_eq!(release_url("http://localhost:1234/releases/latest"), "http://localhost:1234/releases/latest");
        assert_eq!(release_url("https://example.com/releases/latest"), "https://example.com/releases/latest");
    }

    #[test]
    fn release_url_uses_github_api_for_repo() {
        assert_eq!(
            release_url("agurrrrr/bulti"),
            "https://api.github.com/repos/agurrrrr/bulti/releases/latest"
        );
    }

    /// 실제 홈 디렉터리 캐시 파일을 삭제해 테스트 격리를 보장한다.
    fn clear_real_cache() {
        if let Ok(p) = cache_path() {
            let _ = fs::remove_file(p);
        }
    }

    /// wiremock 기반 통합 테스트.
    /// `check` 가 `release_url` 로 wiremock 서버를 가리키게 하여 새 버전/같은 버전/404 응답을 검증한다.
    #[test]
    fn check_against_wiremock_new_version() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let rt = tokio::runtime::Runtime::new().unwrap();
        clear_real_cache();
        let _guard = rt.enter();

        let server = rt.block_on(MockServer::start());
        let body = serde_json::json!({
            "tag_name": "v9.9.9",
            "assets": []
        });
        rt.block_on(async {
            Mock::given(method("GET"))
                .and(path("/releases/latest"))
                .respond_with(ResponseTemplate::new(200).set_body_json(&body))
                .up_to_n_times(1)
                .expect(1)
                .mount(&server)
                .await;
        });

        let url = format!("{}/releases/latest", server.uri());
        let result = check(&url, &UpdateMode::Check, true).unwrap();
        match result {
            CheckResult::UpdateAvailable { latest } => assert_eq!(latest, "v9.9.9"),
            other => panic!("예상: UpdateAvailable, 실제: {other:?}"),
        }
    }

    #[test]
    fn check_against_wiremock_up_to_date() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let rt = tokio::runtime::Runtime::new().unwrap();
        clear_real_cache();
        let _guard = rt.enter();

        let server = rt.block_on(MockServer::start());
        let body = serde_json::json!({
            "tag_name": env!("CARGO_PKG_VERSION"),
            "assets": []
        });
        rt.block_on(async {
            Mock::given(method("GET"))
                .and(path("/releases/latest"))
                .respond_with(ResponseTemplate::new(200).set_body_json(&body))
                .up_to_n_times(1)
                .expect(1)
                .mount(&server)
                .await;
        });

        let url = format!("{}/releases/latest", server.uri());
        let result = check(&url, &UpdateMode::Check, true).unwrap();
        assert!(matches!(result, CheckResult::UpToDate));
    }

    #[test]
    fn check_against_wiremock_404_is_failed() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let rt = tokio::runtime::Runtime::new().unwrap();
        clear_real_cache();
        let _guard = rt.enter();

        let server = rt.block_on(MockServer::start());
        rt.block_on(async {
            Mock::given(method("GET"))
                .and(path("/releases/latest"))
                .respond_with(ResponseTemplate::new(404))
                .up_to_n_times(1)
                .expect(1)
                .mount(&server)
                .await;
        });

        let url = format!("{}/releases/latest", server.uri());
        let result = check(&url, &UpdateMode::Check, true).unwrap();
        assert!(matches!(result, CheckResult::Failed));
    }

    #[test]
    fn check_against_wiremock_304_keeps_cache() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let rt = tokio::runtime::Runtime::new().unwrap();
        clear_real_cache();
        let _guard = rt.enter();

        let server = rt.block_on(MockServer::start());
        rt.block_on(async {
            Mock::given(method("GET"))
                .and(path("/releases/latest"))
                .respond_with(ResponseTemplate::new(304))
                .up_to_n_times(1)
                .expect(1)
                .mount(&server)
                .await;
        });

        let url = format!("{}/releases/latest", server.uri());
        let result = check(&url, &UpdateMode::Check, true).unwrap();
        // 캐시가 없으므로 304 는 UpToDate 로 처리된다.
        assert!(matches!(result, CheckResult::UpToDate));
    }

    /// 모드별 check_only / run_update 분기 검증 (wiremock).
    #[test]
    fn check_only_prints_new_version() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let rt = tokio::runtime::Runtime::new().unwrap();
        clear_real_cache();
        let _guard = rt.enter();

        let server = rt.block_on(MockServer::start());
        let body = serde_json::json!({
            "tag_name": "v9.9.9",
            "assets": []
        });
        rt.block_on(async {
            Mock::given(method("GET"))
                .and(path("/releases/latest"))
                .respond_with(ResponseTemplate::new(200).set_body_json(&body))
                .up_to_n_times(1)
                .expect(1)
                .mount(&server)
                .await;
        });

        let url = format!("{}/releases/latest", server.uri());
        let code = check_only(&url, &UpdateMode::Check).unwrap();
        assert_eq!(code, 0);
    }

    #[test]
    fn run_update_check_mode_does_not_download() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let rt = tokio::runtime::Runtime::new().unwrap();
        clear_real_cache();
        let _guard = rt.enter();

        let server = rt.block_on(MockServer::start());
        let body = serde_json::json!({
            "tag_name": "v9.9.9",
            "assets": []
        });
        rt.block_on(async {
            Mock::given(method("GET"))
                .and(path("/releases/latest"))
                .respond_with(ResponseTemplate::new(200).set_body_json(&body))
                .up_to_n_times(1)
                .expect(1)
                .mount(&server)
                .await;
        });

        let url = format!("{}/releases/latest", server.uri());
        let code = run_update(&url, &UpdateMode::Check).unwrap();
        assert_eq!(code, 0);
    }
}
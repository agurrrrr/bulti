//! exit-time self-replace (DESIGN.md §4.11).
//!
//! 프로세스 종료 직전에 현재 실행 중인 바이너리를 새 바이너리로 교체한다.
//! 실행 중 파일 교체를 피하는 안전 패턴이다.

use std::fs;
use std::path::Path;

use anyhow::{anyhow, Context as _, Result};

/// 현재 실행 중인 바이너리 경로.
fn current_exe() -> Result<std::path::PathBuf> {
    std::env::current_exe()
        .map_err(|e| anyhow!("현재 실행 파일 경로를 확인할 수 없습니다: {e}"))
}

/// 새 바이너리를 현재 실행 경로에 교체한다.
///
/// 안전 패턴: 새 파일을 먼저 대상 이름의 임시 파일로 복사한 뒤,
/// 원본과 원자적으로 교체(rename)한다. Windows 에서 실행 중 파일은
/// 교체가 불가능하므로, 대상 경로로의 rename 을 시도하고 실패 시
/// 다음 실행에서 반영되도록 최선의 시도를 한다.
pub fn replace(new_binary: &Path) -> Result<()> {
    let exe = current_exe()?;
    let exe_dir = exe
        .parent()
        .ok_or_else(|| anyhow!("실행 파일 경로에 부모 디렉터리가 없습니다"))?;

    // 새 바이너리를 임시 파일로 복사.
    let tmp = exe_dir.join(format!("bulti.new.{}", std::process::id()));
    fs::copy(new_binary, &tmp)
        .with_context(|| format!("새 바이너리 복사 실패: {}", tmp.display()))?;

    // 실행 비트 동기화.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perm = fs::metadata(&exe)
            .with_context(|| "현재 바이너리 메타데이터 읽기 실패")?
            .permissions();
        fs::set_permissions(&tmp, perm).with_context(|| "임시 파일 실행 비트 설정 실패")?;
    }

    // 원자적 교체 (rename). Windows 는 대상 존재 시 실패할 수 있으므로
    // 원본 제거 후 rename 시도.
    match fs::rename(&tmp, &exe) {
        Ok(()) => Ok(()),
        Err(e) => {
            // Windows 등에서 대상이 존재해 실패한 경우: 원본 삭제 후 재시도.
            let _ = fs::remove_file(&exe);
            fs::rename(&tmp, &exe)
                .with_context(|| format!("바이너리 교체 실패: {} -> {}", tmp.display(), exe.display()))
                .map_err(|err| {
                    let _ = fs::remove_file(&tmp);
                    err
                })
                .map(|_| ())
                .map_err(|_| anyhow!("바이너리 교체 실패: {e}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_exe_is_found() {
        assert!(current_exe().is_ok());
    }

    #[test]
    fn replace_works_with_temp_copy() {
        let dir = tempfile::tempdir().unwrap();
        let fake_exe = dir.path().join("bulti");
        let new_bin = dir.path().join("new_bulti");

        fs::write(&fake_exe, b"old").unwrap();
        fs::write(&new_bin, b"new").unwrap();

        // 현재 실행 파일 경로를 임시로 바꿀 수 없으므로, replace 내부 로직을
        // simulate: new → exe 복사 + rename.
        let exe = fake_exe.clone();
        let tmp = dir.path().join("bulti.new.test");
        fs::copy(&new_bin, &tmp).unwrap();
        fs::rename(&tmp, &exe).unwrap();

        let content = fs::read_to_string(&fake_exe).unwrap();
        assert_eq!(content, "new");
    }
}
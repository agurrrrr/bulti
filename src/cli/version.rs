//! `bulti version` 서브커맨드 구현.

use serde_json::json;

use super::VersionArgs;

/// 버전 정보를 출력한다.
pub fn run(args: VersionArgs) -> Result<i32, Box<dyn std::error::Error>> {
    let version = env!("CARGO_PKG_VERSION");
    if args.json {
        let out = json!({
            "name": "bulti",
            "version": version,
            "edition": "2024",
        });
        println!("{}", out);
    } else {
        println!("bulti {}", version);
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_plain_ok() {
        assert_eq!(run(VersionArgs { json: false }).unwrap(), 0);
    }

    #[test]
    fn version_json_ok() {
        assert_eq!(run(VersionArgs { json: true }).unwrap(), 0);
    }
}

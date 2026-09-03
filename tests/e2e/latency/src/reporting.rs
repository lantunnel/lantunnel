use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Serialize)]
struct BuildMetadata {
    profile: String,
    git_sha: String,
}

impl BuildMetadata {
    fn current() -> Self {
        Self {
            profile: build_profile(),
            git_sha: git_sha(),
        }
    }
}

pub fn write_json_report<T: Serialize>(path: &str, report: &T) -> Result<()> {
    let mut value = serde_json::to_value(report).context("serialize report value")?;
    let build =
        serde_json::to_value(BuildMetadata::current()).context("serialize build metadata")?;
    match &mut value {
        Value::Object(map) => {
            map.insert("build".into(), build);
        }
        _ => {
            let mut map = serde_json::Map::new();
            map.insert("build".into(), build);
            map.insert("report".into(), value);
            value = Value::Object(map);
        }
    }
    let json = serde_json::to_string_pretty(&value).context("serialize report")?;
    std::fs::write(path, json).with_context(|| format!("write report {path}"))
}

fn build_profile() -> String {
    std::env::var("BUILD_PROFILE")
        .or_else(|_| std::env::var("CARGO_PROFILE"))
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(profile_from_current_exe)
        .unwrap_or_else(|| "unknown".into())
}

fn profile_from_current_exe() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    for part in exe.components() {
        let s = part.as_os_str().to_str()?;
        if matches!(s, "release-perf" | "release" | "debug") {
            return Some(s.to_string());
        }
    }
    None
}

fn git_sha() -> String {
    if let Ok(sha) = std::env::var("GITHUB_SHA") {
        let short: String = sha.chars().take(12).collect();
        if !short.is_empty() {
            return short;
        }
    }
    std::process::Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .ok()
        .and_then(|out| {
            out.status
                .success()
                .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

//! Embed the git commit into the binary (audit G11): `cvd --version` prints `x.y.z (abc123)`,
//! so a running binary is checkable against source. `CV_BUILD_SHA` overrides (release pipelines);
//! outside a git checkout (crates.io builds) it degrades to "unknown".

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!s.is_empty()).then_some(s)
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=CV_BUILD_SHA");
    if let Some(dir) = git(&["rev-parse", "--absolute-git-dir"]) {
        println!("cargo:rerun-if-changed={dir}/HEAD");
    }
    let sha = std::env::var("CV_BUILD_SHA")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            git(&["rev-parse", "--short=12", "HEAD"]).map(|short| {
                match git(&["status", "--porcelain"]) {
                    Some(st) if !st.is_empty() => format!("{short}-dirty"),
                    _ => short,
                }
            })
        })
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=CV_BUILD_SHA={sha}");
}

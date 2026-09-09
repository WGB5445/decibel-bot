/// Git commit hash set at build time via `build.rs` or environment.
/// Falls back to `env!("VERGEN_GIT_SHA")` or a short `git rev-parse` snippet.
pub fn short_commit() -> String {
    option_env!("DECIBEL_COMMIT")
        .or_else(|| option_env!("VERGEN_GIT_SHA"))
        .or_else(|| {
            // Last resort: read from .git/HEAD at compile time via a build-script constant.
            option_env!("BUILD_GIT_COMMIT")
        })
        .map(|s| {
            if s.len() > 8 {
                s[..8].to_owned()
            } else {
                s.to_owned()
            }
        })
        .unwrap_or_else(|| "unknown".to_owned())
}

pub fn version_string() -> String {
    format!("{}-{}", env!("CARGO_PKG_VERSION"), short_commit())
}

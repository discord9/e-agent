// The web UI is a single self-contained HTML file at src/ui/index.html.
// When present it is read from disk on every request (cfg `web_ui`), so a
// dev server picks up frontend edits on refresh without a rebuild; until
// the frontend task lands it, `e-agent --serve` serves a minimal
// placeholder page instead. rerun-if-changed makes cargo pick the file up
// the moment it appears (while it is missing cargo reruns the build script
// each build, which is what makes that detection work).
fn main() {
    // Inject the git commit (short hash + dirty marker) so --version can
    // identify the exact build. Falls back to "unknown" when not in a git
    // worktree (e.g. vendored source tarball).
    let commit = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    let dirty = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);
    let version = match commit {
        Some(c) if !c.is_empty() => format!("{c}{}", if dirty { "-dirty" } else { "" }),
        _ => "unknown".to_string(),
    };
    println!("cargo:rustc-env=E_AGENT_COMMIT={version}");
    println!("cargo:rustc-check-cfg=cfg(web_ui)");
    if std::path::Path::new("src/ui/index.html").exists() {
        println!("cargo:rustc-cfg=web_ui");
    }
    println!("cargo:rerun-if-changed=src/ui/index.html");
    // Re-run on every commit so the injected hash tracks HEAD.
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs/");
}

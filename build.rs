// src/ui/index.html is the web UI sentinel: its presence flips the `web_ui`
// cfg and turns on UI serving (a minimal placeholder page otherwise).
// Debug builds read the ten UI assets from disk on every request, so a dev
// server picks up frontend edits on refresh without a rebuild. Release
// builds compile those same assets into the binary with include_str!
// (read_embedded_ui in src/server.rs) so the UI survives a deleted source
// tree. rustc's own include_str! dependency tracking recompiles when any of
// the ten assets changes, so no per-asset rerun-if-changed is needed —
// index.html alone gets one here, and that also makes cargo pick the
// sentinel up the moment it appears (while it is missing, cargo reruns the
// build script each build, which is what makes that detection work).
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

// The web UI is a single self-contained HTML file at src/ui/index.html.
// When present it is read from disk on every request (cfg `web_ui`), so a
// dev server picks up frontend edits on refresh without a rebuild; until
// the frontend task lands it, `e-agent --serve` serves a minimal
// placeholder page instead. rerun-if-changed makes cargo pick the file up
// the moment it appears (while it is missing cargo reruns the build script
// each build, which is what makes that detection work).
fn main() {
    println!("cargo:rustc-check-cfg=cfg(web_ui)");
    if std::path::Path::new("src/ui/index.html").exists() {
        println!("cargo:rustc-cfg=web_ui");
    }
    println!("cargo:rerun-if-changed=src/ui/index.html");
}

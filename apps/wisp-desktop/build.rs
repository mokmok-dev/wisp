use std::fs;

// `?` instead of `expect`: the package lints deny `clippy::expect_used` for
// every target (build scripts included) and CI runs clippy with
// `--deny warnings`. Returning `Err` still fails the build with a clear
// cargo-reported message.
fn main() -> std::io::Result<()> {
    // `web_shell.rs` embeds `ui/dist` via `include_dir!`; cargo has no
    // built-in dependency tracking for that, so declare it here — otherwise
    // a rebuilt UI bundle would not make it into the binary. The directory
    // itself is created when missing (fresh checkout without a UI build:
    // the app then serves a placeholder page).
    fs::create_dir_all("ui/dist")?;
    println!("cargo:rerun-if-changed=ui/src");
    println!("cargo:rerun-if-changed=ui/dist");
    println!("cargo:rerun-if-changed=ui/index.html");
    println!("cargo:rerun-if-changed=ui/package.json");
    Ok(())
}

use std::fs;

fn main() {
    // `web_server.rs` embeds `ui/dist` via `include_dir!`; cargo has no
    // built-in dependency tracking for that, so declare it here — otherwise
    // a rebuilt UI bundle would not make it into the binary. The directory
    // itself is created when missing (fresh checkout without a UI build:
    // the app then serves a placeholder page).
    fs::create_dir_all("ui/dist").expect("create ui/dist");
    println!("cargo:rerun-if-changed=ui/src");
    println!("cargo:rerun-if-changed=ui/dist");
    println!("cargo:rerun-if-changed=ui/index.html");
    println!("cargo:rerun-if-changed=ui/package.json");
}

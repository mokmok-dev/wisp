fn main() {
    // `web_server.rs` embeds `ui/dist` via `include_dir!`; cargo has no
    // built-in dependency tracking for that, so declare it here — otherwise
    // a rebuilt UI bundle would not make it into the binary.
    println!("cargo:rerun-if-changed=ui/src");
    println!("cargo:rerun-if-changed=ui/dist");
    println!("cargo:rerun-if-changed=ui/index.html");
    println!("cargo:rerun-if-changed=ui/package.json");
}

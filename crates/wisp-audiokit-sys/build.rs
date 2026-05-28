//! Build `WispAudioKit` via `SwiftPM` and emit the link flags needed for the
//! Rust binary to consume the resulting static library.
//!
//! On non-macOS targets the build is a no-op (the safe wrapper crate stubs
//! out the API at compile time), so the workspace stays buildable on Linux
//! CI even though the Swift toolchain is not available there.

// Build scripts are intentionally panic-on-error: cargo presents the panic
// message as a build failure, which is exactly the UX we want. The long
// link-args block in `main` is fine to read top-to-bottom; splitting it
// just to satisfy the pedantic line limit hurts more than it helps.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::too_many_lines
)]

use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }

    let manifest_dir = PathBuf::from(
        env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is always set by cargo"),
    );
    // crates/wisp-audiokit-sys -> workspace root
    let workspace_root = manifest_dir
        .parent()
        .and_then(|p| p.parent())
        .expect("crate is two levels deep in the workspace");
    let swift_pkg = workspace_root.join("native").join("WispAudioKit");

    // Invalidate the build whenever the Swift sources, package manifest, or
    // C header change.
    println!(
        "cargo:rerun-if-changed={}",
        swift_pkg.join("Package.swift").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        swift_pkg.join("Sources").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        swift_pkg.join("include").display()
    );

    let profile = env::var("PROFILE").unwrap_or_else(|_| "debug".into());
    let is_release = profile == "release";

    // When invoked from inside `nix develop`, Nix injects its own apple-sdk
    // paths into DEVELOPER_DIR / SDKROOT. The /usr/bin/swift driver then
    // can't find swift-frontend (it's not in the Nix SDK) and the SDK
    // version doesn't match the toolchain. Detect Nix-store paths and fall
    // back to a real Apple toolchain: prefer Xcode (GitHub macOS runners
    // and most local dev machines), then the Command Line Tools. Real
    // DEVELOPER_DIR overrides (anything not under /nix/store) are honored
    // as-is.
    let developer_dir = match env::var("DEVELOPER_DIR") {
        Ok(v) if !v.starts_with("/nix/store/") => v.into(),
        _ => detect_developer_dir(),
    };
    let sdk_root = match env::var("SDKROOT") {
        Ok(v) if !v.starts_with("/nix/store/") => v.into(),
        _ => detect_sdk_root(&developer_dir),
    };

    let configure = |cmd: &mut Command| {
        cmd.current_dir(&swift_pkg)
            .env("DEVELOPER_DIR", &developer_dir)
            .env("SDKROOT", &sdk_root);
    };

    // 1) Build the Swift package.
    let mut build_cmd = Command::new("swift");
    build_cmd.arg("build");
    configure(&mut build_cmd);
    if is_release {
        build_cmd.args(["-c", "release"]);
    }
    let build_status = build_cmd
        .status()
        .expect("failed to execute `swift build` for WispAudioKit");
    assert!(build_status.success(), "swift build failed");

    // 2) Ask SwiftPM where it put the artifacts.
    let mut path_cmd = Command::new("swift");
    path_cmd.args(["build", "--show-bin-path"]);
    configure(&mut path_cmd);
    if is_release {
        path_cmd.args(["-c", "release"]);
    }
    let path_output = path_cmd
        .output()
        .expect("failed to execute `swift build --show-bin-path`");
    assert!(
        path_output.status.success(),
        "swift build --show-bin-path failed"
    );
    let bin_path = String::from_utf8(path_output.stdout)
        .expect("swift bin path is valid utf-8")
        .trim()
        .to_string();

    // 3) Tell rustc where libWispAudioKit.a lives and to link it statically.
    println!("cargo:rustc-link-search=native={bin_path}");
    println!("cargo:rustc-link-lib=static=WispAudioKit");

    // 4) Apple frameworks the Swift sources touch (transitively from the
    //    audio + transcription pipelines; required even when we currently
    //    only call `wisp_audiokit_version`, because the .a archive pulls in
    //    every .o whose external symbols are referenced and we will be
    //    extending the FFI surface very soon).
    for framework in [
        "Foundation",
        "AVFoundation",
        "AVFAudio",
        "CoreAudio",
        // CoreAudioTypes is a header-only framework (no binary); Swift's
        // auto-linker mentions it for module imports but it must not be
        // passed as `-framework`.
        "CoreMedia",
        "ScreenCaptureKit",
        "Speech",
    ] {
        println!("cargo:rustc-link-lib=framework={framework}");
    }

    // 5) Inside `nix develop` the clang wrapper points -F / -L at the Nix
    //    Apple SDK (14.4), which is older than the SDK that built our Swift
    //    .a and is missing newer frameworks and the swift runtime stubs.
    //    Add the system SDK's framework and swift library paths explicitly
    //    so the linker can resolve them. `framework=` / `native=` search
    //    forms are emitted as both -F/-L and respected by rustc.
    let sdk_root_path = PathBuf::from(&sdk_root);
    let sdk_frameworks = sdk_root_path.join("System/Library/Frameworks");
    let sdk_swift_lib = sdk_root_path.join("usr/lib/swift");
    println!(
        "cargo:rustc-link-search=framework={}",
        sdk_frameworks.display()
    );
    println!("cargo:rustc-link-search=framework=/System/Library/Frameworks");
    println!("cargo:rustc-link-search=native={}", sdk_swift_lib.display());
    println!("cargo:rustc-link-search=native=/usr/lib/swift");
    println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");

    // 6) Swift runtime libraries. The static .a doesn't carry the Swift
    //    stdlib / concurrency / Foundation overlays; we link them as
    //    dylibs from /usr/lib/swift (resolved via -L flags above).
    for lib in [
        "swiftCore",
        "swiftFoundation",
        "swiftCoreFoundation",
        "swiftDarwin",
        "swiftDispatch",
        "swiftIOKit",
        "swiftObjectiveC",
        "swiftXPC",
        "swift_Concurrency",
        "swift_StringProcessing",
    ] {
        println!("cargo:rustc-link-lib=dylib={lib}");
    }
}

/// Pick a developer directory when `DEVELOPER_DIR` isn't usable (unset or
/// pointing into the Nix store). Tries Xcode first because GitHub's macOS
/// runners ship Xcode but not the standalone Command Line Tools, then
/// falls back to CLT for local dev machines that only have CLT installed.
fn detect_developer_dir() -> std::ffi::OsString {
    const CANDIDATES: &[&str] = &[
        "/Applications/Xcode.app/Contents/Developer",
        "/Library/Developer/CommandLineTools",
    ];
    for c in CANDIDATES {
        if std::path::Path::new(c).exists() {
            return c.into();
        }
    }
    // Last-ditch: hand back the CLT path even if it doesn't exist; the
    // subsequent `swift build` will fail with a clearer error than we
    // could produce here.
    "/Library/Developer/CommandLineTools".into()
}

/// Locate the macOS SDK under a given developer directory. CLT places it
/// at `$DEV/SDKs/MacOSX.sdk`; Xcode places it at
/// `$DEV/Platforms/MacOSX.platform/Developer/SDKs/MacOSX.sdk`. Picks
/// whichever exists; falls back to the CLT layout otherwise.
fn detect_sdk_root(developer_dir: &std::ffi::OsStr) -> std::ffi::OsString {
    let dev = PathBuf::from(developer_dir);
    let candidates = [
        dev.join("SDKs/MacOSX.sdk"),
        dev.join("Platforms/MacOSX.platform/Developer/SDKs/MacOSX.sdk"),
    ];
    for c in &candidates {
        if c.exists() {
            return c.clone().into_os_string();
        }
    }
    candidates[0].clone().into_os_string()
}

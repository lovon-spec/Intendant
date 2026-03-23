//! Build script for the intendant binary.
//!
//! Checks whether the compiled WASM files in `static/wasm-web/` are older than
//! the Rust source in `crates/presence-web/src/`. If stale, auto-rebuilds via
//! `wasm-pack build` using a separate target directory to avoid deadlocking
//! with the parent cargo process.

use std::path::Path;
use std::process::Command;

fn main() {
    // Re-run if any presence-web source file changes.
    println!("cargo:rerun-if-changed=crates/presence-web/src/");
    println!("cargo:rerun-if-changed=crates/presence-core/src/");
    println!("cargo:rerun-if-changed=static/wasm-web/presence_web_bg.wasm");

    let wasm_bin = Path::new("static/wasm-web/presence_web_bg.wasm");
    let src_dir = Path::new("crates/presence-web/src");
    let core_dir = Path::new("crates/presence-core/src");

    if !wasm_bin.exists() || !src_dir.exists() {
        return;
    }

    let wasm_modified = wasm_bin
        .metadata()
        .and_then(|m| m.modified())
        .ok();

    let src_modified = [src_dir, core_dir]
        .iter()
        .filter_map(|d| newest_in_dir(d))
        .max();

    let stale = match (wasm_modified, src_modified) {
        (Some(w), Some(s)) => s > w,
        _ => false,
    };

    if !stale {
        return;
    }

    println!("cargo:warning=WASM is stale — auto-rebuilding via wasm-pack...");

    // Use a separate target directory to avoid deadlocking with the parent
    // cargo process. The parent holds a lock on `target/`, so wasm-pack's
    // internal `cargo build --target wasm32` must write elsewhere.
    let wasm_target = Path::new("target/wasm-build");

    let result = Command::new("wasm-pack")
        .args([
            "build", "--target", "web",
            "--out-dir", "../../static/wasm-web",
            "--out-name", "presence_web",
        ])
        .current_dir("crates/presence-web")
        .env("CARGO_TARGET_DIR", std::fs::canonicalize(wasm_target)
            .unwrap_or_else(|_| {
                // Create the directory if it doesn't exist
                let _ = std::fs::create_dir_all(wasm_target);
                wasm_target.to_path_buf()
            }))
        .status();

    match result {
        Ok(status) if status.success() => {
            println!("cargo:warning=WASM rebuilt successfully");
        }
        Ok(status) => {
            println!(
                "cargo:warning=wasm-pack failed (exit {}). Run manually: cd crates/presence-web && wasm-pack build --target web --out-dir ../../static/wasm-web --out-name presence_web",
                status
            );
        }
        Err(_) => {
            println!("cargo:warning=wasm-pack not found. Install: cargo install wasm-pack");
        }
    }
}

/// Find the newest modification time among all files in a directory (recursive).
fn newest_in_dir(dir: &Path) -> Option<std::time::SystemTime> {
    let mut newest = None;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if let Some(t) = newest_in_dir(&path) {
                    newest = Some(newest.map_or(t, |n: std::time::SystemTime| n.max(t)));
                }
            } else if let Ok(meta) = path.metadata() {
                if let Ok(modified) = meta.modified() {
                    newest = Some(newest.map_or(modified, |n: std::time::SystemTime| n.max(modified)));
                }
            }
        }
    }
    newest
}

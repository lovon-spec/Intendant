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
    println!("cargo:rerun-if-changed=static/wasm-web/presence_web.js");

    // Detect OpenSSL 3 so the `lan` subcommand can conditionally load the
    // legacy provider for RC2-40 (required for iOS-compatible PKCS#12).
    // openssl-sys sets DEP_OPENSSL_VERSION_NUMBER via its `links = "openssl"`
    // manifest entry; we forward it as a cfg for our own code.
    println!("cargo:rustc-check-cfg=cfg(ossl3)");
    if let Ok(version) = std::env::var("DEP_OPENSSL_VERSION_NUMBER") {
        if let Ok(n) = u64::from_str_radix(&version, 16) {
            if n >= 0x3000_0000 {
                println!("cargo:rustc-cfg=ossl3");
            }
        }
    }

    // Expose the current git commit SHA as an env var so `/config` can
    // report it. The multi-host dashboard compares the primary's SHA
    // against each secondary's SHA and warns on mismatch — same class of
    // version-skew confusion we just hit when the mac guest was running
    // stale code without CORS headers.
    //
    // rerun-if-changed on HEAD + the branch ref file covers the common
    // "committed but didn't recompile" path. If the git command fails
    // (no .git, binary missing, detached head in weird state) the value
    // falls back to "unknown".
    println!("cargo:rerun-if-changed=.git/HEAD");
    if let Ok(head) = std::fs::read_to_string(".git/HEAD") {
        if let Some(ref_path) = head.strip_prefix("ref: ").map(|s| s.trim()) {
            println!("cargo:rerun-if-changed=.git/{}", ref_path);
        }
    }
    let git_sha = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .and_then(|o| if o.status.success() { Some(o.stdout) } else { None })
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());

    // Append `-dirty` when the working tree has uncommitted changes, so
    // the multi-host skew detector catches "I rebuilt but didn't commit"
    // cases. Without this, a dev rebuilding locally on top of HEAD
    // would report the same SHA as a sibling daemon still on that
    // commit, and the yellow warning wouldn't fire.
    let dirty = Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);
    let sha_with_dirty = if dirty {
        format!("{git_sha}-dirty")
    } else {
        git_sha
    };
    println!("cargo:rustc-env=INTENDANT_GIT_SHA={sha_with_dirty}");

    // Write a hash of the WASM binary to OUT_DIR so cargo detects changes
    // reliably. `rerun-if-changed` on binary files can be flaky across
    // worktrees; writing a derived file to OUT_DIR is bulletproof because
    // cargo always checks OUT_DIR contents.
    write_wasm_hash();

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

/// Write a content hash of the WASM files to OUT_DIR. Cargo always tracks
/// OUT_DIR for changes, so when the WASM is rebuilt the hash file changes
/// and cargo recompiles the crate (re-running `include_bytes!`).
fn write_wasm_hash() {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use std::io::Read;

    let out_dir = match std::env::var("OUT_DIR") {
        Ok(d) => d,
        Err(_) => return,
    };

    let mut hasher = DefaultHasher::new();
    for path in &[
        "static/wasm-web/presence_web_bg.wasm",
        "static/wasm-web/presence_web.js",
    ] {
        if let Ok(mut f) = std::fs::File::open(path) {
            let mut buf = Vec::new();
            let _ = f.read_to_end(&mut buf);
            buf.hash(&mut hasher);
        }
    }
    let hash = format!("{:016x}", hasher.finish());

    let hash_path = Path::new(&out_dir).join("wasm_hash.txt");
    // Only write if changed, to avoid unnecessary rebuilds
    let existing = std::fs::read_to_string(&hash_path).unwrap_or_default();
    if existing.trim() != hash {
        let _ = std::fs::write(&hash_path, &hash);
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

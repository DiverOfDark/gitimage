//! Embeds the built web UI (`ui/dist`) into the binary. Without a build of the UI (plain
//! `cargo test`), a placeholder page is embedded instead, so the workspace always compiles.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

fn collect(dir: &Path, base: &Path, out: &mut Vec<(String, PathBuf)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, base, out);
        } else if let Ok(rel) = path.strip_prefix(base) {
            let rel = rel.to_string_lossy().replace('\\', "/");
            if rel != ".gitkeep" {
                out.push((rel, path));
            }
        }
    }
}

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let dist = manifest.join("../../ui/dist");
    println!("cargo:rerun-if-changed={}", dist.display());

    let mut files = Vec::new();
    if dist.join("index.html").exists() {
        let dist = dist.canonicalize().unwrap();
        collect(&dist, &dist, &mut files);
    } else {
        let placeholder = out_dir.join("index.html");
        std::fs::write(
            &placeholder,
            "<!doctype html><title>GitImage</title><p>The UI was not built into this binary. \
             Run <code>npm --prefix ui ci &amp;&amp; npm --prefix ui run build</code>, then rebuild.</p>",
        )
        .unwrap();
        files.push(("index.html".into(), placeholder));
    }
    files.sort();

    let mut src = String::from("pub static ASSETS: &[(&str, &[u8])] = &[\n");
    for (rel, path) in &files {
        writeln!(
            src,
            "    ({rel:?}, include_bytes!({:?})),",
            path.display().to_string()
        )
        .unwrap();
    }
    src.push_str("];\n");
    std::fs::write(out_dir.join("ui_assets.rs"), src).unwrap();
}

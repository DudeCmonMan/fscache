use std::path::PathBuf;
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    Command::new("git")
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

fn main() {
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/index");

    let hash = git(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "unknown".to_string());
    let dirty = Command::new("git")
        .args(["diff", "--quiet", "HEAD"])
        .status()
        .map(|s| !s.success())
        .unwrap_or(false);
    let timestamp = Command::new("date")
        .arg("+%Y-%m-%d %H:%M")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let version = if dirty {
        format!("{}-dirty (built {})", hash, timestamp)
    } else {
        format!("{} (built {})", hash, timestamp)
    };
    println!("cargo:rustc-env=BUILD_VERSION={}", version);

    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let target_dir = out_dir
        .ancestors()
        .nth(3)
        .expect("unexpected OUT_DIR structure")
        .to_path_buf();

    for filename in &["config.toml"] {
        let src = manifest_dir.join(filename);
        let dst = target_dir.join(filename);
        if src.exists() {
            std::fs::copy(&src, &dst)
                .unwrap_or_else(|e| panic!("failed to copy {} to {}: {}", filename, dst.display(), e));
            println!("cargo:warning=Copied {} → {}", filename, dst.display());
        }
        println!("cargo:rerun-if-changed={}", filename);
    }
}

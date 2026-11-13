//! Linking fallback: if the xkbcommon development symlinks
//! (`libxkbcommon-dev`, `libxkbcommon-x11-dev`) aren't installed but the runtime
//! libraries are, point the linker at the runtime `.so.0` files.
use std::path::Path;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let out = std::env::var("OUT_DIR").unwrap();
    let mut added = false;
    for lib in ["xkbcommon", "xkbcommon-x11"] {
        let dev = format!("lib{lib}.so");
        let has_dev = ["/usr/lib/x86_64-linux-gnu", "/usr/lib64", "/usr/lib", "/usr/lib/aarch64-linux-gnu"]
            .iter()
            .any(|d| Path::new(d).join(&dev).exists());
        if has_dev {
            continue;
        }
        for dir in ["/usr/lib/x86_64-linux-gnu", "/usr/lib64", "/usr/lib", "/usr/lib/aarch64-linux-gnu"] {
            let runtime = Path::new(dir).join(format!("lib{lib}.so.0"));
            if runtime.exists() {
                let link = Path::new(&out).join(&dev);
                let _ = std::fs::remove_file(&link);
                #[cfg(unix)]
                std::os::unix::fs::symlink(&runtime, &link).expect("symlink");
                added = true;
                println!("cargo:warning=using runtime {} (install lib{lib}-dev to silence)", runtime.display());
                break;
            }
        }
    }
    if added {
        println!("cargo:rustc-link-search=native={out}");
    }
}

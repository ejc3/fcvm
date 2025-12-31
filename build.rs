use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;

fn main() {
    // Compute inception kernel SHA from build inputs
    let kernel_dir = Path::new("kernel");
    let mut content = Vec::new();

    // Read build.sh
    let script = kernel_dir.join("build.sh");
    if script.exists() {
        if let Ok(data) = fs::read(&script) {
            content.extend(data);
        }
    }

    // Read inception.conf
    let conf = kernel_dir.join("inception.conf");
    if conf.exists() {
        if let Ok(data) = fs::read(&conf) {
            content.extend(data);
        }
    }

    // Read patches/*.patch (sorted for determinism)
    let patches_dir = kernel_dir.join("patches");
    if patches_dir.exists() {
        if let Ok(entries) = fs::read_dir(&patches_dir) {
            let mut patches: Vec<_> = entries
                .filter_map(|e| e.ok())
                .filter(|e| e.path().extension().is_some_and(|ext| ext == "patch"))
                .collect();
            patches.sort_by_key(|e| e.path());
            for patch in patches {
                if let Ok(data) = fs::read(patch.path()) {
                    content.extend(data);
                }
            }
        }
    }

    // Compute SHA (first 12 chars of SHA256)
    let sha = if content.is_empty() {
        "unknown".to_string()
    } else {
        let mut hasher = Sha256::new();
        hasher.update(&content);
        let result = hasher.finalize();
        hex::encode(&result[..6])
    };

    println!("cargo:rustc-env=INCEPTION_KERNEL_SHA={}", sha);

    // Rerun if kernel sources change
    println!("cargo:rerun-if-changed=kernel/build.sh");
    println!("cargo:rerun-if-changed=kernel/inception.conf");
    println!("cargo:rerun-if-changed=kernel/patches");
}

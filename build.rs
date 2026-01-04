fn main() {
    // Rebuild when config changes (include_str! doesn't always trigger rebuilds)
    println!("cargo:rerun-if-changed=rootfs-config.toml");
}

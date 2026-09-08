//! Both pasta builds must include the upstream fix for issue #661.

#[test]
fn pasta_build_paths_pin_the_upstream_addr_seen_fix() {
    // This is the upstream merge of our carried fix. Its stable patch-id is
    // 340576aef02fa19411e69e3587566f3c4950057a, identical to the deleted patch.
    const FIX_COMMIT: &str = "3f57f0382f6a72c0b8ce0c5ff92248b5117ed9b6";
    const ARCHIVE_SHA256: &str = "2d698e3f7a96408231aa11bb1b27775de6964e2de7b0fa29399847d012044e3a";
    let config: toml::Value = toml::from_str(include_str!("../rootfs-config.toml")).unwrap();
    assert_eq!(config["pasta"]["commit"].as_str(), Some(FIX_COMMIT));
    assert_eq!(
        config["pasta"]["repo"].as_str(),
        Some("https://passt.top/passt")
    );

    let script = include_str!("../scripts/build-passt.sh");
    for (name, value) in [
        ("PASST_COMMIT", FIX_COMMIT),
        (
            "PASST_TARBALL_URL",
            "https://passt.top/passt/snapshot/passt-${PASST_COMMIT}.tar.xz",
        ),
        ("PASST_TARBALL_SHA256", ARCHIVE_SHA256),
    ] {
        assert!(
            script
                .lines()
                .any(|line| line == format!("{name}=\"{value}\"")),
            "runner build must use the same upstream fix and verified archive: {name}"
        );
    }
    for patch in [
        "pasta/patches/0001-tap-dont-let-overheard-traffic-move-addr_seen.patch",
        "scripts/passt-addr-seen.patch",
    ] {
        assert!(
            !std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join(patch)
                .exists(),
            "upstream already contains {patch}"
        );
    }
}

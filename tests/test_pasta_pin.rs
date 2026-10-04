//! Both pasta builds must use the same upstream commit, with no local patches.

#[test]
fn both_pasta_builds_pin_the_same_upstream_commit() {
    // The pin is the upstream fix that keeps `-a`, `-g` and `-n` on a host
    // without IPv4. It also contains 3f57f0382f6a, the upstream merge of the
    // addr_seen fix for issue #661 that this repo used to carry as a patch
    // (stable patch-id 340576aef02fa19411e69e3587566f3c4950057a, identical to
    // the deleted patch): `git merge-base --is-ancestor 3f57f0382f6a
    // 4e8aa70379a3` holds in the upstream repository. Nothing here checks that
    // ancestry, so a later pin has to be checked the same way.
    const PINNED_COMMIT: &str = "4e8aa70379a35ec9deb11d76513e7f6c4123b667";
    const ARCHIVE_SHA256: &str = "ef88ad2c6137b52286e6fcd10311d61f2ab329e5558abe097170e9e5851da8b9";
    let config: toml::Value = toml::from_str(include_str!("../rootfs-config.toml")).unwrap();
    assert_eq!(config["pasta"]["commit"].as_str(), Some(PINNED_COMMIT));
    assert_eq!(
        config["pasta"]["repo"].as_str(),
        Some("https://passt.top/passt")
    );

    let script = include_str!("../scripts/build-passt.sh");
    for (name, value) in [
        ("PASST_COMMIT", PINNED_COMMIT),
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
            "runner build must use the same upstream commit and verified archive: {name}"
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

//! Dependency policies that must remain true when Cargo resolves a new lockfile.

#[test]
fn mmds_client_needs_no_legacy_h2_or_advisory_exemption() {
    let lock: toml::Value = toml::from_str(include_str!("../Cargo.lock")).unwrap();
    let mut findings = Vec::new();
    if lock["package"].as_array().unwrap().iter().any(|package| {
        package["name"].as_str() == Some("h2")
            && package["version"].as_str().unwrap().starts_with("0.3.")
    }) {
        findings.push("Cargo.lock contains h2 0.3, which has no fix for RUSTSEC-2026-0258");
    }
    for (path, source) in [
        (".cargo/audit.toml", include_str!("../.cargo/audit.toml")),
        ("deny.toml", include_str!("../deny.toml")),
    ] {
        let config: toml::Value = toml::from_str(source).unwrap();
        if config["advisories"]["ignore"]
            .as_array()
            .unwrap()
            .iter()
            .any(|advisory| advisory.as_str() == Some("RUSTSEC-2026-0258"))
        {
            findings.push(path);
        }
    }
    assert!(
        findings.is_empty(),
        "legacy h2 or its retired advisory exception returned: {findings:?}"
    );
}

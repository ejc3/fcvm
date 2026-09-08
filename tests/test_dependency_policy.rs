//! Dependency policies that must remain true when Cargo resolves a new lockfile.

#[test]
fn unix_api_clients_need_no_legacy_hyper() {
    let lock: toml::Value = toml::from_str(include_str!("../Cargo.lock")).unwrap();
    let policy: toml::Value = toml::from_str(include_str!("../deny.toml")).unwrap();
    let mut findings = Vec::new();
    for (name, minimum, requirement) in [("hyper", (1, 0), "<1.0"), ("hyperlocal", (0, 9), "<0.9")]
    {
        for package in lock["package"].as_array().unwrap() {
            if package["name"].as_str() != Some(name) {
                continue;
            }
            let version = package["version"].as_str().unwrap();
            let mut parts = version.split('.');
            let major: u64 = parts.next().unwrap().parse().unwrap();
            let minor: u64 = parts.next().unwrap().parse().unwrap();
            if (major, minor) < minimum {
                findings.push(format!("Cargo.lock contains {name} {version}"));
            }
        }
        assert!(
            policy["bans"]["deny"]
                .as_array()
                .unwrap()
                .iter()
                .any(|ban| {
                    ban["name"].as_str() == Some(name)
                        && ban["version"].as_str() == Some(requirement)
                }),
            "deny.toml must forbid {name} {requirement}"
        );
    }
    assert!(
        findings.is_empty(),
        "legacy Unix HTTP dependencies: {findings:?}"
    );
}

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

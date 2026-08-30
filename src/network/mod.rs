pub mod bridged;
pub mod egress_proxy;
pub mod names;
pub mod namespace;
pub mod pasta;
pub mod portmap;
pub mod routed;
pub mod tcp_proxy;
pub mod types;
pub mod veth;

pub use bridged::BridgedNetwork;
pub use pasta::PastaNetwork;
pub use routed::RoutedNetwork;
pub use types::*;

use anyhow::{Context, Result};
use std::net::IpAddr;

/// Acquire a cross-process lock serializing host-global bridged network configuration.
///
/// Bridged networking mutates state shared by every fcvm process on the host
/// (veth host IPs, global MASQUERADE rules). The check-then-act sequences on that
/// state must hold one of these locks so two fcvm processes cannot interleave.
///
/// Lock files live in the state directory, following the same flock pattern as
/// `loopback-ip.lock` (world-writable so root and non-root processes can coordinate,
/// never deleted to avoid the recreate-while-locked race).
///
/// Lock ordering: `bridged-subnet.lock` may be held while `bridged-nat.lock` is
/// acquired (setup error paths that call cleanup()); the reverse never happens.
pub(crate) async fn acquire_host_network_lock(
    name: &str,
) -> Result<nix::fcntl::Flock<std::fs::File>> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let dir = crate::paths::state_dir();
    tokio::fs::create_dir_all(&dir)
        .await
        .with_context(|| format!("creating state directory {}", dir.display()))?;
    let path = dir.join(name);

    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o666)
        .open(&path)
        .with_context(|| format!("opening lock file {}", path.display()))?;
    // Force permissions regardless of umask (only effective if we own the file or are root)
    let _ = file.set_permissions(std::fs::Permissions::from_mode(0o666));

    // Acquire in spawn_blocking: flock blocks until the lock is free, and these
    // critical sections span multiple subprocess invocations, so don't pin a
    // tokio worker thread while waiting.
    let lock_name = name.to_string();
    tokio::task::spawn_blocking(move || {
        use nix::fcntl::{Flock, FlockArg};
        Flock::lock(file, FlockArg::LockExclusive)
            .map_err(|(_, err)| anyhow::anyhow!("flock failed: {}", err))
    })
    .await
    .context("joining lock acquisition task")?
    .with_context(|| format!("acquiring host network lock {}", lock_name))
}

/// Network manager trait
#[async_trait::async_trait]
pub trait NetworkManager: Send + Sync {
    /// Setup network before VM start
    async fn setup(&mut self) -> Result<NetworkConfig>;

    /// Post-VM-start setup (e.g., start pasta after Firecracker creates namespace)
    /// Called with the PID of the VM process (Firecracker or unshare wrapper).
    /// Default implementation does nothing.
    async fn post_start(&mut self, _vm_pid: u32) -> Result<()> {
        Ok(())
    }

    /// SIGKILL any long-lived helper process this network owns, WITHOUT waiting for it.
    ///
    /// Lets teardown signal the network helper in the same instant as the VMM and the
    /// namespace holder, so the helper's exit overlaps the VMM's address-space reclaim
    /// instead of queueing behind it; [`Self::cleanup`] then reaps a process that is
    /// already dead. Purely an optimization — `cleanup()` still kills and reaps on its
    /// own, so calling this is optional and calling it twice is harmless.
    ///
    /// Default: no-op (bridged and routed have no helper process).
    fn start_kill_processes(&mut self) {}

    /// Cleanup network after VM stop
    async fn cleanup(&mut self) -> Result<()>;

    /// Get the TAP device name
    fn tap_device(&self) -> &str;

    /// Verify port forwarding works end-to-end after VM is running.
    ///
    /// Called after snapshot restore when the guest is active and fc-agent has reconnected.
    /// Verifies that data actually flows through the forwarding path, not just that
    /// the listening socket exists. Default implementation does nothing (bridged DNAT
    /// is kernel-level and works immediately).
    async fn verify_port_forwarding(&self) -> Result<()> {
        Ok(())
    }

    /// Get a reference to Any for downcasting
    fn as_any(&self) -> &dyn std::any::Any;
}

/// The resolv.conf files fcvm reads to learn the host's nameservers, most
/// authoritative first.
///
/// On a systemd-resolved host the run file holds the upstream servers and
/// `/etc/resolv.conf` is a symlink to the 127.0.0.53 stub; on every other host
/// only `/etc/resolv.conf` exists; and inside an fcvm guest, where resolved is
/// not enabled, the run file can exist while carrying no nameserver at all.
/// Any one of them can be the file with a usable server, so all are read and
/// their nameservers unioned.
pub const RESOLV_CONF_SOURCES: [&str; 2] = ["/run/systemd/resolve/resolv.conf", ETC_RESOLV_CONF];

/// The conventional resolver configuration file: one of the host sources
/// above, and the file fc-agent writes inside a guest.
///
/// Named here so nothing else in the crate spells the path, and the source
/// guard in this module's tests can stay an exact literal scan. A test that
/// inspects a guest's resolv.conf is not choosing the host's resolvers, but it
/// writes the same path, and a scan cannot tell the two apart.
pub const ETC_RESOLV_CONF: &str = "/etc/resolv.conf";

/// One resolv.conf source and what reading it produced.
#[derive(Debug, Clone)]
pub struct ResolvSource {
    pub path: String,
    /// The file body, or the reason it could not be read.
    pub content: Result<String, String>,
}

impl ResolvSource {
    /// Read one source. A read failure is recorded rather than returned: a
    /// missing or unreadable file disqualifies that source, not the lookup.
    pub fn read(path: &str) -> Self {
        Self {
            path: path.to_string(),
            content: std::fs::read_to_string(path).map_err(|e| e.to_string()),
        }
    }

    /// Every address on a `nameserver` line, in file order, parsed or not.
    fn nameservers(&self) -> Vec<&str> {
        let Ok(content) = &self.content else {
            return Vec::new();
        };
        content
            .lines()
            .filter_map(|line| {
                let mut parts = line.split_whitespace();
                match (parts.next(), parts.next()) {
                    (Some("nameserver"), Some(addr)) => Some(addr),
                    _ => None,
                }
            })
            .collect()
    }

    /// The nameservers a VM can actually use: parseable as an IP address and
    /// not a loopback stub, which the VM has no route to.
    fn usable_nameservers(&self) -> Vec<String> {
        self.nameservers()
            .into_iter()
            .filter(|addr| is_usable_nameserver(addr))
            .map(str::to_string)
            .collect()
    }

    /// The domains on this source's first `search` line.
    ///
    /// Tokenised on whitespace, not on a literal `"search "`: resolv.conf(5)
    /// separates a directive from its arguments with spaces or tabs, and
    /// matching the space alone silently dropped every tab-separated search
    /// list. A `search` line naming nothing is still the first search line and
    /// still decides, which is how an empty search list is expressed.
    fn search_domains(&self) -> Vec<String> {
        let Ok(content) = &self.content else {
            return Vec::new();
        };
        content
            .lines()
            .find_map(|line| {
                let mut parts = line.split_whitespace();
                if parts.next() != Some("search") {
                    return None;
                }
                Some(parts.map(str::to_string).collect())
            })
            .unwrap_or_default()
    }

    /// One clause naming what this source contributed, for the error.
    fn summary(&self) -> String {
        if let Err(reason) = &self.content {
            return format!("{} unreadable ({reason})", self.path);
        }
        let found = self.nameservers();
        if found.is_empty() {
            return format!("{} has no nameserver line", self.path);
        }
        let usable = self.usable_nameservers();
        if usable.is_empty() {
            return format!("{} has only unusable {}", self.path, found.join(", "));
        }
        format!("{} has {}", self.path, usable.join(", "))
    }
}

/// The non-loopback nameservers named by any source, most authoritative first,
/// de-duplicated with the first occurrence winning.
///
/// Pure over the source bodies so the decision is testable without touching
/// the filesystem. Every source contributes: committing to the first readable
/// one loses a good `/etc/resolv.conf` behind a stub-only run file (#875).
/// Fails when no source yielded a usable server, and the error names what each
/// source held.
pub fn nameservers_from_sources(sources: &[ResolvSource]) -> anyhow::Result<Vec<String>> {
    let mut servers: Vec<String> = Vec::new();
    for source in sources {
        for server in source.usable_nameservers() {
            if !servers.contains(&server) {
                servers.push(server);
            }
        }
    }

    if servers.is_empty() {
        let detail = sources
            .iter()
            .map(ResolvSource::summary)
            .collect::<Vec<_>>()
            .join("; ");
        anyhow::bail!(
            "no usable DNS server: {detail}. A VM cannot reach a loopback stub \
             resolver; pass --dns to name a server explicitly."
        );
    }

    Ok(servers)
}

/// One source's usable resolvers together with the search domains that belong
/// to them.
///
/// The pairing is the point. A search domain is only meaningful against a
/// resolver that knows it, so a domain must not outlive the selection of its
/// resolvers. Bridged mode forwards ONE server, and a flattened
/// (servers, domains) pair let the server list be narrowed while the whole
/// merged search list stayed, which expanded a short name with a private
/// suffix from a source whose resolver was never selected and asked the
/// resolver that was.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolverGroup {
    pub servers: Vec<String>,
    pub search_domains: Vec<String>,
}

/// Each source's usable servers paired with its search domains.
///
/// A source that named no usable server is dropped, so it cannot contribute a
/// search domain either: that is the #875 rule, held here by construction
/// rather than by a separate check.
pub fn resolver_groups(sources: &[ResolvSource]) -> Vec<ResolverGroup> {
    sources
        .iter()
        .filter_map(|source| {
            let servers = source.usable_nameservers();
            if servers.is_empty() {
                return None;
            }
            Some(ResolverGroup {
                servers,
                search_domains: source.search_domains(),
            })
        })
        .collect()
}

/// The first IPv6 nameserver any group names, in source order.
///
/// Routed mode gives the guest exactly this server when a source names one: an
/// IPv4 resolver is unreachable from a routed guest without MASQUERADE, so
/// `RoutedNetwork::setup` picks the first IPv6 one. That choice reads only the
/// resolv.conf sources, so it can be made before the launch config exists, and
/// both the selection and the search-domain narrowing call this one function
/// rather than each spelling the rule out.
///
/// Every server in a group is already a parsed, non-loopback address, so
/// "is IPv6" and the older "contains a colon" test select the same entry.
pub fn first_ipv6_nameserver(groups: &[ResolverGroup]) -> Option<String> {
    groups
        .iter()
        .flat_map(|group| group.servers.iter())
        .find(|server| {
            server
                .parse::<IpAddr>()
                .is_ok_and(|address| address.is_ipv6())
        })
        .cloned()
}

/// The IPv6 address of the AWS VPC resolver, which routed mode probes when no
/// resolv.conf source names an IPv6 server.
///
/// Spelled once, next to the rule for what this resolver serves, so the probe
/// and the search-domain narrowing that depends on it cannot drift apart.
pub const AWS_VPC_IPV6_RESOLVER: &str = "fd00:ec2::253";

/// A search domain that is an AWS VPC internal zone.
///
/// The zone is `<region>.compute.internal` outside us-east-1, or `ec2.internal`
/// in us-east-1, and it is the domain-name AWS DHCP hands the host. The
/// `<region>` label is validated, not merely the suffix: `foo.compute.internal`
/// is a custom domain-name, and the measurement on [`sole_aws_vpc_zone`]
/// records the probed resolver answering it NXDOMAIN exactly as it answers an
/// unrelated private suffix. Accepting it forwards a suffix that cannot
/// resolve, and hides a real zone named after it.
fn is_aws_vpc_internal_zone(domain: &str) -> bool {
    let domain = normalized_zone(domain);
    domain == "ec2.internal"
        || domain
            .strip_suffix(".compute.internal")
            .is_some_and(|region| region != "us-east-1" && is_aws_region_label(region))
}

/// An AWS region name: two or more lowercase words of at least two letters,
/// then a decimal number. `us-west-2` and `eu-central-1` through
/// `ap-southeast-4`, `il-central-1`, `us-gov-west-1`, `cn-northwest-1`,
/// `us-isob-east-1` and `eusc-de-east-1`.
///
/// The shape is loose about word LENGTHS and POSITIONS because the first
/// version of this check was not, and that is what broke it. It required a
/// two-letter first word followed by words of at least three letters, and
/// claimed in this comment that every region in every partition has that form.
/// The AWS European Sovereign Cloud region in Brandenburg has neither: `eusc`
/// is four letters and `de` is two. An instance there carries a real
/// `eusc-de-east-1.compute.internal` zone that the check called a custom
/// domain-name, so the probe path dropped the host's own zone (#891).
///
/// That is the staleness failure a pattern was chosen over a list to avoid, so
/// the argument has to be stated more carefully than it was. A list admits no
/// non-region at all, which is the stronger guarantee, but it rejects a real
/// zone the day AWS opens a region and reports nothing when it does. A pattern
/// avoids that only where it encodes properties AWS's namespace actually has.
/// Where it encodes a guess about them it goes stale on the same schedule and
/// just as quietly, which is what a positional model of the name turned out to
/// be. One uniform claim about every word makes fewer guesses to be wrong
/// about, and `every_published_aws_region_label_is_recognized`
/// checks the remaining ones against AWS's own region list rather than against
/// the examples someone remembered.
///
/// The looseness is affordable because a false accept is bounded at both ends
/// and a false reject is not. Beside a real zone a false accept makes the input
/// ambiguous and [`sole_aws_vpc_zone`] forwards neither. Alone it is a
/// `.compute.internal` name, which the probed resolver answers NXDOMAIN with no
/// authority instead of recursing it into the public DNS the way it recurses
/// `db.corp.example`, so the label reaches AWS's resolver and stops there. A
/// false reject is a legitimate EC2 host silently losing its own zone for as
/// long as nobody notices.
///
/// So `my-test-1.compute.internal` and `usa-west-1.compute.internal` are
/// accepted, and that is the price. `foo.compute.internal`, the custom
/// domain-name whose NXDOMAIN the measurement on [`sole_aws_vpc_zone`] records,
/// still is not: it names no region at all.
fn is_aws_region_label(label: &str) -> bool {
    let parts: Vec<&str> = label.split('-').collect();
    let [words @ .., number] = parts.as_slice() else {
        return false;
    };
    words.len() >= 2
        && words
            .iter()
            .all(|word| word.len() >= 2 && word.bytes().all(|b| b.is_ascii_lowercase()))
        && !number.is_empty()
        && number.bytes().all(|b| b.is_ascii_digit())
}

/// A search domain compared the way DNS compares names: case-insensitively,
/// with the trailing root label a presentation detail rather than a difference.
fn normalized_zone(domain: &str) -> String {
    domain.trim_end_matches('.').to_ascii_lowercase()
}

/// The one AWS VPC internal zone this host's search list names, or `None` when
/// it names none or several.
///
/// Measured on an EC2 instance in us-west-1, against [`AWS_VPC_IPV6_RESOLVER`]
/// and against the same service's 10.0.0.2 and 169.254.169.253, all three
/// returning identical verdicts:
///
/// | query | verdict |
/// |---|---|
/// | `ip-10-0-1-49.us-west-1.compute.internal` | NOERROR, A 10.0.1.49 |
/// | `foo.compute.internal` | NXDOMAIN, no authority |
/// | `ip-10-0-1-49.eu-west-1.compute.internal` | NXDOMAIN, no authority |
/// | `db.corp.example` | NXDOMAIN carrying the root SOA |
/// | `secret-host.internal.example.com` | NOERROR carrying example.com's SOA at Cloudflare |
///
/// AWS gives an instance exactly one internal zone and its resolver is
/// authoritative for that one. Rows two and three are what a zone-shape test
/// forwards and this resolver will not answer. Rows four and five are why a
/// suffix it does not serve is dropped rather than merely tried later: it is
/// recursed into the public DNS, which discloses the private label to AWS's
/// resolver and then to the parent zone's authoritative servers (CWE-200), and
/// still does not resolve. The kept zone is private to that resolver: the same
/// `ip-10-0-1-49.us-west-1.compute.internal` is NXDOMAIN at 1.1.1.1.
///
/// Read off the search list, with no lookup of any kind.
/// `GuestBootInputs::for_launch` runs before `RoutedNetwork::setup` probes, and
/// its result is hashed into the snapshot key, so this has to be a pure
/// function of the same resolv.conf snapshot the key is computed from. That is
/// what rules out the alternatives: the instance's region from IMDS is an HTTP
/// request and the resolver's own verdict is a DNS query, both then taken at
/// key time, and on a host that is not on EC2 both have to time out first.
///
/// Which leaves a host that names two. Nothing in a resolv.conf snapshot says
/// which link a search domain came from. Order does not: it decides how a
/// short name is expanded, and systemd-resolved merges every link's domains
/// into one file in an order of its own, so a VPN link's zone can precede the
/// host's. Pairing a domain with a nameserver does not either, because that
/// same merge collapses every link onto the 127.0.0.53 stub. So do not pick
/// one. Forward none and let the guest use fully qualified names: losing
/// short-name completion is something a guest can work around, and handing a
/// private label to a resolver that NXDOMAINs it while dropping the zone that
/// would have worked is not.
fn sole_aws_vpc_zone(groups: &[ResolverGroup]) -> Option<String> {
    let mut zones: Vec<String> = search_domains_of(groups)
        .iter()
        .filter(|domain| is_aws_vpc_internal_zone(domain))
        .map(|domain| normalized_zone(domain))
        .collect();
    zones.sort();
    zones.dedup();
    match zones.as_slice() {
        [zone] => Some(zone.clone()),
        _ => None,
    }
}

/// The groups carrying this host's AWS VPC internal zone and nothing else,
/// servers untouched.
///
/// What routed mode applies when no source named an IPv6 server and
/// `detect_ipv6_dns` fell back to probing [`AWS_VPC_IPV6_RESOLVER`]. That
/// address belongs to no source, so there is no group to select and
/// [`narrowed_to`] does not apply; the probed server replaces the guest's
/// resolver list in the network config rather than in these groups, which is
/// why only the search side narrows here.
///
/// `sole_aws_vpc_zone` holds which zone that is, and names none on a host
/// carrying none or several. Three kinds of host therefore keep no search
/// domain.
///
/// One is not on EC2 and lands here because "no source names an IPv6 server"
/// is all `for_launch` can see. Nothing is lost: fd00:ec2::253 is a
/// VPC-internal address that does not answer off EC2, so the probe returns
/// None, routed hands the guest no IPv6 resolver at all, and the IPv4 servers
/// it gets instead are unreachable from a routed guest. No suffix had a
/// resolver to be completed against.
///
/// One is an EC2 instance whose VPC overrides the DHCP domain-name, so its own
/// zone names no region even though its resolver may serve it.
///
/// One names two VPC zones, its own and another VPC's arriving over a VPN link
/// or a merged resolved list, and the snapshot does not say which is which.
///
/// The last two lose short-name completion on routed. That is the conservative
/// side of a decision made from the search list alone: keeping the list whole
/// instead hands every private suffix to the AWS resolver on every host where
/// the probe does answer, which is the leak this narrowing exists to close.
pub fn search_narrowed_to_sole_aws_vpc_zone(groups: Vec<ResolverGroup>) -> Vec<ResolverGroup> {
    let zone = sole_aws_vpc_zone(&groups);
    groups
        .into_iter()
        .map(|group| ResolverGroup {
            servers: group.servers,
            search_domains: group
                .search_domains
                .into_iter()
                .filter(|domain| zone.as_deref() == Some(normalized_zone(domain).as_str()))
                .collect(),
        })
        .collect()
}

/// The groups narrowed to one server: those that named it, carrying only it.
///
/// What a mode that forwards a single resolver applies. Dropping the other
/// servers takes their search domains with them, because a domain lives in the
/// same value as the servers it belongs to.
pub fn narrowed_to(groups: Vec<ResolverGroup>, server: &str) -> Vec<ResolverGroup> {
    groups
        .into_iter()
        .filter(|group| group.servers.iter().any(|s| s == server))
        .map(|group| ResolverGroup {
            servers: vec![server.to_string()],
            search_domains: group.search_domains,
        })
        .collect()
}

/// The search domains of the groups that still carry a resolver, in order,
/// de-duplicated with the first occurrence winning.
///
/// Groups with no server contribute nothing, so narrowing a guest's resolvers
/// to a subset of the groups narrows its search list with them. That filter is
/// a second guard rather than the only one: [`resolver_groups`] already drops
/// a source that named no usable server, and a narrowing mode drops the whole
/// group. It holds the same rule for a group built by hand.
pub fn search_domains_of(groups: &[ResolverGroup]) -> Vec<String> {
    let mut domains: Vec<String> = Vec::new();
    for group in groups.iter().filter(|g| !g.servers.is_empty()) {
        for domain in &group.search_domains {
            if !domains.contains(domain) {
                domains.push(domain.clone());
            }
        }
    }
    domains
}

/// A nameserver a VM can route to: an IP address that is not loopback.
///
/// An entry that does not parse as an IP (a scoped IPv6 address, a hostname, a
/// truncated line) is dropped rather than forwarded to the guest, which could
/// do nothing with it either.
fn is_usable_nameserver(server: &str) -> bool {
    match server.parse::<IpAddr>() {
        Ok(ip) => !ip.is_loopback(),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn readable(path: &str, content: &str) -> ResolvSource {
        ResolvSource {
            path: path.to_string(),
            content: Ok(content.to_string()),
        }
    }

    fn unreadable(path: &str, reason: &str) -> ResolvSource {
        ResolvSource {
            path: path.to_string(),
            content: Err(reason.to_string()),
        }
    }

    /// The systemd-resolved stub: the exact body a resolved host serves at
    /// /etc/resolv.conf, and what the failing runner in #875 had.
    const STUB: &str =
        "# Dynamic resolv.conf\nnameserver 127.0.0.53\noptions edns0 trust-ad\nsearch .\n";

    #[test]
    fn nameserver_lines_parse_with_comments_tabs_and_malformed_entries() {
        let source = readable(
            "/etc/resolv.conf",
            "# comment\n\
             ; another comment\n\
             #nameserver 10.0.0.1\n\
             # nameserver 10.0.0.2\n\
             nameserver\n\
             nameserver not-an-ip\n\
             nameserver fe80::1%eth0\n\
             \tnameserver\t8.8.8.8\n\
             nameserver 2001:4860:4860::8888 # trailing comment\n\
             nameserver 127.0.0.53\n\
             nameserver ::1\n\
             options edns0\n",
        );

        assert_eq!(
            source.usable_nameservers(),
            vec!["8.8.8.8", "2001:4860:4860::8888"],
            "commented-out, addressless, unparseable and loopback entries must all be \
             dropped, IPv4 and IPv6 servers kept in file order"
        );
    }

    #[test]
    fn loopback_nameservers_are_never_usable() {
        assert!(!is_usable_nameserver("127.0.0.53"));
        assert!(!is_usable_nameserver("127.0.0.1"));
        assert!(!is_usable_nameserver("::1"));
        assert!(!is_usable_nameserver("0:0:0:0:0:0:0:1"));
        assert!(is_usable_nameserver("8.8.8.8"));
        assert!(is_usable_nameserver("2001:4860:4860::8888"));
    }

    /// #875: the run file exists and carries only the stub, so committing to
    /// the first readable source reports no usable DNS while /etc/resolv.conf
    /// names a real one. Both must be read.
    #[test]
    fn stub_only_run_file_does_not_hide_a_usable_etc_resolv_conf() {
        let servers = nameservers_from_sources(&[
            readable("/run/systemd/resolve/resolv.conf", STUB),
            readable("/etc/resolv.conf", "nameserver 10.1.0.2\n"),
        ])
        .expect("a usable server in any source is a usable server");

        assert_eq!(servers, vec!["10.1.0.2"]);
    }

    /// The same shape with the run file readable but empty of nameservers,
    /// which is what an fcvm guest gets: resolved is not enabled there, so the
    /// run file never receives upstream servers.
    #[test]
    fn nameserver_free_run_file_does_not_hide_a_usable_etc_resolv_conf() {
        let servers = nameservers_from_sources(&[
            readable(
                "/run/systemd/resolve/resolv.conf",
                "# This is /run/systemd/resolve/resolv.conf managed by man:systemd-resolved(8).\n",
            ),
            readable("/etc/resolv.conf", "search corp\nnameserver 10.1.0.2\n"),
        ])
        .expect("an empty run file must not disqualify /etc/resolv.conf");

        assert_eq!(servers, vec!["10.1.0.2"]);
    }

    #[test]
    fn every_source_stub_only_is_a_failure() {
        let err = nameservers_from_sources(&[
            readable("/run/systemd/resolve/resolv.conf", STUB),
            readable("/etc/resolv.conf", STUB),
        ])
        .expect_err("a loopback stub is not a DNS server a VM can reach");

        let message = format!("{err:#}");
        assert!(
            message.contains("/run/systemd/resolve/resolv.conf has only unusable 127.0.0.53"),
            "the error must name each source and what it held: {message}"
        );
        assert!(
            message.contains("/etc/resolv.conf has only unusable 127.0.0.53"),
            "the error must name each source and what it held: {message}"
        );
        assert!(
            !message.contains("mount /run/systemd/resolve"),
            "the container mount advice is wrong inside a guest, where resolved is \
             not enabled at all: {message}"
        );
    }

    #[test]
    fn an_unreadable_source_does_not_disqualify_a_good_one() {
        let servers = nameservers_from_sources(&[
            unreadable(
                "/run/systemd/resolve/resolv.conf",
                "No such file or directory (os error 2)",
            ),
            readable("/etc/resolv.conf", "nameserver 8.8.8.8\n"),
        ])
        .expect("a host without systemd-resolved still has /etc/resolv.conf");
        assert_eq!(servers, vec!["8.8.8.8"]);

        let servers = nameservers_from_sources(&[
            readable("/run/systemd/resolve/resolv.conf", "nameserver 8.8.4.4\n"),
            unreadable("/etc/resolv.conf", "Permission denied (os error 13)"),
        ])
        .expect("an unreadable /etc/resolv.conf must not hide the run file");
        assert_eq!(servers, vec!["8.8.4.4"]);
    }

    #[test]
    fn no_source_readable_names_both_failures() {
        let err = nameservers_from_sources(&[
            unreadable(
                "/run/systemd/resolve/resolv.conf",
                "No such file or directory (os error 2)",
            ),
            unreadable("/etc/resolv.conf", "Permission denied (os error 13)"),
        ])
        .expect_err("no source could be read, so no server is known");

        let message = format!("{err:#}");
        assert!(
            message.contains("/run/systemd/resolve/resolv.conf unreadable (No such file"),
            "the error must say which file could not be read and why: {message}"
        );
        assert!(
            message.contains("/etc/resolv.conf unreadable (Permission denied"),
            "the error must say which file could not be read and why: {message}"
        );
    }

    #[test]
    fn sources_union_in_order_without_duplicates() {
        let servers = nameservers_from_sources(&[
            readable(
                "/run/systemd/resolve/resolv.conf",
                "nameserver 10.1.0.2\nnameserver 8.8.8.8\n",
            ),
            readable(
                "/etc/resolv.conf",
                "nameserver 8.8.8.8\nnameserver 1.1.1.1\n",
            ),
        ])
        .expect("both sources name usable servers");

        assert_eq!(
            servers,
            vec!["10.1.0.2", "8.8.8.8", "1.1.1.1"],
            "the run file's servers come first and a server named twice appears once"
        );
    }

    #[test]
    fn search_domains_take_the_first_search_line() {
        assert_eq!(
            readable(
                "/etc/resolv.conf",
                "nameserver 192.0.2.1\nsearch corp.example internal\n"
            )
            .search_domains(),
            vec!["corp.example", "internal"]
        );
        assert_eq!(
            readable("/etc/resolv.conf", "search a.example\nsearch b.example\n").search_domains(),
            vec!["a.example"]
        );
        assert!(readable("/etc/resolv.conf", "nameserver 192.0.2.1\n")
            .search_domains()
            .is_empty());
    }

    /// resolv.conf(5) separates a directive from its arguments with spaces or
    /// tabs. Matching a literal "search " dropped every tab-separated search
    /// list, and a guest that gets no search list cannot resolve short names.
    #[test]
    fn search_domains_accept_any_whitespace_after_the_directive() {
        assert_eq!(
            readable("/etc/resolv.conf", "search\tcorp.example\n").search_domains(),
            vec!["corp.example"],
            "a tab separates the directive from its arguments"
        );
        assert_eq!(
            readable(
                "/etc/resolv.conf",
                "search \t corp.example\t\tinternal  other.example\n"
            )
            .search_domains(),
            vec!["corp.example", "internal", "other.example"],
            "runs of mixed spaces and tabs separate the domains too"
        );
        assert_eq!(
            readable("/etc/resolv.conf", "  search corp.example  \t\n").search_domains(),
            vec!["corp.example"],
            "whitespace before the directive and after the last domain is not a domain"
        );
        assert!(
            readable("/etc/resolv.conf", "searchdomain corp.example\n")
                .search_domains()
                .is_empty(),
            "a directive that merely starts with `search` is a different directive"
        );
    }

    /// A `search` naming nothing is still the first search line, and under the
    /// first-line rule it decides. Skipping it to reach a later line would
    /// make an empty search list unreachable.
    #[test]
    fn a_search_line_with_no_domains_yields_no_domains() {
        assert!(
            readable("/etc/resolv.conf", "search\nsearch other.example\n")
                .search_domains()
                .is_empty(),
            "the first search line names nothing, so nothing is searched"
        );
        assert!(
            readable("/etc/resolv.conf", "search   \t\n")
                .search_domains()
                .is_empty(),
            "trailing whitespace is not a domain"
        );
    }

    /// The `nameserver` directive is read on the same path. It already splits
    /// on whitespace rather than matching a literal space, so it was not
    /// narrow in the way `search` was; this holds that.
    #[test]
    fn nameservers_accept_any_whitespace_after_the_directive() {
        let servers = nameservers_from_sources(&[readable(
            "/etc/resolv.conf",
            "nameserver\t10.1.0.2\n  nameserver \t 8.8.8.8  \n",
        )])
        .expect("both lines name a usable server");
        assert_eq!(servers, vec!["10.1.0.2", "8.8.8.8"]);

        assert!(
            nameservers_from_sources(&[readable(
                "/etc/resolv.conf",
                "nameserverfoo 10.1.0.2\nnameserver\n"
            )])
            .is_err(),
            "a lookalike directive and a bare `nameserver` name no server"
        );
    }

    /// The routed selection moved out of detect_ipv6_dns so that the narrowing
    /// and the selection cannot drift. This holds the move behaviour-preserving
    /// against the expression it replaced.
    #[test]
    fn first_ipv6_nameserver_selects_what_routed_selected_before() {
        fn previous(sources: &[ResolvSource]) -> Option<String> {
            nameservers_from_sources(sources)
                .unwrap_or_default()
                .into_iter()
                .find(|server| server.contains(':'))
        }

        for (run_body, etc_body) in [
            ("nameserver 10.1.0.2\n", "nameserver 2001:db8::53\n"),
            ("nameserver 2001:db8::53\n", "nameserver 10.1.0.2\n"),
            ("nameserver 10.1.0.2\n", "nameserver 192.0.2.53\n"),
            ("nameserver ::1\n", "nameserver 2001:db8::53\n"),
            ("nameserver 2001:db8::53\n", "nameserver 2001:db8::53\n"),
            ("search .\n", "nameserver fd00:ec2::253\n"),
        ] {
            let sources = [
                readable(RESOLV_CONF_SOURCES[0], run_body),
                readable(ETC_RESOLV_CONF, etc_body),
            ];
            assert_eq!(
                first_ipv6_nameserver(&resolver_groups(&sources)),
                previous(&sources),
                "run={run_body:?} etc={etc_body:?}"
            );
        }
    }

    /// Narrowing to a server keeps the groups that named it, carrying only it,
    /// and takes the other groups' search domains with them.
    #[test]
    fn narrowing_to_a_server_drops_the_other_groups_domains() {
        let groups = resolver_groups(&[
            readable(
                RESOLV_CONF_SOURCES[0],
                "nameserver 10.1.0.2\nsearch corp.example\n",
            ),
            readable(
                ETC_RESOLV_CONF,
                "nameserver 2001:db8::53\nnameserver 8.8.8.8\nsearch lab.example\n",
            ),
        ]);

        let narrowed = narrowed_to(groups, "2001:db8::53");
        assert_eq!(narrowed.len(), 1, "only the group that named it survives");
        assert_eq!(narrowed[0].servers, vec!["2001:db8::53"]);
        assert_eq!(search_domains_of(&narrowed), vec!["lab.example"]);
    }

    /// Routed's probe fallback selects a resolver no source named, so there is
    /// no group to narrow to. The search domains narrow instead, to the one
    /// zone the probed AWS VPC resolver answers (#886).
    #[test]
    fn the_probed_aws_resolver_keeps_only_the_zone_it_serves() {
        let groups = resolver_groups(&[
            readable(
                RESOLV_CONF_SOURCES[0],
                "nameserver 10.0.0.2\nsearch us-west-1.compute.internal\n",
            ),
            readable(
                ETC_RESOLV_CONF,
                "nameserver 10.99.0.53\nsearch corp.example lab.internal\n",
            ),
        ]);

        let narrowed = search_narrowed_to_sole_aws_vpc_zone(groups);
        assert_eq!(
            search_domains_of(&narrowed),
            vec!["us-west-1.compute.internal"],
            "the VPC zone resolves at the probed server; the other suffixes are \
             recursed into the public DNS and never resolve"
        );
        assert_eq!(
            narrowed
                .iter()
                .flat_map(|group| group.servers.iter())
                .cloned()
                .collect::<Vec<_>>(),
            vec!["10.0.0.2", "10.99.0.53"],
            "the probed server replaces the guest's resolver in the network \
             config, so the servers here are not the thing being narrowed"
        );
    }

    /// Two AWS VPC internal zones on one host: its own, and another VPC's
    /// arriving over a VPN link or a merged systemd-resolved list. Exactly one
    /// is this host's and the probed resolver answers NXDOMAIN for the other,
    /// measured in us-west-1 as `ip-10-0-1-49.eu-west-1.compute.internal`
    /// NXDOMAIN with no authority. Nothing in the snapshot says which is which.
    ///
    /// Order does not, which is what this test holds: it runs the same pair
    /// both ways round, and the previous rule of taking the first AWS-shaped
    /// entry answered differently for each. resolv.conf order decides how a
    /// short name is expanded, not which link contributed a domain, and
    /// resolved merges every link's domains into one file with no marking.
    ///
    /// So neither is forwarded. The guest loses short-name completion and
    /// recovers it by using a fully qualified name; guessing instead hands a
    /// private label to a resolver that cannot answer it and drops the zone
    /// that would have worked, which the guest cannot recover from.
    #[test]
    fn two_aws_vpc_zones_are_ambiguous_so_the_probe_narrowing_forwards_neither() {
        for (first, second) in [
            ("us-west-1.compute.internal", "eu-west-1.compute.internal"),
            ("eu-west-1.compute.internal", "us-west-1.compute.internal"),
            ("ec2.internal", "us-west-1.compute.internal"),
        ] {
            let groups = resolver_groups(&[
                readable(
                    RESOLV_CONF_SOURCES[0],
                    &format!("nameserver 10.0.0.2\nsearch {first}\n"),
                ),
                readable(
                    ETC_RESOLV_CONF,
                    &format!("nameserver 10.99.0.53\nsearch {second} corp.example\n"),
                ),
            ]);

            let narrowed = search_narrowed_to_sole_aws_vpc_zone(groups);
            assert!(
                search_domains_of(&narrowed).is_empty(),
                "{first} before {second}: two VPC zones, and the snapshot does not \
                 say which one this host owns, so neither may be forwarded"
            );
            assert_eq!(
                narrowed
                    .iter()
                    .flat_map(|group| group.servers.iter())
                    .cloned()
                    .collect::<Vec<_>>(),
                vec!["10.0.0.2", "10.99.0.53"],
                "the servers are untouched even when every suffix goes"
            );
        }
    }

    /// us-east-1 uses `ec2.internal`, not the region-shaped zone used by every
    /// other AWS region. Treating the unused spelling as this host's zone
    /// forwards a suffix the VPC resolver does not serve.
    #[test]
    fn us_east_1_compute_internal_is_not_a_vpc_zone() {
        let groups = resolver_groups(&[readable(
            RESOLV_CONF_SOURCES[0],
            "nameserver 10.0.0.2\nsearch us-east-1.compute.internal\n",
        )]);

        assert!(
            search_domains_of(&search_narrowed_to_sole_aws_vpc_zone(groups)).is_empty(),
            "us-east-1's VPC resolver serves ec2.internal, not \
             us-east-1.compute.internal"
        );
    }

    /// The unused us-east-1 spelling is not a second VPC zone. If it appears
    /// beside `ec2.internal`, the real zone remains unambiguous and survives.
    #[test]
    fn us_east_1_compute_internal_does_not_hide_ec2_internal() {
        let groups = resolver_groups(&[readable(
            RESOLV_CONF_SOURCES[0],
            "nameserver 10.0.0.2\nsearch us-east-1.compute.internal ec2.internal\n",
        )]);

        assert_eq!(
            search_domains_of(&search_narrowed_to_sole_aws_vpc_zone(groups)),
            vec!["ec2.internal"],
            "the unused spelling must not make us-east-1's real zone ambiguous"
        );
    }

    /// `foo.compute.internal` is a custom DHCP domain-name, not a region's VPC
    /// zone, and the measurement records the probed resolver answering it
    /// NXDOMAIN with no authority. A suffix test takes it for this host's zone,
    /// which forwards a suffix that cannot resolve AND drops the real zone
    /// named after it. Validating the region label separates the two.
    #[test]
    fn a_compute_internal_suffix_naming_no_region_is_not_a_vpc_zone() {
        let groups = resolver_groups(&[
            readable(
                RESOLV_CONF_SOURCES[0],
                "nameserver 10.0.0.2\nsearch foo.compute.internal\n",
            ),
            readable(
                ETC_RESOLV_CONF,
                "nameserver 10.99.0.53\nsearch us-west-1.compute.internal\n",
            ),
        ]);

        assert_eq!(
            search_domains_of(&search_narrowed_to_sole_aws_vpc_zone(groups)),
            vec!["us-west-1.compute.internal"],
            "foo names no region, so it is not a second VPC zone, the input is \
             not ambiguous, and the real zone behind it survives"
        );
    }

    /// Several sources naming the SAME zone is one zone, not an ambiguity. A
    /// merged resolved list repeats a link's domain, and a host whose run file
    /// and /etc/resolv.conf both carry the DHCP domain-name names it twice.
    /// Case and a trailing root label are presentation, so those spellings are
    /// the same zone too and the count stays at one.
    #[test]
    fn one_zone_named_by_several_sources_is_not_two_zones() {
        let groups = resolver_groups(&[
            readable(
                RESOLV_CONF_SOURCES[0],
                "nameserver 10.0.0.2\nsearch us-west-1.compute.internal\n",
            ),
            readable(
                ETC_RESOLV_CONF,
                "nameserver 10.99.0.53\nsearch US-West-1.Compute.Internal. corp.example\n",
            ),
        ]);

        assert_eq!(
            search_domains_of(&search_narrowed_to_sole_aws_vpc_zone(groups)),
            vec!["us-west-1.compute.internal", "US-West-1.Compute.Internal."],
            "both spellings name the one zone, so it is forwarded in each \
             source's own spelling and corp.example is not"
        );
    }

    /// A host that is not on EC2 reaches the probe path whenever no source
    /// names an IPv6 server, and `for_launch` cannot know the probe will fail
    /// there. It carries no AWS VPC zone, so nothing is forwarded. Nothing is
    /// lost: `detect_ipv6_dns` returns None on that host, routed gives the
    /// guest no IPv6 resolver, and the IPv4 servers the guest is handed instead
    /// are unreachable from a routed guest, so no suffix had a resolver to be
    /// completed against. Keeping the list whole would send those private
    /// suffixes to the AWS resolver on every host where the probe DOES answer.
    #[test]
    fn a_host_with_no_aws_vpc_zone_forwards_no_suffix_on_the_probe_path() {
        let groups = resolver_groups(&[readable(
            ETC_RESOLV_CONF,
            "nameserver 10.99.0.53\nsearch corp.example lab.example\n",
        )]);

        let narrowed = search_narrowed_to_sole_aws_vpc_zone(groups);
        assert!(
            search_domains_of(&narrowed).is_empty(),
            "no zone here is one the probed resolver serves"
        );
        assert_eq!(
            narrowed
                .iter()
                .flat_map(|group| group.servers.iter())
                .cloned()
                .collect::<Vec<_>>(),
            vec!["10.99.0.53"],
            "the servers are untouched even when every suffix goes"
        );
    }

    /// The zone SHAPE, which is the `.compute.internal` suffix AND a region in
    /// front of it, not a substring match and not a blanket `.internal`
    /// allowance. us-east-1's VPC zone is `ec2.internal` and every other
    /// region's is `<region>.compute.internal`; measured on an EC2 instance,
    /// the VPC resolver NXDOMAINs `foo.compute.internal` and `host1.lab.internal`
    /// alike, so the suffix on its own is not the test. Each domain is fed by
    /// itself, so each is the sole candidate in its own case; a host naming
    /// SEVERAL is what
    /// `two_aws_vpc_zones_are_ambiguous_so_the_probe_narrowing_forwards_neither`
    /// pins.
    #[test]
    fn only_an_aws_internal_zone_survives_the_probe_narrowing() {
        let kept = [
            "ec2.internal",
            "us-west-1.compute.internal",
            "eu-west-1.compute.internal",
            // One pattern covers every partition. `eusc-de-east-1` is the AWS
            // European Sovereign Cloud region whose shape the first version of
            // that pattern did not have (#891);
            // `every_published_aws_region_label_is_recognized`
            // runs AWS's whole published list through this path.
            "ap-southeast-4.compute.internal",
            "il-central-1.compute.internal",
            "us-gov-west-1.compute.internal",
            "cn-northwest-1.compute.internal",
            "us-isob-east-1.compute.internal",
            "eusc-de-east-1.compute.internal",
            "US-West-1.Compute.Internal",
            "us-west-1.compute.internal.",
        ];
        let dropped = [
            "corp.example",
            "lab.internal",
            "internal",
            "compute.internal",
            "notec2.internal",
            "ec2.internal.example.com",
            "compute.internal.example.com",
            // Right suffix, and the label in front of it is not a region: a
            // custom DHCP domain-name, a truncated or malformed region, an
            // availability zone, and a hostname that already carries the zone.
            "foo.compute.internal",
            "dev.compute.internal",
            "us.compute.internal",
            "us-1.compute.internal",
            "us-w-1.compute.internal",
            "u-west-1.compute.internal",
            "us-west.compute.internal",
            "us-west-.compute.internal",
            "us-west-1a.compute.internal",
            "ip-10-0-1-49.us-west-1.compute.internal",
            "us-east-1.compute.internal",
        ];
        // Shaped like a region and not one. `is_aws_region_label` admits these
        // deliberately and says why: it no longer claims to know how long a
        // region's words are, because claiming that is what rejected
        // `eusc-de-east-1`. Asserted rather than left unstated so that
        // tightening the pattern again is a deliberate act with a test to
        // change, not a silent one.
        let admitted_false_accepts = ["usa-west-1.compute.internal", "my-test-1.compute.internal"];

        for domain in kept {
            let narrowed = search_narrowed_to_sole_aws_vpc_zone(vec![ResolverGroup {
                servers: vec!["10.0.0.2".to_string()],
                search_domains: vec![domain.to_string()],
            }]);
            assert_eq!(
                search_domains_of(&narrowed),
                vec![domain],
                "{domain} is an AWS VPC internal zone"
            );
        }

        for domain in dropped {
            let narrowed = search_narrowed_to_sole_aws_vpc_zone(vec![ResolverGroup {
                servers: vec!["10.0.0.2".to_string()],
                search_domains: vec![domain.to_string()],
            }]);
            assert!(
                search_domains_of(&narrowed).is_empty(),
                "{domain} is not a zone the probed resolver serves, and asking \
                 it discloses the private label to the public DNS"
            );
        }

        for domain in admitted_false_accepts {
            let narrowed = search_narrowed_to_sole_aws_vpc_zone(vec![ResolverGroup {
                servers: vec!["10.0.0.2".to_string()],
                search_domains: vec![domain.to_string()],
            }]);
            assert_eq!(
                search_domains_of(&narrowed),
                vec![domain],
                "{domain} is the shape of a region, so it is forwarded. The \
                 probed resolver NXDOMAINs it without recursing it into the \
                 public DNS, which is the bound that makes the false accept \
                 cheaper than rejecting a region AWS has opened"
            );
        }
    }

    /// Every region code AWS publishes, run through the region-label test,
    /// plus the non-regions the zone test exists to reject.
    ///
    /// The list is the union of `partitions[].regions` in botocore's
    /// `endpoints.json` as shipped with aws-cli 2.32.28, which is AWS's own
    /// machine-readable region list. All 46 codes, all eight partitions.
    /// Regenerate it with:
    ///
    /// ```text
    /// python3 -c 'import json,sys; d=json.load(open(sys.argv[1]));
    ///   print(sorted({r for p in d["partitions"] for r in p["regions"]}))' \
    ///   $(dirname $(readlink -f $(command -v aws)))/awscli/botocore/data/endpoints.json
    /// ```
    ///
    /// `eusc-de-east-1` is why this test exists. The first version of
    /// [`is_aws_region_label`] required a two-letter first word and three-letter
    /// words after it, and asserted in its own doc comment that "every region in
    /// every partition has that form". The AWS European Sovereign Cloud region
    /// in Brandenburg has neither: `eusc` is four letters and `de` is two. An
    /// instance there carries `eusc-de-east-1.compute.internal` as its
    /// DHCP domain-name, the shape test called it a custom domain-name, and the
    /// probe path dropped the host's own zone.
    ///
    /// AWS documents the internal zone with no partition in it. The Route 53
    /// autodefined-rules page lists `{Region-name}.compute.internal, for
    /// example, eu-west-1.compute.internal. The us-east-1 Region doesn't use
    /// this domain name.`, and lists the PUBLIC zone one line below as
    /// `{Region-name}.compute.{amazon-domain-name}, for example,
    /// eu-west-1.compute.amazonaws.com or cn-north-1.compute.amazonaws.com.cn`.
    /// The partition domain is a placeholder in the public zone and absent from
    /// the internal one, and us-east-1 is the only carve-out either page names
    /// (EC2's hostname-types page says the same: "Format for an instance in any
    /// other AWS Region: {private-ipv4-address.region}.compute.internal").
    /// So `<region>.compute.internal` holds in aws-eusc as it does in aws-cn.
    ///
    /// The loop asserts that each published region code is RECOGNISED as one,
    /// which is what [`is_aws_region_label`] decides. It is not a claim that
    /// AWS serves every one of those zones: the same Route 53 page says
    /// us-east-1 does not use `<region>.compute.internal`, and the zone it does
    /// use is asserted separately below.
    ///
    /// The negatives are half the test. Without them a predicate that returned
    /// `true` for everything would pass, which is the shape of a check that
    /// cannot fail.
    #[test]
    fn every_published_aws_region_label_is_recognized() {
        let regions = [
            "af-south-1",
            "ap-east-1",
            "ap-east-2",
            "ap-northeast-1",
            "ap-northeast-2",
            "ap-northeast-3",
            "ap-south-1",
            "ap-south-2",
            "ap-southeast-1",
            "ap-southeast-2",
            "ap-southeast-3",
            "ap-southeast-4",
            "ap-southeast-5",
            "ap-southeast-6",
            "ap-southeast-7",
            "ca-central-1",
            "ca-west-1",
            "cn-north-1",
            "cn-northwest-1",
            "eu-central-1",
            "eu-central-2",
            "eu-isoe-west-1",
            "eu-north-1",
            "eu-south-1",
            "eu-south-2",
            "eu-west-1",
            "eu-west-2",
            "eu-west-3",
            "eusc-de-east-1",
            "il-central-1",
            "me-central-1",
            "me-south-1",
            "mx-central-1",
            "sa-east-1",
            "us-east-1",
            "us-east-2",
            "us-gov-east-1",
            "us-gov-west-1",
            "us-iso-east-1",
            "us-iso-west-1",
            "us-isob-east-1",
            "us-isob-west-1",
            "us-isof-east-1",
            "us-isof-south-1",
            "us-west-1",
            "us-west-2",
        ];

        for region in regions {
            assert!(
                is_aws_region_label(region),
                "{region} is a region AWS publishes today, so it has to read \
                 as a region and not as a custom domain-name"
            );
        }

        // us-east-1's own zone, which is the one carve-out both AWS pages name.
        let narrowed = search_narrowed_to_sole_aws_vpc_zone(vec![ResolverGroup {
            servers: vec!["10.0.0.2".to_string()],
            search_domains: vec!["ec2.internal".to_string()],
        }]);
        assert_eq!(search_domains_of(&narrowed), vec!["ec2.internal"]);

        for domain in [
            "corp.example",
            "lab.internal",
            "internal",
            "compute.internal",
            "foo.compute.internal",
            "us.compute.internal",
            "us-1.compute.internal",
            "us-west.compute.internal",
            "us-west-.compute.internal",
            "us-west-1a.compute.internal",
            "ip-10-0-1-49.us-west-1.compute.internal",
        ] {
            let narrowed = search_narrowed_to_sole_aws_vpc_zone(vec![ResolverGroup {
                servers: vec!["10.0.0.2".to_string()],
                search_domains: vec![domain.to_string()],
            }]);
            assert!(
                search_domains_of(&narrowed).is_empty(),
                "{domain} names no region, so it is not a VPC zone and asking \
                 the probed resolver for it resolves nothing"
            );
        }
    }

    /// The other half of #875: the search list and the nameserver list have to
    /// describe the same resolvers. A short name is completed against a search
    /// domain and then asked of a server, so a domain from a source whose
    /// servers the guest never receives cannot resolve.
    #[test]
    fn search_domains_come_only_from_sources_that_supplied_a_server() {
        let sources = [
            readable("/run/systemd/resolve/resolv.conf", "search .\n"),
            readable(
                "/etc/resolv.conf",
                "nameserver 10.1.0.2\nsearch corp.example\n",
            ),
        ];

        assert_eq!(
            nameservers_from_sources(&sources).unwrap(),
            vec!["10.1.0.2"],
            "the usable server comes from /etc/resolv.conf"
        );
        assert_eq!(
            search_domains_of(&resolver_groups(&sources)),
            vec!["corp.example"],
            "the stub-only run file supplied no resolver, so it does not decide what the guest searches"
        );
    }

    /// Unioned for the same reason the nameservers are: the guest is handed
    /// every contributing source's servers, so it must be able to complete
    /// short names for each of them.
    #[test]
    fn search_domains_union_every_contributing_source() {
        let sources = [
            readable(
                "/run/systemd/resolve/resolv.conf",
                "nameserver 10.1.0.2\nsearch corp.example internal\n",
            ),
            readable(
                "/etc/resolv.conf",
                "nameserver 8.8.8.8\nsearch internal other.example\n",
            ),
        ];

        assert_eq!(
            search_domains_of(&resolver_groups(&sources)),
            vec!["corp.example", "internal", "other.example"],
            "the run file's domains come first and a domain named twice appears once"
        );
    }

    /// The L1 state `test_nested_run_fcvm_inside_vm` gates on, evaluated with
    /// the same predicate the inner fcvm uses. A guest whose fc-agent wrote a
    /// real nameserver into /etc/resolv.conf must pass the gate even though
    /// its run file holds no server.
    #[test]
    fn nested_guest_dns_gate_matches_inner_fcvm() {
        let healthy = [
            readable("/run/systemd/resolve/resolv.conf", "search .\n"),
            readable("/etc/resolv.conf", "nameserver 10.1.0.2\n"),
        ];
        assert_eq!(
            nameservers_from_sources(&healthy).unwrap(),
            vec!["10.1.0.2"],
            "an L1 that fc-agent configured must not trip the gate"
        );

        let broken = [
            readable("/run/systemd/resolve/resolv.conf", "search .\n"),
            readable(
                "/etc/resolv.conf",
                "# Placeholder - fc-agent configures DNS at boot from kernel cmdline\n\
                 nameserver 127.0.0.53\n",
            ),
        ];
        assert!(
            nameservers_from_sources(&broken).is_err(),
            "an L1 still carrying the rootfs placeholder must trip the gate"
        );
    }

    /// #875 was one reader deciding for every source. A new reader that names
    /// a resolv.conf path itself can reintroduce that, silently, so every read
    /// in this crate goes through [`RESOLV_CONF_SOURCES`] and this module is
    /// the only place the paths appear.
    ///
    /// The integration tests are scanned too. A test that picks its own source
    /// reaches a different verdict than the launch path it is checking, so in
    /// the exact #875 state it skips instead of exercising the fix, and the
    /// end-to-end regression it exists to catch stays invisible.
    #[test]
    fn only_the_shared_source_list_names_a_resolv_conf_path() {
        fn walk(dir: &std::path::Path, offenders: &mut Vec<String>) {
            let entries = std::fs::read_dir(dir)
                .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()));
            for entry in entries {
                let path = entry.expect("readable directory entry").path();
                if path.is_dir() {
                    walk(&path, offenders);
                    continue;
                }
                if path.extension().is_none_or(|ext| ext != "rs")
                    || path.ends_with("network/mod.rs")
                {
                    continue;
                }
                let body = std::fs::read_to_string(&path)
                    .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
                // Comment lines are prose, not readers.
                let code = body
                    .lines()
                    .filter(|line| !line.trim_start().starts_with("//"))
                    .collect::<Vec<_>>()
                    .join("\n");
                for literal in ["/run/systemd/resolve/resolv.conf", "\"/etc/resolv.conf\""] {
                    if code.contains(literal) {
                        offenders.push(format!("{} names {literal}", path.display()));
                    }
                }
            }
        }

        let mut offenders = Vec::new();
        let crate_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        for dir in ["src", "tests"] {
            walk(&crate_root.join(dir), &mut offenders);
        }

        assert!(
            offenders.is_empty(),
            "these readers can decide from one file while another holds the usable \
             server; read RESOLV_CONF_SOURCES instead: {offenders:?}"
        );
    }

    /// A launch reads the resolv.conf sources ONCE, and every later decision
    /// uses that snapshot. A second read elsewhere in the launch path lets the
    /// guest receive one resolver while its search domains and its snapshot
    /// key describe another, which a cached snapshot then preserves (#885).
    ///
    /// Routed mode did exactly that: it re-read the sources at setup time to
    /// pick its IPv6 resolver. The selection is now threaded in from
    /// GuestBootInputs, and this keeps the single read single.
    #[test]
    fn only_guest_boot_inputs_reads_the_resolv_conf_sources() {
        fn walk(dir: &std::path::Path, hits: &mut Vec<String>) {
            for entry in std::fs::read_dir(dir)
                .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()))
            {
                let path = entry.expect("readable directory entry").path();
                if path.is_dir() {
                    walk(&path, hits);
                    continue;
                }
                // This module defines the reader and exercises it in its own
                // tests, so it is not a launch-path caller.
                if path.extension().is_none_or(|ext| ext != "rs")
                    || path.ends_with("network/mod.rs")
                {
                    continue;
                }
                let body = std::fs::read_to_string(&path)
                    .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
                for line in body.lines().filter(|l| !l.trim_start().starts_with("//")) {
                    if line.contains("ResolvSource::read") {
                        hits.push(format!("{}: {}", path.display(), line.trim()));
                    }
                }
            }
        }

        let mut hits = Vec::new();
        walk(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut hits,
        );

        assert_eq!(
            hits.len(),
            1,
            "the launch must read the sources exactly once, in \
             GuestBootInputs::resolve, and thread the result to everything that \
             needs it; found: {hits:?}"
        );
        assert!(
            hits[0].contains("commands/podman/vm_config.rs"),
            "the single read belongs to GuestBootInputs::resolve: {hits:?}"
        );
    }

    /// The read path against this host's real files. Whether it finds a server
    /// depends on the host, but anything it returns must be usable.
    #[test]
    fn reading_this_host_returns_only_usable_servers() {
        let sources = RESOLV_CONF_SOURCES.map(ResolvSource::read);
        let result = nameservers_from_sources(&sources);
        println!("host DNS servers: {result:?}");
        if let Ok(servers) = result {
            assert!(!servers.is_empty());
            for server in &servers {
                assert!(
                    is_usable_nameserver(server),
                    "{server} is not a server a VM can reach"
                );
            }
        }
    }
}

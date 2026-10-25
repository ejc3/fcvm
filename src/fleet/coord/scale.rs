//! The GPU hosts' auto scaling groups. The coordinator owns their desired capacity: zero
//! while the fleet is asleep, the configured target otherwise; while it wakes it polls
//! their instances for the page's progress.
//!
//! A spot group can have an on-demand group in its region (`--on-demand-group`). Auto
//! Scaling never moves a spot group's capacity to on demand on its own (a mixed instances
//! policy keeps its configured split and keeps retrying spot), so the coordinator does it:
//! when a wake leaves the spot group and its on-demand group together short of the awake
//! capacity for `spot_wait`, the on-demand group takes the shortfall and the spot group is
//! held at the instances it has, so a spot launch that succeeds later cannot double the
//! hosts. The on-demand group is never raised otherwise, and goes to 0 with the spot group
//! when the fleet sleeps; the next wake tries spot first again.
//!
//! The wait is a backstop. Auto Scaling records a refused spot launch as a failed scaling
//! activity (`UnfulfillableCapacity`, `MaxSpotInstanceCountExceeded`) about 10 s after the
//! desired capacity is raised, and keeps retrying spot without telling anyone, so a short
//! spot group with a failed launch since the wake began falls back at the next poll.
//!
//! The groups are watched (polled, their instances reported, the fallback decided) while
//! the fleet wakes and after, until they hold the awake capacity in service: the EC2 half
//! can make the fleet awake before the GPU hosts are up (53 s into the staging wake of
//! 2026-10-25), and a fallback decided only while waking then never came. Once in service
//! the coordinator logs which path the wake's hosts took (spot, on demand or both).

use super::aws::{xml_tag as tag, CredentialChain};
use super::sigv4::{authorization, uri_encode};
use super::Handle;
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use fleet_core::event::ScaleInstance;
use fleet_core::Event;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::Instant;

/// An auto scaling group, `<region>:<name>` on the command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    pub region: String,
    pub name: String,
}

impl std::str::FromStr for Group {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        let (region, name) = s
            .split_once(':')
            .filter(|(r, n)| !r.is_empty() && !n.is_empty())
            .ok_or_else(|| format!("{s:?} is not <region>:<group name>"))?;
        Ok(Group {
            region: region.into(),
            name: name.into(),
        })
    }
}

#[async_trait]
pub trait ScaleDriver: Send + Sync {
    async fn set_desired(&self, group: &Group, desired: u32) -> Result<()>;
    async fn instances(&self, group: &Group) -> Result<Vec<ScaleInstance>>;
    /// The group's desired capacity now.
    async fn desired(&self, group: &Group) -> Result<u32>;
    /// The status messages of the group's failed scaling activities that started at or
    /// after `since` (Unix milliseconds), newest first.
    async fn failed_launches(&self, group: &Group, since: u64) -> Result<Vec<String>>;
}

/// The groups, their awake capacity, and how often to poll while waking.
pub struct PowerConfig {
    pub driver: Arc<dyn ScaleDriver>,
    pub groups: Vec<Group>,
    pub awake_capacity: u32,
    pub poll: Duration,
    /// The on-demand fallback of each of `groups`, by index (`pair_on_demand`); a missing
    /// or `None` entry has none.
    pub on_demand: Vec<Option<Group>>,
    /// How long a waking spot group may stay short before its on-demand group takes over,
    /// when Auto Scaling has not reported a failed spot launch.
    pub spot_wait: Duration,
}

/// Each spot group's on-demand group: the one in its region. An on-demand group with no
/// spot group in its region, or two in one region, is an error.
pub fn pair_on_demand(spot: &[Group], on_demand: &[Group]) -> Result<Vec<Option<Group>>> {
    let mut by_region: HashMap<&str, &Group> = HashMap::new();
    for g in on_demand {
        if by_region.insert(g.region.as_str(), g).is_some() {
            bail!("two --on-demand-group in {}", g.region);
        }
        let n = spot.iter().filter(|s| s.region == g.region).count();
        if n != 1 {
            bail!(
                "--on-demand-group {}:{} needs exactly one --scale-group in {}, found {n}",
                g.region,
                g.name,
                g.region
            );
        }
        if spot.iter().any(|s| s == g) {
            bail!(
                "{}:{} is both a scale group and an on-demand group",
                g.region,
                g.name
            );
        }
    }
    Ok(spot
        .iter()
        .map(|s| by_region.get(s.region.as_str()).map(|g| (*g).clone()))
        .collect())
}

/// Whether an instance counts toward a group's capacity: anything not on its way out.
pub fn is_live(i: &ScaleInstance) -> bool {
    !i.lifecycle.starts_with("Terminat") && !i.lifecycle.starts_with("Detach")
}

/// One spot group's fallback to its on-demand group.
#[derive(Debug, Default)]
pub struct Fallback {
    short_since: Option<Instant>,
    /// The (spot, on-demand) split in force: decided by a fallback, or found at a wake.
    target: Option<(u32, u32)>,
    /// Whether both capacities of `target` were set.
    applied: bool,
}

impl Fallback {
    /// After a poll of a waking fleet: the desired capacities (spot, on-demand) to set.
    /// A split not yet fully set is asked for again whatever the instances are, so a write
    /// that failed half way is retried until both groups hold it. Otherwise, once the two
    /// groups' live instances have stayed below `want` for `wait`, or at once when the spot
    /// group reported a failed launch (`spot_failed`), the spot group is held at what it
    /// has and the on-demand group takes the rest, so the two never ask for more than
    /// `want` together. `None` while they are not short, before `wait` has passed without
    /// a failed spot launch, or when that split is already in force.
    pub fn observe(
        &mut self,
        now: Instant,
        want: u32,
        spot_live: u32,
        on_demand_live: u32,
        wait: Duration,
        spot_failed: bool,
    ) -> Option<(u32, u32)> {
        if let Some(t) = self.pending() {
            return Some(t);
        }
        if spot_live.saturating_add(on_demand_live) >= want {
            self.short_since = None;
            return None;
        }
        let since = *self.short_since.get_or_insert(now);
        if now.duration_since(since) < wait && !spot_failed {
            return None;
        }
        let target = (spot_live, want - spot_live);
        if self.target == Some(target) {
            return None;
        }
        self.target = Some(target);
        self.applied = false;
        Some(target)
    }

    /// The split to set, while it is not fully set.
    pub fn pending(&self) -> Option<(u32, u32)> {
        self.target.filter(|_| !self.applied)
    }

    /// Both capacities of `target` were set.
    pub fn applied(&mut self, target: (u32, u32)) {
        if self.target == Some(target) {
            self.applied = true;
        }
    }

    /// At a wake, the on-demand group already holds `on_demand` (a fallback made before a
    /// restart): keep it, up to `want`, and hold the spot group at the rest.
    pub fn adopt(&mut self, want: u32, on_demand: u32) {
        let od = on_demand.min(want);
        self.target = Some((want - od, od));
        self.applied = false;
    }
}

/// Follow the fleet's power state: set every group's desired capacity on each change
/// (0 asleep, `awake_capacity` otherwise; on-demand groups 0 asleep and untouched
/// otherwise), watch the groups while they come up, report their instances, and fall back
/// to on-demand groups as the module says.
pub async fn run(cfg: PowerConfig, mut power: watch::Receiver<String>, handle: Handle) {
    let on_demand = |i: usize| cfg.on_demand.get(i).cloned().flatten();
    let mut set: Option<u32> = None;
    let mut fallbacks: Vec<Fallback> = cfg.groups.iter().map(|_| Fallback::default()).collect();
    // Per group: whether it is still coming up (watched until its live instances, with its
    // on-demand group's, hold the awake capacity in service), and since when (Unix ms) a
    // failed launch counts toward its fallback.
    let mut watch: Vec<Watch> = cfg.groups.iter().map(|_| Watch::default()).collect();
    // A degraded `scale` was reported and not cleared yet.
    let mut reported = false;
    loop {
        let state = power.borrow_and_update().clone();
        let want = if state == "asleep" {
            0
        } else {
            cfg.awake_capacity
        };
        let mut newly_set = false;
        if set != Some(want) {
            // A transition that does not finish leaves no target behind, so a wake that
            // comes before a half-done sleep is retried writes every group again.
            set = None;
            let mut ok = true;
            if want == 0 {
                fallbacks.iter_mut().for_each(|f| *f = Fallback::default());
                watch.iter_mut().for_each(|w| *w = Watch::default());
            } else {
                let since = unix_ms().saturating_sub(CLOCK_SLACK_MS);
                watch.iter_mut().for_each(|w| {
                    if !w.up {
                        w.coming_up = true;
                        w.since.get_or_insert(since);
                    }
                });
            }
            for (i, g) in cfg.groups.iter().enumerate() {
                let od = on_demand(i);
                let result = match (&od, want) {
                    // Asleep, the on-demand group goes to 0 with its spot group.
                    (Some(od), 0) => async {
                        cfg.driver.set_desired(g, 0).await?;
                        cfg.driver.set_desired(od, 0).await
                    }
                    .await,
                    // Awake, read both groups first: an on-demand group that already holds
                    // hosts (a fallback made before a restart) keeps them, and the spot group
                    // is held at the rest by the fallback write below.
                    (Some(od), _) => async {
                        let od_now = cfg.driver.desired(od).await?;
                        let spot_now = cfg.driver.desired(g).await?;
                        if od_now > 0 {
                            fallbacks[i].adopt(want, od_now);
                            tracing::warn!(spot_group = %g.name, on_demand_group = %od.name, on_demand = od_now, "the on-demand group holds hosts; keeping that fallback");
                            Ok(())
                        } else if spot_now != want {
                            cfg.driver.set_desired(g, want).await
                        } else {
                            Ok(())
                        }
                    }
                    .await,
                    (None, _) => cfg.driver.set_desired(g, want).await,
                };
                match result {
                    Ok(()) => {
                        tracing::info!(group = %g.name, region = %g.region, power_target = want, "group set for the power state")
                    }
                    Err(e) => {
                        ok = false;
                        reported = true;
                        degraded(&handle, g, &e).await;
                    }
                }
            }
            if ok {
                set = Some(want);
                newly_set = true;
            }
        }
        let mut watching = false;
        if state != "asleep" {
            let mut all = Vec::new();
            let mut polled = false;
            for (i, g) in cfg.groups.iter().enumerate() {
                let od = on_demand(i);
                let mut target = fallbacks[i].pending();
                if state == "waking" || watch[i].coming_up {
                    polled = true;
                    let spot = match cfg.driver.instances(g).await {
                        Ok(i) => Some(i),
                        Err(e) => {
                            tracing::warn!(group = %g.name, error = %format!("{e:#}"), "describing the group failed");
                            None
                        }
                    };
                    let od_instances = match &od {
                        Some(od) => match cfg.driver.instances(od).await {
                            Ok(i) => Some(i),
                            Err(e) => {
                                tracing::warn!(group = %od.name, error = %format!("{e:#}"), "describing the group failed");
                                None
                            }
                        },
                        None => Some(Vec::new()),
                    };
                    // A group that cannot be described is not known to be short: no new
                    // fallback (a split already decided is still written).
                    if let (Some(s), Some(o)) = (&spot, &od_instances) {
                        let live =
                            |v: &[ScaleInstance]| v.iter().filter(|i| is_live(i)).count() as u32;
                        let serving = |v: &[ScaleInstance]| {
                            v.iter().filter(|i| i.lifecycle == "InService").count() as u32
                        };
                        let (s_live, o_live) = (live(s), live(o));
                        let (s_in, o_in) = (serving(s), serving(o));
                        if s_in + o_in >= cfg.awake_capacity {
                            if watch[i].coming_up {
                                tracing::info!(
                                    spot_group = %g.name, region = %g.region,
                                    path = path_name(s_in, o_in),
                                    spot = s_in, on_demand = o_in,
                                    since_wake_ms = watch[i].since.map(|t| unix_ms().saturating_sub(t + CLOCK_SLACK_MS)),
                                    "wake: host capacity in service"
                                );
                            }
                            watch[i].coming_up = false;
                            watch[i].up = true;
                        } else {
                            watch[i].up = false;
                        }
                        if let Some(od) = &od {
                            let short = s_live + o_live < cfg.awake_capacity;
                            let spot_failed = if short && fallbacks[i].target.is_none() {
                                watch[i].spot_failure(&*cfg.driver, g).await
                            } else {
                                None
                            };
                            let failed = spot_failed.is_some();
                            let decided = fallbacks[i].target;
                            target = fallbacks[i].observe(
                                Instant::now(),
                                cfg.awake_capacity,
                                s_live,
                                o_live,
                                cfg.spot_wait,
                                failed,
                            );
                            if let Some((spot_to, od_to)) = target {
                                if fallbacks[i].target != decided {
                                    match &spot_failed {
                                        Some(why) => tracing::warn!(
                                            spot_group = %g.name, on_demand_group = %od.name,
                                            spot = spot_to, on_demand = od_to, reason = %why,
                                            "spot launch failed; falling back to on-demand now"
                                        ),
                                        None => tracing::warn!(
                                            spot_group = %g.name, on_demand_group = %od.name,
                                            spot = spot_to, on_demand = od_to,
                                            spot_wait_s = cfg.spot_wait.as_secs(),
                                            "spot group still short after the spot wait; falling back to on-demand"
                                        ),
                                    }
                                }
                            }
                        }
                    }
                    all.extend(spot.unwrap_or_default());
                    all.extend(od_instances.unwrap_or_default());
                }
                watching |= watch[i].coming_up;
                let (Some(od), Some((spot_to, od_to))) = (od, target) else {
                    continue;
                };
                tracing::warn!(
                    spot_group = %g.name, on_demand_group = %od.name, region = %g.region,
                    spot = spot_to, on_demand = od_to, want = cfg.awake_capacity,
                    "setting the on-demand fallback split"
                );
                // The on-demand group first; the split stays pending until both are set.
                let mut ok = true;
                for (grp, to) in [(&od, od_to), (g, spot_to)] {
                    if let Err(e) = cfg.driver.set_desired(grp, to).await {
                        ok = false;
                        reported = true;
                        degraded(&handle, grp, &e).await;
                        break;
                    }
                    tracing::info!(group = %grp.name, region = %grp.region, desired = to, "desired capacity set");
                }
                if ok {
                    fallbacks[i].applied((spot_to, od_to));
                }
            }
            if polled {
                handle
                    .send(Event::ScaleInstances {
                        at: 0,
                        instances: all,
                    })
                    .await;
            }
        }
        let pending = fallbacks.iter().any(|f| f.pending().is_some());
        // Clear `scale` once every write it reported has gone through.
        if set == Some(want) && !pending && (newly_set || reported) {
            reported = false;
            healthy(&handle).await;
        }
        // A failed set is retried on the next poll.
        let wait = if state == "waking" || watching || set != Some(want) || pending {
            cfg.poll
        } else {
            Duration::from_secs(3600)
        };
        tokio::select! {
            r = power.changed() => if r.is_err() { return },
            _ = tokio::time::sleep(wait) => {}
        }
    }
}

/// How far before the coordinator's own clock a failed activity still counts as this
/// wake's: Auto Scaling stamps it after the desired capacity was raised, so this only
/// absorbs the difference between the two clocks.
const CLOCK_SLACK_MS: u64 = 2000;

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Which groups a wake's in-service hosts came from.
pub fn path_name(spot: u32, on_demand: u32) -> &'static str {
    match (spot > 0, on_demand > 0) {
        (true, false) => "spot",
        (false, true) => "on-demand",
        (true, true) => "spot and on-demand",
        (false, false) => "none",
    }
}

/// One spot group's progress through a wake.
#[derive(Debug, Default)]
struct Watch {
    /// Raised for the wake and not yet holding the awake capacity in service.
    coming_up: bool,
    /// Held the awake capacity in service at the last poll.
    up: bool,
    /// Failed activities from this time on (Unix ms) count toward the fallback.
    since: Option<u64>,
    /// Reading the activities failed once and was logged (a role without
    /// DescribeScalingActivities); the spot wait still applies.
    warned: bool,
}

impl Watch {
    /// The newest failed launch of the spot group since the wake began, if any.
    async fn spot_failure(&mut self, driver: &dyn ScaleDriver, g: &Group) -> Option<String> {
        let since = self.since?;
        match driver.failed_launches(g, since).await {
            Ok(f) => f.into_iter().next(),
            Err(e) => {
                if !self.warned {
                    self.warned = true;
                    tracing::warn!(group = %g.name, error = %format!("{e:#}"), "reading the group's scaling activities failed; falling back only after the spot wait");
                }
                None
            }
        }
    }
}

async fn degraded(handle: &Handle, g: &Group, e: &anyhow::Error) {
    tracing::warn!(group = %g.name, region = %g.region, error = %format!("{e:#}"), "setting desired capacity failed");
    handle
        .send(Event::Degraded {
            at: 0,
            component: "scale".into(),
            reason: Some(format!("{e:#}")),
        })
        .await;
}

async fn healthy(handle: &Handle) {
    handle
        .send(Event::Degraded {
            at: 0,
            component: "scale".into(),
            reason: None,
        })
        .await;
}

/// Amazon EC2 Auto Scaling (query API, SigV4).
pub struct AwsAutoScaling {
    creds: CredentialChain,
    http: reqwest::Client,
}

impl AwsAutoScaling {
    pub fn new() -> Result<Self> {
        Ok(AwsAutoScaling {
            creds: CredentialChain::new()?,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .build()?,
        })
    }

    async fn call(&self, region: &str, params: &[(&str, String)]) -> Result<String> {
        let host = format!("autoscaling.{region}.amazonaws.com");
        let mut body = String::from("Version=2011-01-01");
        for (k, v) in params {
            body.push('&');
            body.push_str(&uri_encode(k));
            body.push('=');
            body.push_str(&uri_encode(v));
        }
        let creds = self.creds.get().await?;
        let amz_date = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
        let mut headers = vec![
            ("host".to_string(), host.clone()),
            ("x-amz-date".to_string(), amz_date.clone()),
            (
                "content-type".to_string(),
                "application/x-www-form-urlencoded; charset=utf-8".to_string(),
            ),
        ];
        if let Some(t) = &creds.session_token {
            headers.push(("x-amz-security-token".to_string(), t.clone()));
        }
        let auth = authorization(
            &creds,
            "POST",
            "/",
            "",
            &headers,
            body.as_bytes(),
            region,
            "autoscaling",
            &amz_date,
        );
        let mut req = self
            .http
            .post(format!("https://{host}/"))
            .header("authorization", auth);
        for (k, v) in &headers {
            if k != "host" {
                req = req.header(k, v);
            }
        }
        let r = req
            .body(body)
            .send()
            .await
            .context("calling Auto Scaling")?;
        let status = r.status();
        let text = r.text().await.context("reading Auto Scaling's answer")?;
        if !status.is_success() {
            bail!(
                "Auto Scaling answered {status}: {}",
                tag(&text, "Message").unwrap_or_default()
            );
        }
        Ok(text)
    }
}

#[async_trait]
impl ScaleDriver for AwsAutoScaling {
    async fn set_desired(&self, group: &Group, desired: u32) -> Result<()> {
        self.call(
            &group.region,
            &[
                ("Action", "SetDesiredCapacity".into()),
                ("AutoScalingGroupName", group.name.clone()),
                ("DesiredCapacity", desired.to_string()),
                ("HonorCooldown", "false".into()),
            ],
        )
        .await
        .map(|_| ())
    }

    async fn instances(&self, group: &Group) -> Result<Vec<ScaleInstance>> {
        let xml = self
            .call(
                &group.region,
                &[
                    ("Action", "DescribeAutoScalingGroups".into()),
                    ("AutoScalingGroupNames.member.1", group.name.clone()),
                ],
            )
            .await?;
        Ok(parse_instances(&xml))
    }

    async fn desired(&self, group: &Group) -> Result<u32> {
        let xml = self
            .call(
                &group.region,
                &[
                    ("Action", "DescribeAutoScalingGroups".into()),
                    ("AutoScalingGroupNames.member.1", group.name.clone()),
                ],
            )
            .await?;
        parse_desired(&xml).with_context(|| format!("no desired capacity for group {}", group.name))
    }

    async fn failed_launches(&self, group: &Group, since: u64) -> Result<Vec<String>> {
        let xml = self
            .call(
                &group.region,
                &[
                    ("Action", "DescribeScalingActivities".into()),
                    ("AutoScalingGroupName", group.name.clone()),
                    ("MaxRecords", "20".into()),
                ],
            )
            .await?;
        Ok(parse_failed_activities(&xml, since))
    }
}

/// The failed activities of a DescribeScalingActivities answer that started at or after
/// `since` (Unix ms): each one's `StatusMessage` (its `Description` when it has none), in
/// the answer's order (newest first). An activity whose `StartTime` does not parse is left
/// out: it cannot be placed in this wake.
pub fn parse_failed_activities(xml: &str, since: u64) -> Vec<String> {
    let Some(start) = xml.find("<Activities>") else {
        return Vec::new();
    };
    let body = &xml[start..];
    let body = &body[..body.find("</Activities>").unwrap_or(body.len())];
    body.split("<member>")
        .skip(1)
        .filter(|m| tag(m, "StatusCode").as_deref() == Some("Failed"))
        .filter(|m| {
            tag(m, "StartTime")
                .and_then(|t| chrono::DateTime::parse_from_rfc3339(&t).ok())
                .is_some_and(|t| t.timestamp_millis() >= since as i64)
        })
        .filter_map(|m| tag(m, "StatusMessage").or_else(|| tag(m, "Description")))
        .collect()
}

/// The desired capacity of a DescribeAutoScalingGroups answer naming one group.
pub fn parse_desired(xml: &str) -> Option<u32> {
    tag(xml, "AutoScalingGroupName")?;
    tag(xml, "DesiredCapacity")?.parse().ok()
}

/// The instances of a DescribeAutoScalingGroups answer: each `<member>` of `<Instances>`
/// with its `InstanceId` and `LifecycleState`.
pub fn parse_instances(xml: &str) -> Vec<ScaleInstance> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(i) = rest.find("<Instances>") {
        let after = &rest[i + "<Instances>".len()..];
        let Some(end) = after.find("</Instances>") else {
            break;
        };
        for m in after[..end].split("<member>").skip(1) {
            if let (Some(id), Some(lifecycle)) = (tag(m, "InstanceId"), tag(m, "LifecycleState")) {
                out.push(ScaleInstance { id, lifecycle });
            }
        }
        rest = &after[end..];
    }
    out
}

/// Records every call; its instances are set by the test: `by_group` for a group named
/// there, `instances` for every other group. `desired` holds each group's desired capacity
/// (0 for one never set) and follows every successful set. A set matching an entry of
/// `fail_sets` (group, desired) fails once and removes that entry.
#[derive(Default)]
pub struct FakeScale {
    pub calls: Mutex<Vec<(String, u32)>>,
    pub instances: Mutex<Vec<ScaleInstance>>,
    pub by_group: Mutex<HashMap<String, Vec<ScaleInstance>>>,
    pub desired: Mutex<HashMap<String, u32>>,
    pub fail_sets: Mutex<Vec<(String, u32)>>,
    /// Each group's failed launches: (start time in Unix ms, status message).
    pub failed: Mutex<HashMap<String, Vec<(u64, String)>>>,
}

#[async_trait]
impl ScaleDriver for FakeScale {
    async fn set_desired(&self, group: &Group, desired: u32) -> Result<()> {
        {
            let mut fail = self.fail_sets.lock().unwrap();
            if let Some(p) = fail
                .iter()
                .position(|(n, d)| *n == group.name && *d == desired)
            {
                fail.remove(p);
                bail!("injected failure setting {} to {desired}", group.name);
            }
        }
        self.calls
            .lock()
            .unwrap()
            .push((group.name.clone(), desired));
        self.desired
            .lock()
            .unwrap()
            .insert(group.name.clone(), desired);
        Ok(())
    }
    async fn instances(&self, group: &Group) -> Result<Vec<ScaleInstance>> {
        if let Some(i) = self.by_group.lock().unwrap().get(&group.name) {
            return Ok(i.clone());
        }
        Ok(self.instances.lock().unwrap().clone())
    }
    async fn desired(&self, group: &Group) -> Result<u32> {
        Ok(self
            .desired
            .lock()
            .unwrap()
            .get(&group.name)
            .copied()
            .unwrap_or(0))
    }
    async fn failed_launches(&self, group: &Group, since: u64) -> Result<Vec<String>> {
        Ok(self
            .failed
            .lock()
            .unwrap()
            .get(&group.name)
            .map(|v| {
                v.iter()
                    .filter(|(t, _)| *t >= since)
                    .map(|(_, m)| m.clone())
                    .collect()
            })
            .unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describe_answers_parse_to_instances() {
        let xml = r#"<DescribeAutoScalingGroupsResponse xmlns="http://autoscaling.amazonaws.com/doc/2011-01-01/">
  <DescribeAutoScalingGroupsResult><AutoScalingGroups><member>
    <AutoScalingGroupName>fleet-hosts</AutoScalingGroupName>
    <Instances>
      <member><InstanceId>i-0123456789abcdef0</InstanceId><LifecycleState>Pending</LifecycleState><HealthStatus>Healthy</HealthStatus></member>
      <member><InstanceId>i-1234567890abcdef0</InstanceId><LifecycleState>InService</LifecycleState></member>
    </Instances>
    <DesiredCapacity>2</DesiredCapacity>
  </member></AutoScalingGroups></DescribeAutoScalingGroupsResult>
</DescribeAutoScalingGroupsResponse>"#;
        let got = parse_instances(xml);
        assert_eq!(
            got,
            vec![
                ScaleInstance {
                    id: "i-0123456789abcdef0".into(),
                    lifecycle: "Pending".into()
                },
                ScaleInstance {
                    id: "i-1234567890abcdef0".into(),
                    lifecycle: "InService".into()
                },
            ]
        );
        assert!(parse_instances("<Instances/>").is_empty());
        assert_eq!(
            "us-east-1:fleet-hosts".parse::<Group>().unwrap().name,
            "fleet-hosts"
        );
        assert!("fleet-hosts".parse::<Group>().is_err());
    }

    fn group(s: &str) -> Group {
        s.parse().unwrap()
    }

    #[test]
    fn on_demand_groups_pair_with_the_spot_group_in_their_region() {
        let spot = [group("us-east-1:hosts"), group("us-east-2:hosts")];
        assert_eq!(
            pair_on_demand(&spot, &[group("us-east-1:hosts-od")]).unwrap(),
            vec![Some(group("us-east-1:hosts-od")), None]
        );
        assert_eq!(pair_on_demand(&spot, &[]).unwrap(), vec![None, None]);
        // No spot group in its region, two in one region, or the spot group itself.
        assert!(pair_on_demand(&spot, &[group("us-west-2:hosts-od")]).is_err());
        assert!(pair_on_demand(&spot, &[group("us-east-1:a"), group("us-east-1:b")]).is_err());
        assert!(pair_on_demand(&spot, &[group("us-east-1:hosts")]).is_err());
        let two = [group("us-east-1:a"), group("us-east-1:b")];
        assert!(pair_on_demand(&two, &[group("us-east-1:od")]).is_err());
    }

    #[test]
    fn instances_on_their_way_out_do_not_count() {
        let live = |l: &str| {
            is_live(&ScaleInstance {
                id: "i-0".into(),
                lifecycle: l.into(),
            })
        };
        for l in ["Pending", "Pending:Wait", "InService", "Standby"] {
            assert!(live(l), "{l}");
        }
        for l in [
            "Terminating",
            "Terminating:Wait",
            "Terminated",
            "Detaching",
            "Detached",
        ] {
            assert!(!live(l), "{l}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_short_spot_group_falls_back_after_the_wait_and_only_once() {
        let wait = Duration::from_secs(120);
        let mut f = Fallback::default();
        let t0 = Instant::now();
        // Short, but not for the whole wait yet.
        assert_eq!(f.observe(t0, 1, 0, 0, wait, false), None);
        assert_eq!(
            f.observe(t0 + Duration::from_secs(119), 1, 0, 0, wait, false),
            None
        );
        // Short for the wait: hold spot at what it has, on-demand takes the rest.
        assert_eq!(f.observe(t0 + wait, 1, 0, 0, wait, false), Some((0, 1)));
        // Not applied (the call failed): asked again.
        assert_eq!(f.observe(t0 + wait, 1, 0, 0, wait, false), Some((0, 1)));
        f.applied((0, 1));
        // Applied, and the on-demand launch fails too: never more than the shortfall.
        assert_eq!(f.observe(t0 + 10 * wait, 1, 0, 0, wait, false), None);
        // The on-demand host arrives: not short.
        assert_eq!(f.observe(t0 + 11 * wait, 1, 0, 1, wait, false), None);
    }

    #[tokio::test(start_paused = true)]
    async fn a_spot_group_that_fills_in_time_never_falls_back() {
        let wait = Duration::from_secs(120);
        let mut f = Fallback::default();
        let t0 = Instant::now();
        assert_eq!(f.observe(t0, 2, 0, 0, wait, false), None);
        assert_eq!(
            f.observe(t0 + Duration::from_secs(60), 2, 2, 0, wait, false),
            None
        );
        // Short again later: the wait starts over.
        assert_eq!(
            f.observe(t0 + Duration::from_secs(200), 2, 1, 0, wait, false),
            None
        );
        assert_eq!(
            f.observe(t0 + Duration::from_secs(200) + wait, 2, 1, 0, wait, false),
            Some((1, 1))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_fallback_not_fully_written_is_retried_after_capacity_appears() {
        let wait = Duration::from_secs(120);
        let mut f = Fallback::default();
        let t0 = Instant::now();
        assert_eq!(f.observe(t0, 1, 0, 0, wait, false), None);
        assert_eq!(f.observe(t0 + wait, 1, 0, 0, wait, false), Some((0, 1)));
        // The on-demand write landed and its instance is pending, but holding the spot
        // group failed: the two groups together are not short any more, and the spot
        // group would still launch. The split is asked for again until it is applied.
        assert_eq!(f.observe(t0 + wait, 1, 0, 1, wait, false), Some((0, 1)));
        assert_eq!(f.pending(), Some((0, 1)));
        f.applied((0, 1));
        assert_eq!(f.pending(), None);
        assert_eq!(f.observe(t0 + 2 * wait, 1, 0, 1, wait, false), None);
    }

    /// A DescribeScalingActivities answer as Auto Scaling gave it for the staging spot group
    /// (2026-10-25, instance ids replaced): failed launches since the wake began, newest
    /// first, and nothing older or successful.
    #[test]
    fn failed_spot_launches_parse_from_the_scaling_activities() {
        let xml = r#"<DescribeScalingActivitiesResponse xmlns="http://autoscaling.amazonaws.com/doc/2011-01-01/">
  <DescribeScalingActivitiesResult><Activities>
    <member><ActivityId>a3</ActivityId><AutoScalingGroupName>fleet-hosts</AutoScalingGroupName>
      <Description>Launching a new EC2 instance.  Status Reason: Could not launch Spot Instances. MaxSpotInstanceCountExceeded - Max spot instance count exceeded. Launching EC2 instance failed.</Description>
      <StatusCode>Failed</StatusCode><StatusMessage>Could not launch Spot Instances. MaxSpotInstanceCountExceeded - Max spot instance count exceeded. Launching EC2 instance failed.</StatusMessage>
      <StartTime>2026-10-25T05:40:11.080Z</StartTime><Progress>100</Progress></member>
    <member><ActivityId>a2</ActivityId><Description>Launching a new EC2 instance: i-0123456789abcdef0</Description>
      <StatusCode>Successful</StatusCode><StartTime>2026-10-25T05:23:37.888Z</StartTime></member>
    <member><ActivityId>a1</ActivityId><Description>Launching a new EC2 instance.  Status Reason: Could not launch Spot Instances. UnfulfillableCapacity - Unable to fulfill capacity due to your request configuration. Please adjust your request and try again. Launching EC2 instance failed.</Description>
      <StatusCode>Failed</StatusCode><StartTime>2026-10-25T05:23:26.582Z</StartTime></member>
    <member><ActivityId>a0</ActivityId><StatusCode>Failed</StatusCode><StatusMessage>before the wake</StatusMessage>
      <StartTime>2026-10-24T21:57:06.649Z</StartTime></member>
  </Activities></DescribeScalingActivitiesResult></DescribeScalingActivitiesResponse>"#;
        let since = chrono::DateTime::parse_from_rfc3339("2026-10-25T05:23:19Z")
            .unwrap()
            .timestamp_millis() as u64;
        let got = parse_failed_activities(xml, since);
        assert_eq!(got.len(), 2, "{got:?}");
        assert!(got[0].starts_with("Could not launch Spot Instances. MaxSpot"));
        // No StatusMessage: the description stands in.
        assert!(got[1].contains("UnfulfillableCapacity"));
        assert!(parse_failed_activities("<Activities/>", 0).is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_spot_launch_falls_back_without_the_wait() {
        let wait = Duration::from_secs(120);
        let mut f = Fallback::default();
        let t0 = Instant::now();
        assert_eq!(f.observe(t0, 1, 0, 0, wait, false), None);
        assert_eq!(
            f.observe(t0 + Duration::from_secs(12), 1, 0, 0, wait, true),
            Some((0, 1))
        );
        // Not short: a failure (a rebalance launch while the host runs) changes nothing.
        let mut g = Fallback::default();
        assert_eq!(g.observe(t0, 1, 1, 0, wait, true), None);
    }

    #[test]
    fn the_path_names_the_groups_the_hosts_came_from() {
        assert_eq!(path_name(1, 0), "spot");
        assert_eq!(path_name(0, 1), "on-demand");
        assert_eq!(path_name(1, 1), "spot and on-demand");
    }

    #[test]
    fn describe_answers_parse_to_a_desired_capacity() {
        let xml = r#"<DescribeAutoScalingGroupsResponse><DescribeAutoScalingGroupsResult><AutoScalingGroups><member>
    <AutoScalingGroupName>fleet-hosts-on-demand</AutoScalingGroupName><MinSize>0</MinSize><MaxSize>8</MaxSize>
    <DesiredCapacity>1</DesiredCapacity><Instances/></member></AutoScalingGroups></DescribeAutoScalingGroupsResult>
</DescribeAutoScalingGroupsResponse>"#;
        assert_eq!(parse_desired(xml), Some(1));
        // A name that matches no group answers an empty list.
        assert_eq!(
            parse_desired("<DescribeAutoScalingGroupsResult><AutoScalingGroups/></DescribeAutoScalingGroupsResult>"),
            None
        );
    }
}

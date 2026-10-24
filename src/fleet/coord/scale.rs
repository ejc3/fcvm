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
    /// How long a waking spot group may stay short before its on-demand group takes over.
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

/// One spot group's fallback to its on-demand group during a wake.
#[derive(Debug, Default)]
pub struct Fallback {
    short_since: Option<Instant>,
    applied: Option<(u32, u32)>,
}

impl Fallback {
    /// After a poll of a waking fleet: the desired capacities (spot, on-demand) to set, once
    /// the two groups' live instances have stayed below `want` for `wait`. The spot group
    /// is held at what it has and the on-demand group takes the rest, so the two never ask
    /// for more than `want` together. `None` while they are not short, before `wait` has
    /// passed, or when the same capacities were already set.
    pub fn observe(
        &mut self,
        now: Instant,
        want: u32,
        spot_live: u32,
        on_demand_live: u32,
        wait: Duration,
    ) -> Option<(u32, u32)> {
        if spot_live.saturating_add(on_demand_live) >= want {
            self.short_since = None;
            return None;
        }
        let since = *self.short_since.get_or_insert(now);
        if now.duration_since(since) < wait {
            return None;
        }
        let target = (spot_live, want - spot_live);
        (self.applied != Some(target)).then_some(target)
    }

    /// Both capacities of `target` were set.
    pub fn applied(&mut self, target: (u32, u32)) {
        self.applied = Some(target);
    }
}

/// Follow the fleet's power state: set every group's desired capacity on each change
/// (0 asleep, `awake_capacity` otherwise; on-demand groups 0 asleep and untouched
/// otherwise), report the groups' instances while waking, and fall back to on-demand
/// groups as the module says.
pub async fn run(cfg: PowerConfig, mut power: watch::Receiver<String>, handle: Handle) {
    let on_demand = |i: usize| cfg.on_demand.get(i).cloned().flatten();
    let mut set: Option<u32> = None;
    let mut fallbacks: Vec<Fallback> = cfg.groups.iter().map(|_| Fallback::default()).collect();
    let mut fallback_ok = true;
    loop {
        let state = power.borrow_and_update().clone();
        let want = if state == "asleep" {
            0
        } else {
            cfg.awake_capacity
        };
        if set != Some(want) {
            let mut ok = true;
            for (i, g) in cfg.groups.iter().enumerate() {
                // Asleep, the on-demand group goes to 0 with its spot group; awake, only a
                // fallback raises it.
                let od = on_demand(i).filter(|_| want == 0);
                for g in std::iter::once(g).chain(od.as_ref()) {
                    if let Err(e) = cfg.driver.set_desired(g, want).await {
                        ok = false;
                        degraded(&handle, g, &e).await;
                    } else {
                        tracing::info!(group = %g.name, region = %g.region, desired = want, "desired capacity set");
                    }
                }
            }
            if want == 0 {
                fallbacks.iter_mut().for_each(|f| *f = Fallback::default());
            }
            if ok {
                set = Some(want);
                healthy(&handle).await;
            }
        }
        if state == "waking" {
            let mut all = Vec::new();
            fallback_ok = true;
            for (i, g) in cfg.groups.iter().enumerate() {
                let spot = match cfg.driver.instances(g).await {
                    Ok(i) => i,
                    Err(e) => {
                        tracing::warn!(group = %g.name, error = %format!("{e:#}"), "describing the group failed");
                        continue;
                    }
                };
                let spot_live = spot.iter().filter(|i| is_live(i)).count() as u32;
                all.extend(spot);
                let Some(od) = on_demand(i) else { continue };
                // A group that cannot be described is not known to be short: no fallback.
                let od_instances = match cfg.driver.instances(&od).await {
                    Ok(i) => i,
                    Err(e) => {
                        tracing::warn!(group = %od.name, error = %format!("{e:#}"), "describing the group failed");
                        continue;
                    }
                };
                let od_live = od_instances.iter().filter(|i| is_live(i)).count() as u32;
                all.extend(od_instances);
                let Some((spot_to, od_to)) = fallbacks[i].observe(
                    Instant::now(),
                    cfg.awake_capacity,
                    spot_live,
                    od_live,
                    cfg.spot_wait,
                ) else {
                    continue;
                };
                tracing::warn!(
                    spot_group = %g.name, on_demand_group = %od.name, region = %g.region,
                    spot_live, on_demand_live = od_live, want = cfg.awake_capacity,
                    wait_s = cfg.spot_wait.as_secs(),
                    "spot group short after the wait; the on-demand group takes the shortfall"
                );
                // The on-demand group first: if holding the spot group then fails, the
                // next poll sets both again.
                let mut ok = true;
                for (grp, to) in [(&od, od_to), (g, spot_to)] {
                    if let Err(e) = cfg.driver.set_desired(grp, to).await {
                        ok = false;
                        degraded(&handle, grp, &e).await;
                        break;
                    }
                    tracing::info!(group = %grp.name, region = %grp.region, desired = to, "desired capacity set");
                }
                if ok {
                    fallbacks[i].applied((spot_to, od_to));
                } else {
                    fallback_ok = false;
                }
            }
            handle
                .send(Event::ScaleInstances {
                    at: 0,
                    instances: all,
                })
                .await;
        }
        // A failed set is retried on the next poll.
        let wait = if state == "waking" || set != Some(want) || !fallback_ok {
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
/// there, `instances` for every other group.
#[derive(Default)]
pub struct FakeScale {
    pub calls: Mutex<Vec<(String, u32)>>,
    pub instances: Mutex<Vec<ScaleInstance>>,
    pub by_group: Mutex<HashMap<String, Vec<ScaleInstance>>>,
}

#[async_trait]
impl ScaleDriver for FakeScale {
    async fn set_desired(&self, group: &Group, desired: u32) -> Result<()> {
        self.calls
            .lock()
            .unwrap()
            .push((group.name.clone(), desired));
        Ok(())
    }
    async fn instances(&self, group: &Group) -> Result<Vec<ScaleInstance>> {
        if let Some(i) = self.by_group.lock().unwrap().get(&group.name) {
            return Ok(i.clone());
        }
        Ok(self.instances.lock().unwrap().clone())
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
        assert_eq!(f.observe(t0, 1, 0, 0, wait), None);
        assert_eq!(
            f.observe(t0 + Duration::from_secs(119), 1, 0, 0, wait),
            None
        );
        // Short for the wait: hold spot at what it has, on-demand takes the rest.
        assert_eq!(f.observe(t0 + wait, 1, 0, 0, wait), Some((0, 1)));
        // Not applied (the call failed): asked again.
        assert_eq!(f.observe(t0 + wait, 1, 0, 0, wait), Some((0, 1)));
        f.applied((0, 1));
        // Applied, and the on-demand launch fails too: never more than the shortfall.
        assert_eq!(f.observe(t0 + 10 * wait, 1, 0, 0, wait), None);
        // The on-demand host arrives: not short.
        assert_eq!(f.observe(t0 + 11 * wait, 1, 0, 1, wait), None);
    }

    #[tokio::test(start_paused = true)]
    async fn a_spot_group_that_fills_in_time_never_falls_back() {
        let wait = Duration::from_secs(120);
        let mut f = Fallback::default();
        let t0 = Instant::now();
        assert_eq!(f.observe(t0, 2, 0, 0, wait), None);
        assert_eq!(f.observe(t0 + Duration::from_secs(60), 2, 2, 0, wait), None);
        // Short again later: the wait starts over.
        assert_eq!(
            f.observe(t0 + Duration::from_secs(200), 2, 1, 0, wait),
            None
        );
        assert_eq!(
            f.observe(t0 + Duration::from_secs(200) + wait, 2, 1, 0, wait),
            Some((1, 1))
        );
    }
}

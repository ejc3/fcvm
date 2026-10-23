//! The GPU hosts' auto scaling groups. The coordinator owns their desired capacity: zero
//! while the fleet is asleep, the configured target otherwise; while it wakes it polls
//! their instances for the page's progress.

use super::aws::CredentialChain;
use super::sigv4::{authorization, uri_encode};
use super::Handle;
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use fleet_core::event::ScaleInstance;
use fleet_core::Event;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;

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
}

/// Follow the fleet's power state: set every group's desired capacity on each change
/// (0 asleep, `awake_capacity` otherwise) and report the groups' instances while waking.
pub async fn run(cfg: PowerConfig, mut power: watch::Receiver<String>, handle: Handle) {
    let mut set: Option<u32> = None;
    loop {
        let state = power.borrow_and_update().clone();
        let want = if state == "asleep" {
            0
        } else {
            cfg.awake_capacity
        };
        if set != Some(want) {
            let mut ok = true;
            for g in &cfg.groups {
                if let Err(e) = cfg.driver.set_desired(g, want).await {
                    ok = false;
                    tracing::warn!(group = %g.name, region = %g.region, error = %format!("{e:#}"), "setting desired capacity failed");
                    handle
                        .send(Event::Degraded {
                            at: 0,
                            component: "scale".into(),
                            reason: Some(format!("{e:#}")),
                        })
                        .await;
                } else {
                    tracing::info!(group = %g.name, region = %g.region, desired = want, "desired capacity set");
                }
            }
            if ok {
                set = Some(want);
                handle
                    .send(Event::Degraded {
                        at: 0,
                        component: "scale".into(),
                        reason: None,
                    })
                    .await;
            }
        }
        if state == "waking" {
            let mut all = Vec::new();
            for g in &cfg.groups {
                match cfg.driver.instances(g).await {
                    Ok(mut i) => all.append(&mut i),
                    Err(e) => {
                        tracing::warn!(group = %g.name, error = %format!("{e:#}"), "describing the group failed")
                    }
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
        let wait = if state == "waking" || set != Some(want) {
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

/// The text of the first `<name>` element in `xml`.
fn tag(xml: &str, name: &str) -> Option<String> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(xml[start..end].trim().to_string())
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

/// Records every call; its instances are set by the test.
#[derive(Default)]
pub struct FakeScale {
    pub calls: Mutex<Vec<(String, u32)>>,
    pub instances: Mutex<Vec<ScaleInstance>>,
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
    async fn instances(&self, _group: &Group) -> Result<Vec<ScaleInstance>> {
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
      <member><InstanceId>i-0fedcba9876543210</InstanceId><LifecycleState>InService</LifecycleState></member>
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
                    id: "i-0fedcba9876543210".into(),
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
}

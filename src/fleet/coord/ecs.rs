//! The coordinator task's own public addresses on ECS (Fargate, awsvpc), for the EC2 half's
//! aggregator that runs beside it: the IPv6 address from the task metadata endpoint
//! (`ECS_CONTAINER_METADATA_URI_V4`, which lists the task's addresses, IPv6 when its subnet
//! and the account's `dualStackIPv6` setting give one), and the public IPv4 address that
//! EC2 associated with the task's network interface (`DescribeNetworkInterfaces` by its
//! private address; the metadata does not carry it).

use super::aws::{signed, xml_tag, CredentialChain};
use anyhow::{bail, Context, Result};
use std::time::Duration;

/// The public addresses: (IPv6, IPv4).
#[derive(Debug, Clone, PartialEq)]
pub struct TaskAddresses {
    pub ipv6: Option<String>,
    pub ipv4: Option<String>,
}

/// The task's private IPv4 and global IPv6 addresses from a container metadata answer
/// (`GET ${ECS_CONTAINER_METADATA_URI_V4}`).
pub fn parse_metadata(body: &str) -> Result<(Option<String>, Option<String>)> {
    let v: serde_json::Value =
        serde_json::from_str(body).context("the task metadata is not JSON")?;
    let net = v["Networks"]
        .as_array()
        .and_then(|n| n.first())
        .context("the task metadata lists no network")?;
    let first = |k: &str| {
        net[k]
            .as_array()
            .and_then(|a| a.first())
            .and_then(|a| a.as_str())
            .map(str::to_string)
    };
    let ipv6 = first("IPv6Addresses").filter(|a| {
        a.parse::<std::net::Ipv6Addr>()
            .is_ok_and(|ip| (ip.segments()[0] & 0xe000) == 0x2000)
    });
    Ok((first("IPv4Addresses"), ipv6))
}

/// The public IPv4 address in a `DescribeNetworkInterfaces` answer, if the interface has one.
pub fn parse_public_ip(xml: &str) -> Option<String> {
    xml_tag(xml, "publicIp").filter(|a| a.parse::<std::net::Ipv4Addr>().is_ok())
}

/// Read both, with the task role's credentials, in `region` (the task's own).
pub async fn task_addresses(region: &str) -> Result<TaskAddresses> {
    let uri = std::env::var("ECS_CONTAINER_METADATA_URI_V4")
        .context("ECS_CONTAINER_METADATA_URI_V4 is not set: not an ECS task")?;
    let http = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()?;
    let body = http
        .get(&uri)
        .send()
        .await
        .context("reading the task metadata")?
        .error_for_status()?
        .text()
        .await?;
    let (private_v4, ipv6) = parse_metadata(&body)?;
    let ipv4 = match private_v4 {
        Some(private) => {
            let chain = CredentialChain::new()?;
            let ec2 = reqwest::Client::builder()
                .timeout(Duration::from_secs(20))
                .build()?;
            let body = format!(
                "Action=DescribeNetworkInterfaces&Version=2016-11-15&Filter.1.Name=addresses.private-ip-address&Filter.1.Value.1={}",
                super::sigv4::uri_encode(&private)
            );
            let (status, bytes) = signed(
                &chain,
                &ec2,
                &format!("https://ec2.{region}.amazonaws.com"),
                reqwest::Method::POST,
                "/",
                "",
                region,
                "ec2",
                &[(
                    "content-type",
                    "application/x-www-form-urlencoded; charset=utf-8".into(),
                )],
                body.into_bytes(),
            )
            .await?;
            let xml = String::from_utf8_lossy(&bytes);
            if !status.is_success() {
                bail!(
                    "EC2 DescribeNetworkInterfaces in {region} answered {status}: {}: {}",
                    xml_tag(&xml, "Code").unwrap_or_default(),
                    xml_tag(&xml, "Message").unwrap_or_default()
                );
            }
            parse_public_ip(&xml)
        }
        None => None,
    };
    if ipv6.is_none() && ipv4.is_none() {
        bail!("the task has neither a global IPv6 address nor a public IPv4 address");
    }
    Ok(TaskAddresses { ipv6, ipv4 })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_task_addresses_are_read_from_metadata_and_ec2() {
        let dual = r#"{"DockerId":"x","Networks":[{"NetworkMode":"awsvpc",
            "IPv4Addresses":["10.0.1.5"],"IPv6Addresses":["2001:db8:20::5"]}]}"#;
        assert_eq!(
            parse_metadata(dual).unwrap(),
            (Some("10.0.1.5".into()), Some("2001:db8:20::5".into()))
        );
        let v4_only = r#"{"Networks":[{"IPv4Addresses":["10.0.1.5"]}]}"#;
        assert_eq!(
            parse_metadata(v4_only).unwrap(),
            (Some("10.0.1.5".into()), None)
        );
        // A link-local or unique-local address is not one a browser reaches.
        let ula = r#"{"Networks":[{"IPv4Addresses":["10.0.1.5"],"IPv6Addresses":["fd00::5"]}]}"#;
        assert_eq!(parse_metadata(ula).unwrap().1, None);
        assert!(parse_metadata(r#"{"Networks":[]}"#).is_err());

        let xml = "<DescribeNetworkInterfacesResponse><networkInterfaceSet><item>\
            <networkInterfaceId>eni-0</networkInterfaceId><association>\
            <publicIp>192.0.2.44</publicIp><ipOwnerId>amazon</ipOwnerId></association>\
            <privateIpAddress>10.0.1.5</privateIpAddress></item></networkInterfaceSet>\
            </DescribeNetworkInterfacesResponse>";
        assert_eq!(parse_public_ip(xml).as_deref(), Some("192.0.2.44"));
        assert_eq!(
            parse_public_ip("<networkInterfaceSet><item></item></networkInterfaceSet>"),
            None
        );
    }
}

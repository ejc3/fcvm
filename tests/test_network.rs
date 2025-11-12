/// Integration test for network connectivity
/// Run with: cargo test --test test_network -- --nocapture
use anyhow::{Context, Result};
use std::process::Command;
use std::time::Duration;
use tokio::time::sleep;

#[tokio::test]
#[ignore] // Run manually: cargo test --test test_network -- --ignored
async fn test_network_stack() -> Result<()> {
    println!("\n=== Network Stack Test ===\n");

    // Test 1: Check if TAP device exists and is UP
    println!("Test 1: Checking TAP device status...");
    let tap_check = Command::new("ip")
        .args(&["link", "show"])
        .output()
        .context("Failed to run 'ip link show'")?;

    let tap_output = String::from_utf8_lossy(&tap_check.stdout);
    let tap_found = tap_output.lines().any(|line| line.contains("tap-vm-"));

    if tap_found {
        println!("✓ TAP device found");
        for line in tap_output.lines() {
            if line.contains("tap-vm-") {
                println!("  {}", line);
                if line.contains("state UP") {
                    println!("  ✓ TAP device is UP");
                } else {
                    println!("  ✗ TAP device is DOWN!");
                }
            }
        }
    } else {
        println!("✗ No TAP device found!");
    }

    // Test 2: Check IP forwarding
    println!("\nTest 2: Checking IP forwarding...");
    let ip_forward = std::fs::read_to_string("/proc/sys/net/ipv4/ip_forward")
        .context("Failed to read ip_forward")?;

    if ip_forward.trim() == "1" {
        println!("✓ IP forwarding is enabled");
    } else {
        println!("✗ IP forwarding is DISABLED!");
    }

    // Test 3: Check iptables NAT rules
    println!("\nTest 3: Checking iptables NAT rules...");
    let nat_check = Command::new("sudo")
        .args(&["iptables", "-t", "nat", "-L", "POSTROUTING", "-n", "-v"])
        .output()
        .context("Failed to check iptables")?;

    let nat_output = String::from_utf8_lossy(&nat_check.stdout);
    if nat_output.contains("MASQUERADE") && nat_output.contains("172.16.0.0/24") {
        println!("✓ NAT MASQUERADE rule found for 172.16.0.0/24");
    } else {
        println!("✗ NAT MASQUERADE rule NOT found!");
        println!("NAT rules:");
        println!("{}", nat_output);
    }

    // Test 4: Ping guest from host (if we know guest IP)
    println!("\nTest 4: Pinging guest from host...");
    let ping_result = Command::new("ping")
        .args(&["-c", "3", "-W", "2", "172.16.0.2"])
        .output()
        .context("Failed to ping guest")?;

    if ping_result.status.success() {
        println!("✓ Can ping guest at 172.16.0.2");
    } else {
        println!("✗ Cannot ping guest at 172.16.0.2");
        println!("{}", String::from_utf8_lossy(&ping_result.stderr));
    }

    // Test 5: DNS resolution from host
    println!("\nTest 5: DNS resolution from host...");
    let dns_test = Command::new("dig")
        .args(&["+short", "@8.8.8.8", "registry-1.docker.io"])
        .output()
        .context("Failed to test DNS")?;

    if dns_test.status.success() {
        println!("✓ DNS works from host:");
        println!("  {}", String::from_utf8_lossy(&dns_test.stdout).trim());
    } else {
        println!("✗ DNS failed from host");
    }

    // Test 6: Check default route
    println!("\nTest 6: Checking default route...");
    let route_check = Command::new("ip")
        .args(&["route", "show", "default"])
        .output()
        .context("Failed to check routes")?;

    let route_output = String::from_utf8_lossy(&route_check.stdout);
    println!("Default route: {}", route_output.trim());

    // Test 7: Trace route to 8.8.8.8 from host
    println!("\nTest 7: Testing connectivity to 8.8.8.8...");
    let traceroute = Command::new("timeout")
        .args(&["5", "ping", "-c", "2", "8.8.8.8"])
        .output()
        .context("Failed to ping 8.8.8.8")?;

    if traceroute.status.success() {
        println!("✓ Can reach 8.8.8.8 from host");
    } else {
        println!("✗ Cannot reach 8.8.8.8 from host!");
    }

    println!("\n=== Test Complete ===\n");
    Ok(())
}

/// Test DNS specifically by checking dnsmasq
#[tokio::test]
#[ignore]
async fn test_dns_setup() -> Result<()> {
    println!("\n=== DNS Setup Test ===\n");

    // Check if dnsmasq is running
    println!("Checking dnsmasq status...");
    let dnsmasq_check = Command::new("systemctl")
        .args(&["status", "dnsmasq"])
        .output()
        .context("Failed to check dnsmasq")?;

    let status_output = String::from_utf8_lossy(&dnsmasq_check.stdout);
    if status_output.contains("active (running)") {
        println!("✓ dnsmasq is running");
    } else {
        println!("✗ dnsmasq is NOT running!");
        println!("{}", status_output);
    }

    // Check dnsmasq configuration
    println!("\nChecking dnsmasq configuration...");
    if let Ok(config) = std::fs::read_to_string("/etc/dnsmasq.d/fcvm.conf") {
        println!("✓ fcvm.conf exists:");
        println!("{}", config);
    } else {
        println!("✗ fcvm.conf NOT found!");
    }

    // Test DNS query to dnsmasq
    println!("\nTesting DNS query via dnsmasq...");
    let dns_query = Command::new("dig")
        .args(&["+short", "@127.0.0.1", "google.com"])
        .output()
        .context("Failed to query dnsmasq")?;

    if dns_query.status.success() && !dns_query.stdout.is_empty() {
        println!("✓ DNS resolution via dnsmasq works:");
        println!("  {}", String::from_utf8_lossy(&dns_query.stdout).trim());
    } else {
        println!("✗ DNS resolution via dnsmasq failed!");
    }

    println!("\n=== Test Complete ===\n");
    Ok(())
}

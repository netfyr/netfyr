//! `netfyr show` — system overview: daemon status and interface details.

use std::process::ExitCode;
use std::sync::Arc;

use anyhow::Result;
use clap::{Args, ValueEnum};
use colored::Colorize;

use netfyr_backend::{BackendRegistry, NetlinkBackend};
use netfyr_state::StateSet;
use netfyr_varlink::{
    VarlinkClient, VarlinkDaemonInfo, VarlinkError, VarlinkInterfaceInfo, VarlinkShowInfo,
};

use crate::daemon_socket_path;

// ── ShowOutputFormat ──────────────────────────────────────────────────────────

#[derive(Clone, ValueEnum)]
pub enum ShowOutputFormat {
    Text,
    Json,
}

// ── ShowArgs ──────────────────────────────────────────────────────────────────

/// Show system overview
///
/// Display daemon status, all network interfaces, and their active
/// policies and DHCP lease state. Works with or without the daemon.
#[derive(Args)]
pub struct ShowArgs {
    /// Output format: text (default) or json.
    #[arg(short, long, default_value = "text")]
    pub output: ShowOutputFormat,
}

// ── run_show ──────────────────────────────────────────────────────────────────

pub async fn run_show(args: ShowArgs) -> Result<ExitCode> {
    let socket_path = daemon_socket_path();

    let info = match VarlinkClient::connect(&socket_path).await {
        Ok(mut client) => match client.get_show_info().await {
            Ok(mut info) => {
                info.interfaces.sort_by(|a, b| a.name.cmp(&b.name));
                info
            }
            Err(VarlinkError::ConnectionFailed(_)) => {
                eprintln!("Error: lost connection to netfyr daemon");
                return Ok(ExitCode::from(1u8));
            }
            Err(e) => {
                eprintln!("Error: {e}");
                return Ok(ExitCode::from(1u8));
            }
        },
        Err(VarlinkError::ConnectionFailed(_)) => {
            // Daemon not running — fall back to direct backend query.
            match build_fallback_info().await {
                Ok(info) => info,
                Err(e) => {
                    eprintln!("Error: {e}");
                    return Ok(ExitCode::from(1u8));
                }
            }
        }
        Err(e) => {
            eprintln!("Error: {e}");
            return Ok(ExitCode::from(1u8));
        }
    };

    match args.output {
        ShowOutputFormat::Text => print!("{}", format_text(&info)),
        ShowOutputFormat::Json => match format_json(&info) {
            Ok(s) => println!("{s}"),
            Err(e) => {
                eprintln!("Error: {e}");
                return Ok(ExitCode::from(1u8));
            }
        },
    }

    Ok(ExitCode::SUCCESS)
}

// ── Fallback (daemon not running) ─────────────────────────────────────────────

async fn build_fallback_info() -> Result<VarlinkShowInfo> {
    let registry = create_backend_registry();
    let state_set = registry.query_all().await?;
    let interfaces = build_fallback_interfaces(&state_set);
    Ok(VarlinkShowInfo {
        daemon: VarlinkDaemonInfo { status: "not_running".to_string(), uptime_seconds: None },
        interfaces,
    })
}

fn build_fallback_interfaces(state_set: &StateSet) -> Vec<VarlinkInterfaceInfo> {
    let mut seen = std::collections::BTreeSet::new();
    let mut interfaces = Vec::new();

    for state in state_set.iter() {
        let name: String = match &state.selector.name {
            Some(n) => n.clone(),
            None => continue,
        };
        if !seen.insert(name.clone()) {
            continue;
        }

        let enabled = state.fields.get("enabled").and_then(|fv| fv.value.as_bool());
        let carrier = state.fields.get("carrier").and_then(|fv| fv.value.as_bool());
        let addresses: Option<Vec<String>> = state
            .fields
            .get("addresses")
            .and_then(|fv| fv.value.as_list())
            .map(|list| list.iter().map(|v| v.to_string()).collect())
            .filter(|v: &Vec<String>| !v.is_empty());

        interfaces.push(VarlinkInterfaceInfo {
            name,
            enabled,
            carrier,
            addresses,
            policies: None,
            dhcp: None,
            config_state: None,
            config_drift: None,
        });
    }

    interfaces
}

fn create_backend_registry() -> BackendRegistry {
    let mut registry = BackendRegistry::new();
    registry
        .register(Arc::new(NetlinkBackend::new()))
        .expect("failed to register NetlinkBackend");
    registry
}


// ── format_text ───────────────────────────────────────────────────────────────

pub fn format_text(info: &VarlinkShowInfo) -> String {
    let mut out = String::new();

    out.push_str("Daemon\n");
    let status_display = if info.daemon.status == "not_running" {
        "not running"
    } else {
        &info.daemon.status
    };
    out.push_str(&format!("  Status:  {status_display}\n"));
    if let Some(secs) = info.daemon.uptime_seconds {
        out.push_str(&format!(
            "  Uptime:  {}\n",
            format_duration(secs as u64)
        ));
    }

    out.push('\n');
    out.push_str("Interfaces\n");

    let n = info.interfaces.len();
    for (i, iface) in info.interfaces.iter().enumerate() {
        out.push_str(&format!("  {}\n", iface.name));

        if let Some(enabled) = iface.enabled {
            let state_text = format_state(enabled, iface.carrier.unwrap_or(false));
            out.push_str(&format!("    State:     {state_text}\n"));
        }

        if let Some(addresses) = &iface.addresses {
            if !addresses.is_empty() {
                out.push_str(&format!("    Addresses: {}\n", addresses.join(", ")));
            }
        }

        if let Some(policies) = &iface.policies {
            if !policies.is_empty() {
                let policy_str: Vec<String> = policies
                    .iter()
                    .map(|p| format!("{} ({})", p.name, p.policy_type))
                    .collect();
                out.push_str(&format!("    Policies:  {}\n", policy_str.join(", ")));
            }
        }

        if let Some(dhcp) = &iface.dhcp {
            out.push_str(&format!("    DHCP:      {}\n", dhcp.state));
            if dhcp.state == "running" {
                if let Some(total) = dhcp.lease_time_secs {
                    let remaining = dhcp.lease_remaining_secs.unwrap_or(0);
                    out.push_str(&format!(
                        "    Lease:     {}s total, {} remaining\n",
                        total,
                        format_duration(remaining as u64)
                    ));
                }
            }
        }

        if let Some(config_state) = &iface.config_state {
            let colored_state = if config_state == "applied" {
                config_state.green().to_string()
            } else {
                config_state.red().to_string()
            };
            out.push_str(&format!("    Config:    {colored_state}\n"));

            if let Some(drift) = &iface.config_drift {
                for entry in drift {
                    out.push_str(&format!(
                        "               {}\n",
                        format!("- {}: {}", entry.field_name, entry.description).yellow()
                    ));
                }
            }
        }

        if i + 1 < n {
            out.push('\n');
        }
    }

    out
}

fn format_state(enabled: bool, carrier: bool) -> String {
    match (enabled, carrier) {
        (true, true) => "up, carrier".green().to_string(),
        (true, false) => "up, no-carrier".yellow().to_string(),
        (false, true) => "down, carrier".yellow().to_string(),
        (false, false) => "down".red().to_string(),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use netfyr_varlink::{VarlinkDaemonInfo, VarlinkDhcpInfo, VarlinkDriftEntry, VarlinkInterfaceInfo, VarlinkPolicyInfo, VarlinkShowInfo};

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn make_daemon_running(uptime_secs: i64) -> VarlinkDaemonInfo {
        VarlinkDaemonInfo { status: "running".to_string(), uptime_seconds: Some(uptime_secs) }
    }

    fn make_daemon_not_running() -> VarlinkDaemonInfo {
        VarlinkDaemonInfo { status: "not_running".to_string(), uptime_seconds: None }
    }

    fn make_static_policy_info(name: &str) -> VarlinkPolicyInfo {
        VarlinkPolicyInfo { name: name.to_string(), policy_type: "static".to_string() }
    }

    fn make_dhcpv4_policy_info(name: &str) -> VarlinkPolicyInfo {
        VarlinkPolicyInfo { name: name.to_string(), policy_type: "dhcpv4".to_string() }
    }

    fn make_running_dhcp(lease_addr: &str, lease_time: i64, lease_remaining: i64) -> VarlinkDhcpInfo {
        VarlinkDhcpInfo {
            state: "running".to_string(),
            lease_address: Some(lease_addr.to_string()),
            lease_time_secs: Some(lease_time),
            lease_remaining_secs: Some(lease_remaining),
        }
    }

    fn make_waiting_dhcp() -> VarlinkDhcpInfo {
        VarlinkDhcpInfo { state: "waiting".to_string(), lease_address: None, lease_time_secs: None, lease_remaining_secs: None }
    }

    fn make_iface(name: &str, policies: Option<Vec<VarlinkPolicyInfo>>, dhcp: Option<VarlinkDhcpInfo>) -> VarlinkInterfaceInfo {
        VarlinkInterfaceInfo {
            name: name.to_string(),
            enabled: Some(true),
            carrier: Some(true),
            addresses: None,
            policies,
            dhcp,
            config_state: None,
            config_drift: None,
        }
    }

    fn make_bare_iface(name: &str) -> VarlinkInterfaceInfo {
        VarlinkInterfaceInfo {
            name: name.to_string(),
            enabled: None,
            carrier: None,
            addresses: None,
            policies: None,
            dhcp: None,
            config_state: None,
            config_drift: None,
        }
    }

    fn make_show_info(daemon: VarlinkDaemonInfo, interfaces: Vec<VarlinkInterfaceInfo>) -> VarlinkShowInfo {
        VarlinkShowInfo { daemon, interfaces }
    }

    // ── format_text: Daemon section ───────────────────────────────────────────

    /// Scenario: Show displays daemon status and interfaces when daemon is running.
    /// The Daemon section must include "Status:" with "running".
    #[test]
    fn test_format_text_daemon_running_shows_status_running() {
        let info = make_show_info(make_daemon_running(8100), vec![]);
        let text = format_text(&info);
        assert!(text.contains("Daemon"), "output must contain 'Daemon' header");
        assert!(
            text.contains("Status:  running"),
            "output must contain 'Status:  running', got:\n{text}"
        );
    }

    /// The Daemon section includes an Uptime line when daemon is running.
    #[test]
    fn test_format_text_daemon_running_shows_uptime_line() {
        let info = make_show_info(make_daemon_running(8100), vec![]);
        let text = format_text(&info);
        assert!(text.contains("Uptime:"), "output must contain 'Uptime:' line when daemon is running");
    }

    /// Scenario: Uptime format uses compact two-unit durations — 7385 seconds → "2h 3m".
    #[test]
    fn test_format_text_uptime_7385_seconds_shows_2h_3m() {
        let info = make_show_info(make_daemon_running(7385), vec![]);
        let text = format_text(&info);
        assert!(
            text.contains("2h 3m"),
            "uptime for 7385s must be '2h 3m', got:\n{text}"
        );
    }

    /// Scenario: Show works when daemon is not running — Status shows "not running".
    #[test]
    fn test_format_text_daemon_not_running_shows_not_running_status() {
        let info = make_show_info(make_daemon_not_running(), vec![make_bare_iface("lo")]);
        let text = format_text(&info);
        assert!(
            text.contains("Status:  not running"),
            "output must contain 'Status:  not running' when daemon is off, got:\n{text}"
        );
    }

    /// When daemon is not running, no Uptime line should appear.
    #[test]
    fn test_format_text_daemon_not_running_omits_uptime_line() {
        let info = make_show_info(make_daemon_not_running(), vec![make_bare_iface("lo")]);
        let text = format_text(&info);
        assert!(
            !text.contains("Uptime:"),
            "output must not contain 'Uptime:' when daemon is not running, got:\n{text}"
        );
    }

    // ── format_text: Interfaces section ──────────────────────────────────────

    /// The Interfaces section is always present.
    #[test]
    fn test_format_text_always_shows_interfaces_header() {
        let info = make_show_info(make_daemon_running(100), vec![]);
        let text = format_text(&info);
        assert!(text.contains("Interfaces"), "output must contain 'Interfaces' section");
    }

    /// Scenario: Show lists all system interfaces including unmanaged — all appear.
    #[test]
    fn test_format_text_all_interfaces_listed_including_unmanaged() {
        let info = make_show_info(
            make_daemon_running(100),
            vec![
                make_iface("veth-e2e0", Some(vec![make_static_policy_info("pol1")]), None),
                make_bare_iface("veth-e2e1"),
                make_bare_iface("lo"),
            ],
        );
        let text = format_text(&info);
        assert!(text.contains("veth-e2e0"), "veth-e2e0 must appear in output");
        assert!(text.contains("veth-e2e1"), "veth-e2e1 must appear in output");
        assert!(text.contains("lo"), "lo must appear in output");
    }

    /// Managed interface with a static policy shows a Policies line.
    #[test]
    fn test_format_text_managed_interface_shows_policies_line() {
        let info = make_show_info(
            make_daemon_running(100),
            vec![make_iface("veth-e2e0", Some(vec![make_static_policy_info("mgmt")]), None)],
        );
        let text = format_text(&info);
        assert!(
            text.contains("Policies:"),
            "managed interface must show 'Policies:' line, got:\n{text}"
        );
    }

    /// Scenario: Show displays static-only interfaces — Policies line includes "(static)".
    #[test]
    fn test_format_text_static_policy_shows_type_static() {
        let info = make_show_info(
            make_daemon_running(100),
            vec![make_iface("veth-e2e0", Some(vec![make_static_policy_info("server-net")]), None)],
        );
        let text = format_text(&info);
        assert!(
            text.contains("server-net (static)"),
            "static policy must be shown as 'name (static)', got:\n{text}"
        );
    }

    /// Static-only interfaces must not show DHCP or Lease lines.
    #[test]
    fn test_format_text_static_only_interface_omits_dhcp_and_lease_lines() {
        let info = make_show_info(
            make_daemon_running(100),
            vec![make_iface("veth-e2e0", Some(vec![make_static_policy_info("server-net")]), None)],
        );
        let text = format_text(&info);
        assert!(!text.contains("DHCP:"), "static-only interface must not show 'DHCP:' line, got:\n{text}");
        assert!(!text.contains("Lease:"), "static-only interface must not show 'Lease:' line, got:\n{text}");
    }

    /// Unmanaged interface (no policies, no dhcp) appears as bare name only.
    #[test]
    fn test_format_text_unmanaged_interface_appears_as_bare_name() {
        let info = make_show_info(
            make_daemon_running(100),
            vec![
                make_iface("veth-e2e0", Some(vec![make_static_policy_info("pol")]), None),
                make_bare_iface("lo"),
            ],
        );
        let text = format_text(&info);
        assert!(text.contains("lo"), "'lo' must appear in output");
        // Confirm no policy/DHCP lines follow "lo" by checking the structure
        let lo_pos = text.find("  lo\n").expect("'lo' entry must appear indented");
        let after_lo = &text[lo_pos + 5..]; // skip "  lo\n"
        // Next non-empty character should not be a deeper indent for Policies/DHCP
        let next_line = after_lo.lines().next().unwrap_or("");
        assert!(
            !next_line.starts_with("    Policies:") && !next_line.starts_with("    DHCP:"),
            "'lo' must not have Policies or DHCP lines, got after lo:\n{after_lo}"
        );
    }

    // ── format_text: DHCP factory ─────────────────────────────────────────────

    /// Scenario: Show displays DHCP factory with lease timing — Policies line shows "(dhcpv4)".
    #[test]
    fn test_format_text_dhcp_interface_shows_dhcpv4_policy_type() {
        let info = make_show_info(
            make_daemon_running(100),
            vec![make_iface(
                "veth-dhcp0",
                Some(vec![make_dhcpv4_policy_info("server-dhcp")]),
                Some(make_running_dhcp("192.168.122.63/24", 3600, 3252)),
            )],
        );
        let text = format_text(&info);
        assert!(
            text.contains("server-dhcp (dhcpv4)"),
            "DHCP policy must be shown as 'name (dhcpv4)', got:\n{text}"
        );
    }

    /// DHCP running factory shows "DHCP:      running".
    #[test]
    fn test_format_text_running_dhcp_factory_shows_dhcp_running() {
        let info = make_show_info(
            make_daemon_running(100),
            vec![make_iface(
                "veth-dhcp0",
                Some(vec![make_dhcpv4_policy_info("server-dhcp")]),
                Some(make_running_dhcp("10.0.0.5/24", 120, 105)),
            )],
        );
        let text = format_text(&info);
        assert!(
            text.contains("DHCP:      running"),
            "running factory must show 'DHCP:      running', got:\n{text}"
        );
    }

    /// DHCP running factory shows Lease line with total seconds and remaining time.
    #[test]
    fn test_format_text_running_dhcp_factory_shows_lease_line_with_total_and_remaining() {
        let info = make_show_info(
            make_daemon_running(100),
            vec![make_iface(
                "veth-dhcp0",
                Some(vec![make_dhcpv4_policy_info("server-dhcp")]),
                Some(make_running_dhcp("10.0.0.5/24", 120, 105)),
            )],
        );
        let text = format_text(&info);
        assert!(
            text.contains("Lease:"),
            "running factory must show 'Lease:' line, got:\n{text}"
        );
        assert!(
            text.contains("120s total"),
            "Lease line must show '120s total', got:\n{text}"
        );
        assert!(
            text.contains("remaining"),
            "Lease line must contain 'remaining', got:\n{text}"
        );
    }

    /// Scenario: Lease remaining format uses compact two-unit durations.
    /// 3600s lease, 600s remaining → "10m remaining".
    #[test]
    fn test_format_text_lease_remaining_600_secs_shows_10m() {
        let info = make_show_info(
            make_daemon_running(100),
            vec![make_iface(
                "veth-dhcp0",
                Some(vec![make_dhcpv4_policy_info("server-dhcp")]),
                Some(make_running_dhcp("10.0.0.1/24", 3600, 600)),
            )],
        );
        let text = format_text(&info);
        assert!(
            text.contains("3600s total"),
            "Lease line must show '3600s total', got:\n{text}"
        );
        assert!(
            text.contains("10m remaining"),
            "Lease remaining 600s must show '10m remaining', got:\n{text}"
        );
    }

    /// Scenario: Show displays factory in waiting state — DHCP line shows "waiting".
    #[test]
    fn test_format_text_waiting_dhcp_factory_shows_dhcp_waiting() {
        let info = make_show_info(
            make_daemon_running(100),
            vec![make_iface(
                "veth-dhcp0",
                Some(vec![make_dhcpv4_policy_info("server-dhcp")]),
                Some(make_waiting_dhcp()),
            )],
        );
        let text = format_text(&info);
        assert!(
            text.contains("DHCP:      waiting"),
            "waiting factory must show 'DHCP:      waiting', got:\n{text}"
        );
    }

    /// Waiting factory must not show a Lease line.
    #[test]
    fn test_format_text_waiting_dhcp_factory_omits_lease_line() {
        let info = make_show_info(
            make_daemon_running(100),
            vec![make_iface(
                "veth-dhcp0",
                Some(vec![make_dhcpv4_policy_info("server-dhcp")]),
                Some(make_waiting_dhcp()),
            )],
        );
        let text = format_text(&info);
        assert!(
            !text.contains("Lease:"),
            "waiting factory must not show 'Lease:' line, got:\n{text}"
        );
    }

    // ── format_text: Multiple policies ────────────────────────────────────────

    /// Scenario: Show with multiple policies on one interface — both appear comma-separated.
    #[test]
    fn test_format_text_multiple_policies_listed_comma_separated() {
        let info = make_show_info(
            make_daemon_running(100),
            vec![make_iface(
                "veth-dhcp0",
                Some(vec![
                    make_static_policy_info("server-mtu"),
                    make_dhcpv4_policy_info("server-dhcp"),
                ]),
                Some(make_running_dhcp("10.0.0.1/24", 3600, 3000)),
            )],
        );
        let text = format_text(&info);
        assert!(
            text.contains("server-mtu (static), server-dhcp (dhcpv4)"),
            "multiple policies must appear comma-separated on Policies line, got:\n{text}"
        );
    }

    // ── format_text: Daemon not running ──────────────────────────────────────

    /// Scenario: Show works when daemon is not running — Interfaces lists bare names only.
    #[test]
    fn test_format_text_daemon_not_running_interfaces_are_bare_names() {
        let info = make_show_info(
            make_daemon_not_running(),
            vec![
                make_bare_iface("enp7s0"),
                make_bare_iface("enp1s0"),
                make_bare_iface("lo"),
            ],
        );
        let text = format_text(&info);
        assert!(text.contains("enp7s0"), "enp7s0 must appear");
        assert!(text.contains("enp1s0"), "enp1s0 must appear");
        assert!(text.contains("lo"), "lo must appear");
        assert!(
            !text.contains("Policies:"),
            "daemon-not-running output must not contain 'Policies:' lines, got:\n{text}"
        );
        assert!(
            !text.contains("DHCP:"),
            "daemon-not-running output must not contain 'DHCP:' lines, got:\n{text}"
        );
        assert!(
            !text.contains("Lease:"),
            "daemon-not-running output must not contain 'Lease:' lines, got:\n{text}"
        );
    }

    // ── format_json: Basic structure ──────────────────────────────────────────

    /// Scenario: Show JSON output when daemon is running — output is valid JSON.
    #[test]
    fn test_format_json_produces_valid_json() {
        let info = make_show_info(
            make_daemon_running(8103),
            vec![make_bare_iface("lo")],
        );
        let json_str = format_json(&info).expect("format_json must succeed");
        let parsed: serde_json::Value = serde_json::from_str(&json_str)
            .expect("format_json must produce valid JSON");
        assert!(parsed.is_object(), "JSON output must be an object");
    }

    /// JSON output has "daemon.status" as "running".
    #[test]
    fn test_format_json_daemon_running_has_status_running() {
        let info = make_show_info(make_daemon_running(8103), vec![]);
        let parsed: serde_json::Value = serde_json::from_str(&format_json(&info).unwrap()).unwrap();
        assert_eq!(
            parsed["daemon"]["status"],
            "running",
            "daemon.status must be 'running'"
        );
    }

    /// JSON output has "daemon.uptime_seconds" as a non-negative integer when running.
    #[test]
    fn test_format_json_daemon_running_has_uptime_seconds() {
        let info = make_show_info(make_daemon_running(8103), vec![]);
        let parsed: serde_json::Value = serde_json::from_str(&format_json(&info).unwrap()).unwrap();
        let uptime = parsed["daemon"]["uptime_seconds"]
            .as_i64()
            .expect("daemon.uptime_seconds must be an integer when daemon is running");
        assert!(uptime >= 0, "uptime_seconds must be non-negative, got {uptime}");
        assert_eq!(uptime, 8103, "uptime_seconds must match the provided value");
    }

    /// JSON output has "interfaces" as an array.
    #[test]
    fn test_format_json_interfaces_is_array() {
        let info = make_show_info(make_daemon_running(100), vec![make_bare_iface("lo")]);
        let parsed: serde_json::Value = serde_json::from_str(&format_json(&info).unwrap()).unwrap();
        assert!(parsed["interfaces"].is_array(), "interfaces must be a JSON array");
    }

    // ── format_json: DHCP running ─────────────────────────────────────────────

    /// Scenario: Show JSON output with DHCP lease — interface entry has "policies" and "dhcp".
    #[test]
    fn test_format_json_dhcp_running_interface_has_policies_and_dhcp_fields() {
        let info = make_show_info(
            make_daemon_running(8103),
            vec![make_iface(
                "veth-dhcp0",
                Some(vec![make_dhcpv4_policy_info("server-dhcp")]),
                Some(make_running_dhcp("192.168.122.63/24", 3600, 3252)),
            )],
        );
        let parsed: serde_json::Value = serde_json::from_str(&format_json(&info).unwrap()).unwrap();
        let ifaces = parsed["interfaces"].as_array().unwrap();
        let iface = ifaces.iter().find(|i| i["name"] == "veth-dhcp0")
            .expect("veth-dhcp0 must appear in interfaces");
        assert!(iface.get("policies").is_some(), "veth-dhcp0 must have 'policies' field");
        assert!(iface.get("dhcp").is_some(), "veth-dhcp0 must have 'dhcp' field");
    }

    /// dhcp.state is "running" for a running factory.
    #[test]
    fn test_format_json_dhcp_running_state_is_running() {
        let info = make_show_info(
            make_daemon_running(100),
            vec![make_iface(
                "veth-dhcp0",
                Some(vec![make_dhcpv4_policy_info("server-dhcp")]),
                Some(make_running_dhcp("192.168.122.63/24", 3600, 3252)),
            )],
        );
        let parsed: serde_json::Value = serde_json::from_str(&format_json(&info).unwrap()).unwrap();
        let iface = &parsed["interfaces"][0];
        assert_eq!(iface["dhcp"]["state"], "running", "dhcp.state must be 'running'");
    }

    /// dhcp.lease_time_secs matches the server's lease time.
    #[test]
    fn test_format_json_dhcp_running_has_correct_lease_time_secs() {
        let info = make_show_info(
            make_daemon_running(100),
            vec![make_iface(
                "veth-dhcp0",
                Some(vec![make_dhcpv4_policy_info("server-dhcp")]),
                Some(make_running_dhcp("192.168.122.63/24", 3600, 3252)),
            )],
        );
        let parsed: serde_json::Value = serde_json::from_str(&format_json(&info).unwrap()).unwrap();
        let iface = &parsed["interfaces"][0];
        assert_eq!(
            iface["dhcp"]["lease_time_secs"].as_i64(),
            Some(3600),
            "dhcp.lease_time_secs must match the server's lease time"
        );
    }

    /// dhcp.lease_remaining_secs is a non-negative integer for a running factory.
    #[test]
    fn test_format_json_dhcp_running_has_non_negative_lease_remaining() {
        let info = make_show_info(
            make_daemon_running(100),
            vec![make_iface(
                "veth-dhcp0",
                Some(vec![make_dhcpv4_policy_info("server-dhcp")]),
                Some(make_running_dhcp("192.168.122.63/24", 3600, 3252)),
            )],
        );
        let parsed: serde_json::Value = serde_json::from_str(&format_json(&info).unwrap()).unwrap();
        let iface = &parsed["interfaces"][0];
        let remaining = iface["dhcp"]["lease_remaining_secs"]
            .as_i64()
            .expect("dhcp.lease_remaining_secs must be present for running factory");
        assert!(remaining >= 0, "lease_remaining_secs must be non-negative, got {remaining}");
    }

    // ── format_json: DHCP waiting ─────────────────────────────────────────────

    /// Scenario: Show JSON output for waiting factory — dhcp.state is "waiting".
    #[test]
    fn test_format_json_waiting_factory_has_dhcp_state_waiting() {
        let info = make_show_info(
            make_daemon_running(100),
            vec![make_iface(
                "veth-dhcp0",
                Some(vec![make_dhcpv4_policy_info("server-dhcp")]),
                Some(make_waiting_dhcp()),
            )],
        );
        let parsed: serde_json::Value = serde_json::from_str(&format_json(&info).unwrap()).unwrap();
        let iface = &parsed["interfaces"][0];
        assert_eq!(
            iface["dhcp"]["state"],
            "waiting",
            "waiting factory must have dhcp.state = 'waiting'"
        );
    }

    /// Waiting factory dhcp object must not have lease_time_secs or lease_remaining_secs.
    #[test]
    fn test_format_json_waiting_factory_omits_lease_fields() {
        let info = make_show_info(
            make_daemon_running(100),
            vec![make_iface(
                "veth-dhcp0",
                Some(vec![make_dhcpv4_policy_info("server-dhcp")]),
                Some(make_waiting_dhcp()),
            )],
        );
        let parsed: serde_json::Value = serde_json::from_str(&format_json(&info).unwrap()).unwrap();
        let dhcp_obj = parsed["interfaces"][0]["dhcp"]
            .as_object()
            .expect("dhcp field must be a JSON object");
        assert!(
            !dhcp_obj.contains_key("lease_time_secs"),
            "waiting factory dhcp must not have 'lease_time_secs' (must be absent, not null)"
        );
        assert!(
            !dhcp_obj.contains_key("lease_remaining_secs"),
            "waiting factory dhcp must not have 'lease_remaining_secs' (must be absent, not null)"
        );
    }

    // ── format_json: Daemon not running ──────────────────────────────────────

    /// Scenario: Show JSON output when daemon is not running — status is "not_running".
    #[test]
    fn test_format_json_daemon_not_running_has_status_not_running() {
        let info = make_show_info(
            make_daemon_not_running(),
            vec![make_bare_iface("enp7s0"), make_bare_iface("lo")],
        );
        let parsed: serde_json::Value = serde_json::from_str(&format_json(&info).unwrap()).unwrap();
        assert_eq!(
            parsed["daemon"]["status"],
            "not_running",
            "daemon.status must be 'not_running' when daemon is off"
        );
    }

    /// When daemon is not running, daemon object must not have "uptime_seconds" field.
    #[test]
    fn test_format_json_daemon_not_running_omits_uptime_seconds() {
        let info = make_show_info(
            make_daemon_not_running(),
            vec![make_bare_iface("lo")],
        );
        let parsed: serde_json::Value = serde_json::from_str(&format_json(&info).unwrap()).unwrap();
        let daemon_obj = parsed["daemon"].as_object().expect("daemon must be a JSON object");
        assert!(
            !daemon_obj.contains_key("uptime_seconds"),
            "daemon must not have 'uptime_seconds' when not running (must be absent, not null)"
        );
    }

    /// When daemon is not running, all interfaces have only a "name" field (no policies/dhcp).
    #[test]
    fn test_format_json_daemon_not_running_interfaces_have_only_name() {
        let info = make_show_info(
            make_daemon_not_running(),
            vec![
                make_bare_iface("enp7s0"),
                make_bare_iface("enp1s0"),
                make_bare_iface("lo"),
            ],
        );
        let parsed: serde_json::Value = serde_json::from_str(&format_json(&info).unwrap()).unwrap();
        let ifaces = parsed["interfaces"].as_array().expect("interfaces must be an array");
        assert_eq!(ifaces.len(), 3, "must have 3 interfaces");
        for iface in ifaces {
            let obj = iface.as_object().expect("each interface must be a JSON object");
            assert!(obj.contains_key("name"), "each interface must have 'name'");
            assert!(
                !obj.contains_key("policies"),
                "bare interface must not have 'policies' field"
            );
            assert!(
                !obj.contains_key("dhcp"),
                "bare interface must not have 'dhcp' field"
            );
        }
    }

    // ── format_json: Unmanaged interfaces ────────────────────────────────────

    /// Interfaces with no policies omit "policies" and "dhcp" from JSON output.
    #[test]
    fn test_format_json_unmanaged_interface_omits_policies_and_dhcp_fields() {
        let info = make_show_info(
            make_daemon_running(100),
            vec![
                make_iface("veth-e2e0", Some(vec![make_static_policy_info("pol")]), None),
                make_bare_iface("lo"),
            ],
        );
        let parsed: serde_json::Value = serde_json::from_str(&format_json(&info).unwrap()).unwrap();
        let ifaces = parsed["interfaces"].as_array().unwrap();
        let lo_iface = ifaces.iter().find(|i| i["name"] == "lo")
            .expect("lo must appear in interfaces");
        let lo_obj = lo_iface.as_object().unwrap();
        assert!(!lo_obj.contains_key("policies"), "'lo' must not have 'policies' field");
        assert!(!lo_obj.contains_key("dhcp"), "'lo' must not have 'dhcp' field");
    }

    /// Managed interface with policies: the "policies" array has the correct name and type.
    #[test]
    fn test_format_json_managed_interface_policies_array_has_name_and_type() {
        let info = make_show_info(
            make_daemon_running(100),
            vec![make_iface(
                "veth-e2e0",
                Some(vec![make_static_policy_info("mgmt")]),
                None,
            )],
        );
        let parsed: serde_json::Value = serde_json::from_str(&format_json(&info).unwrap()).unwrap();
        let iface = &parsed["interfaces"][0];
        let policies = iface["policies"].as_array().expect("policies must be an array");
        assert_eq!(policies.len(), 1);
        assert_eq!(policies[0]["name"], "mgmt");
        assert_eq!(policies[0]["type"], "static");
    }

    /// Scenario: Show with multiple policies on one interface — both appear in JSON array.
    #[test]
    fn test_format_json_multiple_policies_appear_in_policies_array() {
        let info = make_show_info(
            make_daemon_running(100),
            vec![make_iface(
                "veth-dhcp0",
                Some(vec![
                    make_static_policy_info("server-mtu"),
                    make_dhcpv4_policy_info("server-dhcp"),
                ]),
                Some(make_running_dhcp("10.0.0.1/24", 3600, 3000)),
            )],
        );
        let parsed: serde_json::Value = serde_json::from_str(&format_json(&info).unwrap()).unwrap();
        let policies = parsed["interfaces"][0]["policies"]
            .as_array()
            .expect("policies must be an array");
        assert_eq!(policies.len(), 2, "must have 2 policies in the array");
        let types: Vec<&str> = policies.iter().map(|p| p["type"].as_str().unwrap()).collect();
        assert!(types.contains(&"static"), "policies must include a static entry");
        assert!(types.contains(&"dhcpv4"), "policies must include a dhcpv4 entry");
    }

    // ── format_text: State line ─────────────────────────────────────────────

    /// Interface with enabled=true and carrier=true shows "up, carrier" in State line.
    #[test]
    fn test_format_text_up_carrier_shows_state_line() {
        colored::control::set_override(false);
        let info = make_show_info(
            make_daemon_running(100),
            vec![make_iface("veth-e2e0", Some(vec![make_static_policy_info("pol")]), None)],
        );
        let text = format_text(&info);
        assert!(
            text.contains("State:     up, carrier"),
            "up+carrier interface must show 'State:     up, carrier', got:\n{text}"
        );
    }

    /// Interface with enabled=true and carrier=false shows "up, no-carrier".
    #[test]
    fn test_format_text_up_no_carrier_shows_state_line() {
        colored::control::set_override(false);
        let mut iface = make_bare_iface("eth0");
        iface.enabled = Some(true);
        iface.carrier = Some(false);
        let info = make_show_info(make_daemon_running(100), vec![iface]);
        let text = format_text(&info);
        assert!(
            text.contains("State:     up, no-carrier"),
            "up+no-carrier interface must show 'up, no-carrier', got:\n{text}"
        );
    }

    /// Interface with enabled=false shows "down" in State line.
    #[test]
    fn test_format_text_down_shows_state_line() {
        colored::control::set_override(false);
        let mut iface = make_bare_iface("eth0");
        iface.enabled = Some(false);
        iface.carrier = Some(false);
        let info = make_show_info(make_daemon_running(100), vec![iface]);
        let text = format_text(&info);
        assert!(
            text.contains("State:     down"),
            "down interface must show 'down', got:\n{text}"
        );
    }

    /// Interface without enabled field (fallback or absent) omits State line.
    #[test]
    fn test_format_text_no_enabled_omits_state_line() {
        let info = make_show_info(make_daemon_running(100), vec![make_bare_iface("lo")]);
        let text = format_text(&info);
        assert!(
            !text.contains("State:"),
            "interface without enabled must not show State line, got:\n{text}"
        );
    }

    // ── format_text: Addresses line ──────────────────────────────────────────

    /// Interface with addresses shows them comma-separated on Addresses line.
    #[test]
    fn test_format_text_addresses_shown_comma_separated() {
        colored::control::set_override(false);
        let mut iface = make_iface("veth-e2e0", Some(vec![make_static_policy_info("pol")]), None);
        iface.addresses = Some(vec!["192.168.1.10/24".to_string(), "fd00::1/64".to_string()]);
        let info = make_show_info(make_daemon_running(100), vec![iface]);
        let text = format_text(&info);
        assert!(
            text.contains("Addresses: 192.168.1.10/24, fd00::1/64"),
            "addresses must appear comma-separated, got:\n{text}"
        );
    }

    /// Interface with no addresses omits Addresses line.
    #[test]
    fn test_format_text_no_addresses_omits_addresses_line() {
        let info = make_show_info(
            make_daemon_running(100),
            vec![make_iface("veth-e2e0", Some(vec![make_static_policy_info("pol")]), None)],
        );
        let text = format_text(&info);
        assert!(
            !text.contains("Addresses:"),
            "interface without addresses must not show Addresses line, got:\n{text}"
        );
    }

    // ── format_text: Config drift ────────────────────────────────────────────

    /// Managed interface with config_state=applied shows "Config:    applied".
    #[test]
    fn test_format_text_config_applied_shown() {
        colored::control::set_override(false);
        let mut iface = make_iface("veth-e2e0", Some(vec![make_static_policy_info("pol")]), None);
        iface.config_state = Some("applied".to_string());
        let info = make_show_info(make_daemon_running(100), vec![iface]);
        let text = format_text(&info);
        assert!(
            text.contains("Config:    applied"),
            "applied config must show 'Config:    applied', got:\n{text}"
        );
    }

    /// Managed interface with config_state=drifted shows "Config:    drifted" and drift details.
    #[test]
    fn test_format_text_config_drifted_shows_details() {
        colored::control::set_override(false);
        let mut iface = make_iface("veth-e2e0", Some(vec![make_static_policy_info("pol")]), None);
        iface.config_state = Some("drifted".to_string());
        iface.config_drift = Some(vec![
            VarlinkDriftEntry {
                field_name: "mtu".to_string(),
                description: "expected 9000, actual 1500".to_string(),
            },
        ]);
        let info = make_show_info(make_daemon_running(100), vec![iface]);
        let text = format_text(&info);
        assert!(
            text.contains("Config:    drifted"),
            "drifted config must show 'Config:    drifted', got:\n{text}"
        );
        assert!(
            text.contains("- mtu: expected 9000, actual 1500"),
            "drift details must appear, got:\n{text}"
        );
    }

    /// Unmanaged interface (no policies) omits Config line.
    #[test]
    fn test_format_text_unmanaged_omits_config_line() {
        let info = make_show_info(make_daemon_running(100), vec![make_bare_iface("lo")]);
        let text = format_text(&info);
        assert!(
            !text.contains("Config:"),
            "unmanaged interface must not show Config line, got:\n{text}"
        );
    }

    // ── format_json: New fields ──────────────────────────────────────────────

    /// JSON output includes enabled and carrier when present.
    #[test]
    fn test_format_json_includes_enabled_and_carrier() {
        let info = make_show_info(
            make_daemon_running(100),
            vec![make_iface("veth-e2e0", Some(vec![make_static_policy_info("pol")]), None)],
        );
        let parsed: serde_json::Value = serde_json::from_str(&format_json(&info).unwrap()).unwrap();
        let iface = &parsed["interfaces"][0];
        assert_eq!(iface["enabled"], true, "enabled must be true");
        assert_eq!(iface["carrier"], true, "carrier must be true");
    }

    /// JSON output includes addresses when present.
    #[test]
    fn test_format_json_includes_addresses() {
        let mut iface = make_iface("veth-e2e0", Some(vec![make_static_policy_info("pol")]), None);
        iface.addresses = Some(vec!["10.0.0.1/24".to_string()]);
        let info = make_show_info(make_daemon_running(100), vec![iface]);
        let parsed: serde_json::Value = serde_json::from_str(&format_json(&info).unwrap()).unwrap();
        let addrs = parsed["interfaces"][0]["addresses"].as_array()
            .expect("addresses must be an array");
        assert_eq!(addrs[0], "10.0.0.1/24");
    }

    /// JSON output includes config_state and config_drift when present.
    #[test]
    fn test_format_json_includes_config_state_and_drift() {
        let mut iface = make_iface("veth-e2e0", Some(vec![make_static_policy_info("pol")]), None);
        iface.config_state = Some("drifted".to_string());
        iface.config_drift = Some(vec![VarlinkDriftEntry {
            field_name: "mtu".to_string(),
            description: "expected 9000, actual 1500".to_string(),
        }]);
        let info = make_show_info(make_daemon_running(100), vec![iface]);
        let parsed: serde_json::Value = serde_json::from_str(&format_json(&info).unwrap()).unwrap();
        let iface_json = &parsed["interfaces"][0];
        assert_eq!(iface_json["config_state"], "drifted");
        let drift = iface_json["config_drift"].as_array().expect("config_drift must be an array");
        assert_eq!(drift[0]["field_name"], "mtu");
        assert_eq!(drift[0]["description"], "expected 9000, actual 1500");
    }

    /// JSON output omits config_state for unmanaged interfaces.
    #[test]
    fn test_format_json_omits_config_state_for_unmanaged() {
        let info = make_show_info(make_daemon_running(100), vec![make_bare_iface("lo")]);
        let parsed: serde_json::Value = serde_json::from_str(&format_json(&info).unwrap()).unwrap();
        let iface_obj = parsed["interfaces"][0].as_object().unwrap();
        assert!(!iface_obj.contains_key("config_state"), "unmanaged interface must not have config_state");
    }

    // ── CLI argument parsing ──────────────────────────────────────────────────

    /// Scenario: `netfyr show` can be parsed without arguments (defaults to text output).
    #[test]
    fn test_show_args_default_output_is_text() {
        use clap::Parser;
        use crate::Cli;
        let result = Cli::try_parse_from(["netfyr", "show"]);
        assert!(result.is_ok(), "netfyr show with no args must parse successfully");
        let cli = result.unwrap();
        if let crate::Commands::Show(args) = cli.command {
            assert!(
                matches!(args.output, ShowOutputFormat::Text),
                "default output format must be Text"
            );
        } else {
            panic!("expected Show subcommand");
        }
    }

    /// `netfyr show -o json` parses to ShowOutputFormat::Json.
    #[test]
    fn test_show_args_output_json_flag_parses_correctly() {
        use clap::Parser;
        use crate::Cli;
        let result = Cli::try_parse_from(["netfyr", "show", "-o", "json"]);
        assert!(result.is_ok(), "netfyr show -o json must parse successfully");
        let cli = result.unwrap();
        if let crate::Commands::Show(args) = cli.command {
            assert!(
                matches!(args.output, ShowOutputFormat::Json),
                "'-o json' must parse to ShowOutputFormat::Json"
            );
        } else {
            panic!("expected Show subcommand");
        }
    }
}

// ── format_json ───────────────────────────────────────────────────────────────

pub fn format_json(info: &VarlinkShowInfo) -> Result<String> {
    let mut daemon_obj = serde_json::json!({ "status": info.daemon.status });
    if let Some(secs) = info.daemon.uptime_seconds {
        daemon_obj["uptime_seconds"] = serde_json::json!(secs);
    }

    let interfaces: Vec<serde_json::Value> = info
        .interfaces
        .iter()
        .map(|iface| {
            let mut obj = serde_json::json!({ "name": iface.name });

            if let Some(enabled) = iface.enabled {
                obj["enabled"] = serde_json::json!(enabled);
            }
            if let Some(carrier) = iface.carrier {
                obj["carrier"] = serde_json::json!(carrier);
            }
            if let Some(addresses) = &iface.addresses {
                if !addresses.is_empty() {
                    obj["addresses"] = serde_json::json!(addresses);
                }
            }

            if let Some(policies) = &iface.policies {
                if !policies.is_empty() {
                    let policy_arr: Vec<serde_json::Value> = policies
                        .iter()
                        .map(|p| serde_json::json!({ "name": p.name, "type": p.policy_type }))
                        .collect();
                    obj["policies"] = serde_json::json!(policy_arr);
                }
            }

            if let Some(dhcp) = &iface.dhcp {
                let mut dhcp_obj = serde_json::json!({ "state": dhcp.state });
                if let Some(addr) = &dhcp.lease_address {
                    dhcp_obj["lease_address"] = serde_json::json!(addr);
                }
                if let Some(total) = dhcp.lease_time_secs {
                    dhcp_obj["lease_time_secs"] = serde_json::json!(total);
                }
                if let Some(remaining) = dhcp.lease_remaining_secs {
                    dhcp_obj["lease_remaining_secs"] = serde_json::json!(remaining);
                }
                obj["dhcp"] = dhcp_obj;
            }

            if let Some(config_state) = &iface.config_state {
                obj["config_state"] = serde_json::json!(config_state);
            }
            if let Some(config_drift) = &iface.config_drift {
                let drift_arr: Vec<serde_json::Value> = config_drift
                    .iter()
                    .map(|d| {
                        serde_json::json!({
                            "field_name": d.field_name,
                            "description": d.description,
                        })
                    })
                    .collect();
                obj["config_drift"] = serde_json::json!(drift_arr);
            }

            obj
        })
        .collect();

    let root = serde_json::json!({
        "daemon": daemon_obj,
        "interfaces": interfaces,
    });

    Ok(serde_json::to_string_pretty(&root)?)
}

fn format_duration(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        let min = secs / 60;
        let sec = secs % 60;
        if sec == 0 {
            format!("{min}m")
        } else {
            format!("{min}m {sec}s")
        }
    } else if secs < 86400 {
        let hr = secs / 3600;
        let min = (secs % 3600) / 60;
        if min == 0 {
            format!("{hr}h")
        } else {
            format!("{hr}h {min}m")
        }
    } else {
        let days = secs / 86400;
        let hr = (secs % 86400) / 3600;
        if hr == 0 {
            format!("{days}d")
        } else {
            format!("{days}d {hr}h")
        }
    }
}

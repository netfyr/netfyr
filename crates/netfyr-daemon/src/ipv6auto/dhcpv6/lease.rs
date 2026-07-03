//! DHCPv6 lease data types.

use std::net::Ipv6Addr;
use std::time::{Duration, Instant};

/// A single IPv6 address assigned within an IA_NA option.
#[derive(Debug, Clone)]
pub struct Dhcpv6Address {
    pub address: Ipv6Addr,
    /// Always 128 for IA_NA addresses per RFC 8415.
    pub prefix_len: u8,
    /// Preferred lifetime in seconds; 0 when expired/deprecated.
    pub preferred_lft: u32,
    /// Valid lifetime in seconds.
    pub valid_lft: u32,
}

/// Data acquired from a DHCPv6 server (stateful or stateless).
///
/// Stateful leases have `addresses` populated. Stateless leases have an
/// empty `addresses` vec and use `info_refresh_time` for scheduling refreshes.
#[derive(Debug, Clone)]
pub struct Dhcpv6Lease {
    /// Assigned addresses (empty in stateless mode).
    pub addresses: Vec<Dhcpv6Address>,
    pub dns_servers: Vec<Ipv6Addr>,
    pub dns_search: Vec<String>,
    /// Renewal time in seconds (T1); 0 in stateless mode.
    pub t1: u32,
    /// Rebind time in seconds (T2); 0 in stateless mode.
    pub t2: u32,
    /// Server DUID bytes, used to address Renew/Release messages.
    pub server_duid: Vec<u8>,
    /// Server's link-local address, used for unicast Renew.
    pub server_addr: Ipv6Addr,
    /// Information Refresh Time (option 32) for stateless mode.
    pub info_refresh_time: Option<u32>,
    /// Monotonic instant at which the lease was acquired or last renewed.
    pub acquired_at: Instant,
}

impl Dhcpv6Lease {
    /// Return true if the lease (or info refresh) has expired.
    #[allow(dead_code)]
    pub fn is_expired(&self) -> bool {
        let elapsed = self.acquired_at.elapsed();
        if self.addresses.is_empty() {
            // Stateless mode: expire when info_refresh_time elapses.
            let refresh = self.info_refresh_time.unwrap_or(1800) as u64;
            elapsed.as_secs() >= refresh
        } else {
            // Stateful mode: expire when the shortest valid_lft elapses.
            let min_valid = self
                .addresses
                .iter()
                .map(|a| a.valid_lft as u64)
                .min()
                .unwrap_or(0);
            elapsed.as_secs() >= min_valid
        }
    }

    /// Time remaining until T1 (renewal); `Duration::ZERO` if T1 has passed.
    pub fn time_until_renewal(&self) -> Duration {
        Duration::from_secs(self.t1 as u64).saturating_sub(self.acquired_at.elapsed())
    }

    /// Time remaining until T2 (rebind); `Duration::ZERO` if T2 has passed.
    pub fn time_until_rebind(&self) -> Duration {
        Duration::from_secs(self.t2 as u64).saturating_sub(self.acquired_at.elapsed())
    }

    /// Time remaining until lease expiry.
    pub fn time_until_expiry(&self) -> Duration {
        let secs = if self.addresses.is_empty() {
            self.info_refresh_time.unwrap_or(1800) as u64
        } else {
            self.addresses
                .iter()
                .map(|a| a.valid_lft as u64)
                .min()
                .unwrap_or(0)
        };
        Duration::from_secs(secs).saturating_sub(self.acquired_at.elapsed())
    }

    /// Minimum preferred lifetime across all addresses; 0 if no addresses.
    #[allow(dead_code)]
    pub fn shortest_preferred_lft(&self) -> u32 {
        self.addresses
            .iter()
            .map(|a| a.preferred_lft)
            .min()
            .unwrap_or(0)
    }

    /// Compute effective T1/T2 values.
    ///
    /// RFC 8415 §14.2: when T1 or T2 is 0, the client may choose any value.
    /// Convention: T1 = 50% of shortest preferred lifetime, T2 = 80%.
    pub fn compute_t1_t2(addresses: &[Dhcpv6Address], t1: u32, t2: u32) -> (u32, u32) {
        let effective_t1 = if t1 == 0 {
            let sp = addresses.iter().map(|a| a.preferred_lft).min().unwrap_or(0);
            sp / 2
        } else {
            t1
        };
        let effective_t2 = if t2 == 0 {
            let sp = addresses.iter().map(|a| a.preferred_lft).min().unwrap_or(0);
            sp * 4 / 5
        } else {
            t2
        };
        (effective_t1, effective_t2)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv6Addr;
    use std::time::Instant;

    fn make_addr(address: &str, preferred_lft: u32, valid_lft: u32) -> Dhcpv6Address {
        Dhcpv6Address {
            address: address.parse().unwrap(),
            prefix_len: 128,
            preferred_lft,
            valid_lft,
        }
    }

    fn make_stateful_lease(addresses: Vec<Dhcpv6Address>, t1: u32, t2: u32) -> Dhcpv6Lease {
        Dhcpv6Lease {
            addresses,
            dns_servers: vec!["2001:db8::53".parse().unwrap()],
            dns_search: vec!["example.com".to_string()],
            t1,
            t2,
            server_duid: vec![0, 1, 2, 3],
            server_addr: "fe80::1".parse().unwrap(),
            info_refresh_time: None,
            acquired_at: Instant::now(),
        }
    }

    fn make_stateless_lease(
        dns_servers: Vec<Ipv6Addr>,
        dns_search: Vec<String>,
        info_refresh_time: Option<u32>,
    ) -> Dhcpv6Lease {
        Dhcpv6Lease {
            addresses: vec![],
            dns_servers,
            dns_search,
            t1: 0,
            t2: 0,
            server_duid: vec![0, 1, 2, 3],
            server_addr: "fe80::1".parse().unwrap(),
            info_refresh_time,
            acquired_at: Instant::now(),
        }
    }

    // Scenario: Stateful lease contains correct data (addresses, dns_servers, dns_search)
    #[test]
    fn test_stateful_lease_contains_correct_address_data() {
        let addr = make_addr("2001:db8::100", 14400, 86400);
        let lease = Dhcpv6Lease {
            addresses: vec![addr],
            dns_servers: vec!["2001:db8::53".parse().unwrap()],
            dns_search: vec!["example.com".to_string()],
            t1: 7200,
            t2: 11520,
            server_duid: vec![],
            server_addr: "fe80::1".parse().unwrap(),
            info_refresh_time: None,
            acquired_at: Instant::now(),
        };

        assert_eq!(lease.addresses.len(), 1);
        assert_eq!(
            lease.addresses[0].address,
            "2001:db8::100".parse::<Ipv6Addr>().unwrap()
        );
        assert_eq!(lease.addresses[0].prefix_len, 128);
        assert_eq!(lease.addresses[0].preferred_lft, 14400);
        assert_eq!(lease.addresses[0].valid_lft, 86400);
        assert_eq!(
            lease.dns_servers,
            vec!["2001:db8::53".parse::<Ipv6Addr>().unwrap()]
        );
        assert_eq!(lease.dns_search, vec!["example.com".to_string()]);
    }

    // Scenario: Stateless lease has no addresses, t1=0, t2=0
    #[test]
    fn test_stateless_lease_has_no_addresses() {
        let lease = make_stateless_lease(
            vec!["2001:db8::53".parse().unwrap()],
            vec!["example.com".to_string()],
            Some(1800),
        );
        assert!(lease.addresses.is_empty(), "stateless lease must have no addresses");
        assert_eq!(lease.t1, 0, "stateless lease t1 must be 0");
        assert_eq!(lease.t2, 0, "stateless lease t2 must be 0");
        assert!(!lease.dns_servers.is_empty(), "stateless lease must have dns_servers");
        assert!(!lease.dns_search.is_empty(), "stateless lease must have dns_search");
    }

    // Scenario: Stateful lease is not expired when freshly acquired with long lifetime
    #[test]
    fn test_stateful_lease_not_expired_when_fresh() {
        let addr = make_addr("2001:db8::1", 3600, 86400);
        let lease = make_stateful_lease(vec![addr], 1800, 2880);
        assert!(!lease.is_expired(), "fresh stateful lease must not be expired");
    }

    // Scenario: Stateful lease is expired immediately when valid_lft=0
    #[test]
    fn test_stateful_lease_expired_when_valid_lft_zero() {
        let addr = make_addr("2001:db8::1", 0, 0);
        let lease = make_stateful_lease(vec![addr], 0, 0);
        assert!(lease.is_expired(), "stateful lease with valid_lft=0 must be expired");
    }

    // Scenario: Stateless lease is not expired when freshly acquired
    #[test]
    fn test_stateless_lease_not_expired_when_fresh() {
        let lease = make_stateless_lease(vec![], vec![], None);
        assert!(
            !lease.is_expired(),
            "fresh stateless lease must not be expired (1800s default refresh)"
        );
    }

    // Scenario: Stateless lease is expired when info_refresh_time=0
    #[test]
    fn test_stateless_lease_expired_when_info_refresh_zero() {
        let lease = make_stateless_lease(vec![], vec![], Some(0));
        assert!(
            lease.is_expired(),
            "stateless lease with info_refresh_time=0 must be expired"
        );
    }

    // Scenario: time_until_renewal is positive for a fresh lease with t1>0
    #[test]
    fn test_time_until_renewal_positive_when_t1_future() {
        let addr = make_addr("2001:db8::1", 3600, 86400);
        let lease = make_stateful_lease(vec![addr], 3600, 5760);
        let until = lease.time_until_renewal();
        assert!(
            until.as_secs() > 3590,
            "time_until_renewal must be close to t1 for a fresh lease, got {}s",
            until.as_secs()
        );
    }

    // Scenario: time_until_renewal returns ZERO when t1=0
    #[test]
    fn test_time_until_renewal_zero_when_t1_is_zero() {
        let addr = make_addr("2001:db8::1", 3600, 86400);
        let lease = make_stateful_lease(vec![addr], 0, 0);
        assert_eq!(
            lease.time_until_renewal(),
            Duration::ZERO,
            "time_until_renewal must be ZERO when t1=0"
        );
    }

    // Scenario: time_until_rebind is positive for a fresh lease with t2>0
    #[test]
    fn test_time_until_rebind_positive_when_t2_future() {
        let addr = make_addr("2001:db8::1", 3600, 86400);
        let lease = make_stateful_lease(vec![addr], 1800, 5760);
        let until = lease.time_until_rebind();
        assert!(
            until.as_secs() > 5750,
            "time_until_rebind must be close to t2 for fresh lease, got {}s",
            until.as_secs()
        );
    }

    // Scenario: time_until_rebind returns ZERO when t2=0
    #[test]
    fn test_time_until_rebind_zero_when_t2_is_zero() {
        let addr = make_addr("2001:db8::1", 3600, 86400);
        let lease = make_stateful_lease(vec![addr], 0, 0);
        assert_eq!(
            lease.time_until_rebind(),
            Duration::ZERO,
            "time_until_rebind must be ZERO when t2=0"
        );
    }

    // Scenario: time_until_expiry is close to valid_lft for a fresh stateful lease
    #[test]
    fn test_time_until_expiry_stateful_fresh() {
        let addr = make_addr("2001:db8::1", 3600, 86400);
        let lease = make_stateful_lease(vec![addr], 1800, 2880);
        let expiry = lease.time_until_expiry();
        assert!(
            expiry.as_secs() > 86380,
            "time_until_expiry must be close to valid_lft, got {}s",
            expiry.as_secs()
        );
    }

    // Scenario: time_until_expiry for stateless lease uses info_refresh_time
    #[test]
    fn test_time_until_expiry_stateless_uses_info_refresh() {
        let lease = make_stateless_lease(
            vec!["2001:db8::53".parse().unwrap()],
            vec!["example.com".to_string()],
            Some(1800),
        );
        let expiry = lease.time_until_expiry();
        assert!(
            expiry.as_secs() > 1780,
            "stateless time_until_expiry must be close to info_refresh_time, got {}s",
            expiry.as_secs()
        );
    }

    // Scenario: time_until_expiry for stateless lease defaults to 1800s
    #[test]
    fn test_stateless_lease_default_info_refresh_time() {
        let lease = make_stateless_lease(vec![], vec![], None);
        let expiry = lease.time_until_expiry();
        assert!(
            expiry.as_secs() > 1780,
            "default stateless expiry must be close to 1800s, got {}s",
            expiry.as_secs()
        );
    }

    // Scenario: shortest_preferred_lft returns minimum across multiple addresses
    #[test]
    fn test_shortest_preferred_lft_returns_minimum() {
        let addrs = vec![
            make_addr("2001:db8::1", 3600, 86400),
            make_addr("2001:db8::2", 7200, 86400),
        ];
        let lease = make_stateful_lease(addrs, 1800, 2880);
        assert_eq!(
            lease.shortest_preferred_lft(),
            3600,
            "shortest_preferred_lft must return the minimum preferred_lft"
        );
    }

    // Scenario: shortest_preferred_lft returns 0 for empty address list
    #[test]
    fn test_shortest_preferred_lft_empty_returns_zero() {
        let lease = make_stateless_lease(vec![], vec![], None);
        assert_eq!(
            lease.shortest_preferred_lft(),
            0,
            "shortest_preferred_lft must return 0 when no addresses"
        );
    }

    // Scenario: compute_t1_t2 uses server-provided values when nonzero
    #[test]
    fn test_compute_t1_t2_uses_server_values_when_nonzero() {
        let addrs = vec![make_addr("2001:db8::1", 14400, 86400)];
        let (t1, t2) = Dhcpv6Lease::compute_t1_t2(&addrs, 3600, 5760);
        assert_eq!(t1, 3600, "t1 must use server value when nonzero");
        assert_eq!(t2, 5760, "t2 must use server value when nonzero");
    }

    // Scenario: compute_t1_t2 defaults to 50%/80% of shortest preferred_lft when server sends 0
    #[test]
    fn test_compute_t1_t2_defaults_to_50_80_percent_when_zero() {
        let addrs = vec![make_addr("2001:db8::1", 14400, 86400)];
        let (t1, t2) = Dhcpv6Lease::compute_t1_t2(&addrs, 0, 0);
        assert_eq!(t1, 7200, "t1 must default to 50% of preferred_lft (14400/2=7200)");
        assert_eq!(t2, 11520, "t2 must default to 80% of preferred_lft (14400*4/5=11520)");
    }

    // Scenario: compute_t1_t2 with multiple addresses uses the shortest preferred_lft
    #[test]
    fn test_compute_t1_t2_multiple_addresses_uses_shortest_preferred_lft() {
        let addrs = vec![
            make_addr("2001:db8::1", 3600, 86400),
            make_addr("2001:db8::2", 7200, 86400),
        ];
        let (t1, t2) = Dhcpv6Lease::compute_t1_t2(&addrs, 0, 0);
        assert_eq!(t1, 1800, "t1 must use shortest preferred_lft (3600/2=1800)");
        assert_eq!(t2, 2880, "t2 must use shortest preferred_lft (3600*4/5=2880)");
    }

    // Scenario: Multiple addresses in IA_NA — lease contains both with correct lifetimes
    #[test]
    fn test_multiple_addresses_in_lease() {
        let addrs = vec![
            make_addr("2001:db8::100", 14400, 86400),
            make_addr("2001:db8::200", 7200, 43200),
        ];
        let lease = make_stateful_lease(addrs, 3600, 5760);
        assert_eq!(lease.addresses.len(), 2, "lease must contain two addresses");
        assert_eq!(
            lease.addresses[0].address,
            "2001:db8::100".parse::<Ipv6Addr>().unwrap()
        );
        assert_eq!(lease.addresses[0].preferred_lft, 14400);
        assert_eq!(lease.addresses[0].valid_lft, 86400);
        assert_eq!(
            lease.addresses[1].address,
            "2001:db8::200".parse::<Ipv6Addr>().unwrap()
        );
        assert_eq!(lease.addresses[1].preferred_lft, 7200);
        assert_eq!(lease.addresses[1].valid_lft, 43200);
    }

    // Scenario: is_expired uses minimum valid_lft across multiple addresses
    #[test]
    fn test_stateful_lease_expired_uses_minimum_valid_lft() {
        let addrs = vec![
            make_addr("2001:db8::1", 3600, 0),     // valid_lft=0: already expired
            make_addr("2001:db8::2", 7200, 86400), // valid_lft=86400: not expired
        ];
        let lease = make_stateful_lease(addrs, 1800, 2880);
        assert!(
            lease.is_expired(),
            "lease must be expired when minimum valid_lft=0"
        );
    }

    // Scenario: info_refresh_time is stored in stateless lease
    #[test]
    fn test_stateless_lease_stores_info_refresh_time() {
        let lease = make_stateless_lease(vec![], vec![], Some(900));
        assert_eq!(
            lease.info_refresh_time,
            Some(900),
            "stateless lease must store info_refresh_time"
        );
    }
}

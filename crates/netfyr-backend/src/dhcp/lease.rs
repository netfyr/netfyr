//! DHCPv4 lease data type with computed timer properties.
//!
//! This module is pure data — no I/O, no async, no external state. All methods
//! are derived from the lease parameters and elapsed time since acquisition.

use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

/// A DHCPv4 lease acquired from a DHCP server.
#[derive(Debug, Clone)]
pub struct DhcpLease {
    /// The IP address assigned to the client.
    pub ip: Ipv4Addr,
    /// The subnet mask provided by the server (e.g., 255.255.255.0).
    pub subnet_mask: Ipv4Addr,
    /// The default gateway (option 3), if provided.
    pub gateway: Option<Ipv4Addr>,
    /// DNS servers (option 6), if provided.
    pub dns_servers: Vec<Ipv4Addr>,
    /// Total lease duration in seconds (option 51).
    pub lease_time: u32,
    /// T1 renewal time in seconds (option 58). Defaults to lease_time/2.
    pub renewal_time: u32,
    /// T2 rebinding time in seconds (option 59). Defaults to lease_time*7/8.
    pub rebind_time: u32,
    /// The DHCP server's identifier (option 54), used for unicast renewal.
    pub server_id: Ipv4Addr,
    /// When this lease was acquired (used to compute remaining times).
    pub acquired_at: Instant,
}

impl DhcpLease {
    /// Convert the subnet mask to a prefix length (e.g., 255.255.255.0 → 24).
    ///
    /// Counts the number of leading 1-bits in the 32-bit representation of the
    /// subnet mask. A valid subnet mask has only contiguous leading 1s.
    pub fn subnet_mask_to_prefix(&self) -> u8 {
        let mask_u32 = u32::from(self.subnet_mask);
        mask_u32.leading_ones() as u8
    }

    /// Returns `true` if the lease has expired (elapsed time ≥ lease_time).
    pub fn is_expired(&self) -> bool {
        self.acquired_at.elapsed() >= Duration::from_secs(self.lease_time as u64)
    }

    /// Returns the time remaining until T1 (renewal). Returns `Duration::ZERO`
    /// if the renewal time has already passed.
    pub fn time_until_renewal(&self) -> Duration {
        Duration::from_secs(self.renewal_time as u64)
            .saturating_sub(self.acquired_at.elapsed())
    }

    /// Returns the time remaining until T2 (rebinding). Returns `Duration::ZERO`
    /// if the rebinding time has already passed.
    pub fn time_until_rebind(&self) -> Duration {
        Duration::from_secs(self.rebind_time as u64)
            .saturating_sub(self.acquired_at.elapsed())
    }

    /// Returns the time remaining until lease expiry. Returns `Duration::ZERO`
    /// if the lease has already expired.
    pub fn time_until_expiry(&self) -> Duration {
        Duration::from_secs(self.lease_time as u64)
            .saturating_sub(self.acquired_at.elapsed())
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a lease with a configurable subnet mask and acquisition time offset.
    fn make_lease(
        lease_time: u32,
        renewal_time: u32,
        rebind_time: u32,
        acquired_ago_secs: u64,
        subnet_mask: Ipv4Addr,
    ) -> DhcpLease {
        DhcpLease {
            ip: Ipv4Addr::new(10, 0, 1, 50),
            subnet_mask,
            gateway: None,
            dns_servers: vec![],
            lease_time,
            renewal_time,
            rebind_time,
            server_id: Ipv4Addr::new(10, 0, 1, 1),
            acquired_at: Instant::now() - Duration::from_secs(acquired_ago_secs),
        }
    }

    // ── subnet_mask_to_prefix ────────────────────────────────────────────────

    /// Scenario: Subnet mask to prefix conversion — 255.255.255.0 → 24
    #[test]
    fn test_subnet_mask_to_prefix_255_255_255_0_returns_24() {
        let lease = make_lease(3600, 1800, 3150, 0, Ipv4Addr::new(255, 255, 255, 0));
        assert_eq!(lease.subnet_mask_to_prefix(), 24);
    }

    #[test]
    fn test_subnet_mask_to_prefix_255_255_0_0_returns_16() {
        let lease = make_lease(3600, 1800, 3150, 0, Ipv4Addr::new(255, 255, 0, 0));
        assert_eq!(lease.subnet_mask_to_prefix(), 16);
    }

    #[test]
    fn test_subnet_mask_to_prefix_255_0_0_0_returns_8() {
        let lease = make_lease(3600, 1800, 3150, 0, Ipv4Addr::new(255, 0, 0, 0));
        assert_eq!(lease.subnet_mask_to_prefix(), 8);
    }

    #[test]
    fn test_subnet_mask_to_prefix_255_255_255_128_returns_25() {
        let lease = make_lease(3600, 1800, 3150, 0, Ipv4Addr::new(255, 255, 255, 128));
        assert_eq!(lease.subnet_mask_to_prefix(), 25);
    }

    #[test]
    fn test_subnet_mask_to_prefix_all_ones_returns_32() {
        let lease = make_lease(3600, 1800, 3150, 0, Ipv4Addr::new(255, 255, 255, 255));
        assert_eq!(lease.subnet_mask_to_prefix(), 32);
    }

    #[test]
    fn test_subnet_mask_to_prefix_all_zeros_returns_0() {
        let lease = make_lease(3600, 1800, 3150, 0, Ipv4Addr::new(0, 0, 0, 0));
        assert_eq!(lease.subnet_mask_to_prefix(), 0);
    }

    // ── is_expired ───────────────────────────────────────────────────────────

    /// Scenario: DhcpLease expiry detection
    /// Given a DhcpLease with lease_time=3600 acquired 3601 seconds ago
    /// When is_expired() is called
    /// Then it returns true
    #[test]
    fn test_is_expired_returns_true_when_past_lease_time() {
        let lease = make_lease(3600, 1800, 3150, 3601, Ipv4Addr::new(255, 255, 255, 0));
        assert!(
            lease.is_expired(),
            "lease acquired 3601s ago with lease_time=3600 must be expired"
        );
    }

    #[test]
    fn test_is_expired_returns_false_for_fresh_lease() {
        let lease = make_lease(3600, 1800, 3150, 0, Ipv4Addr::new(255, 255, 255, 0));
        assert!(
            !lease.is_expired(),
            "newly acquired lease must not be expired"
        );
    }

    #[test]
    fn test_is_expired_returns_false_just_before_expiry() {
        // acquired 3599s ago, lease_time=3600 → 1 second remaining
        let lease = make_lease(3600, 1800, 3150, 3599, Ipv4Addr::new(255, 255, 255, 0));
        assert!(
            !lease.is_expired(),
            "lease with 1 second remaining must not be expired"
        );
    }

    // ── time_until_renewal ───────────────────────────────────────────────────

    /// time_until_renewal returns Duration::ZERO when T1 has already passed.
    #[test]
    fn test_time_until_renewal_returns_zero_when_t1_passed() {
        // renewal_time=1800, acquired 2000s ago → T1 already passed
        let lease = make_lease(3600, 1800, 3150, 2000, Ipv4Addr::new(255, 255, 255, 0));
        assert_eq!(
            lease.time_until_renewal(),
            Duration::ZERO,
            "time_until_renewal must return ZERO when T1 has passed"
        );
    }

    #[test]
    fn test_time_until_renewal_returns_positive_for_fresh_lease() {
        let lease = make_lease(3600, 1800, 3150, 0, Ipv4Addr::new(255, 255, 255, 0));
        let remaining = lease.time_until_renewal();
        assert!(
            remaining > Duration::ZERO,
            "time_until_renewal must be positive for a fresh lease"
        );
        assert!(
            remaining <= Duration::from_secs(1800),
            "time_until_renewal must not exceed renewal_time"
        );
    }

    // ── time_until_rebind ────────────────────────────────────────────────────

    /// time_until_rebind returns Duration::ZERO when T2 has already passed.
    #[test]
    fn test_time_until_rebind_returns_zero_when_t2_passed() {
        // rebind_time=3150, acquired 3200s ago → T2 already passed
        let lease = make_lease(3600, 1800, 3150, 3200, Ipv4Addr::new(255, 255, 255, 0));
        assert_eq!(
            lease.time_until_rebind(),
            Duration::ZERO,
            "time_until_rebind must return ZERO when T2 has passed"
        );
    }

    #[test]
    fn test_time_until_rebind_returns_positive_for_fresh_lease() {
        let lease = make_lease(3600, 1800, 3150, 0, Ipv4Addr::new(255, 255, 255, 0));
        let remaining = lease.time_until_rebind();
        assert!(
            remaining > Duration::ZERO,
            "time_until_rebind must be positive for a fresh lease"
        );
        assert!(
            remaining <= Duration::from_secs(3150),
            "time_until_rebind must not exceed rebind_time"
        );
    }

    // ── time_until_expiry ────────────────────────────────────────────────────

    #[test]
    fn test_time_until_expiry_returns_zero_when_expired() {
        let lease = make_lease(3600, 1800, 3150, 4000, Ipv4Addr::new(255, 255, 255, 0));
        assert_eq!(
            lease.time_until_expiry(),
            Duration::ZERO,
            "time_until_expiry must return ZERO when lease has expired"
        );
    }

    #[test]
    fn test_time_until_expiry_returns_positive_for_fresh_lease() {
        let lease = make_lease(3600, 1800, 3150, 0, Ipv4Addr::new(255, 255, 255, 0));
        let remaining = lease.time_until_expiry();
        assert!(
            remaining > Duration::ZERO,
            "time_until_expiry must be positive for a fresh lease"
        );
        assert!(
            remaining <= Duration::from_secs(3600),
            "time_until_expiry must not exceed lease_time"
        );
    }

    // ── Timer ordering invariant ─────────────────────────────────────────────

    /// For a fresh standard lease, the timers must satisfy T1 < T2 < lease_time.
    ///
    /// RFC 2131 §4.4.5 requires T1 (renewal) ≤ T2 (rebind) ≤ lease duration.
    /// This test verifies that time_until_renewal < time_until_rebind <
    /// time_until_expiry holds for a freshly acquired lease with typical defaults.
    #[test]
    fn test_timer_ordering_t1_before_t2_before_expiry_for_fresh_lease() {
        // Standard defaults: T1=50%, T2=87.5% of lease_time.
        let lease = make_lease(3600, 1800, 3150, 0, Ipv4Addr::new(255, 255, 255, 0));

        let renewal = lease.time_until_renewal();
        let rebind = lease.time_until_rebind();
        let expiry = lease.time_until_expiry();

        assert!(
            renewal < rebind,
            "T1 (renewal={:?}) must be before T2 (rebind={:?})",
            renewal,
            rebind,
        );
        assert!(
            rebind < expiry,
            "T2 (rebind={:?}) must be before lease expiry ({:?})",
            rebind,
            expiry,
        );
    }

    /// Clone and Debug work correctly on DhcpLease.
    #[test]
    fn test_dhcp_lease_clone_and_debug() {
        let lease = make_lease(3600, 1800, 3150, 0, Ipv4Addr::new(255, 255, 255, 0));
        let cloned = lease.clone();
        assert_eq!(cloned.ip, lease.ip);
        assert_eq!(cloned.lease_time, lease.lease_time);
        assert!(!format!("{:?}", lease).is_empty());
    }

    // ── Lease expiry boundary: 120-second dnsmasq minimum lease ─────────────────

    /// Scenario: Lease with 120-second duration expires at exactly 120 seconds.
    ///
    /// The integration test (407-dhcpv4-lease-expiry.sh) uses a 120-second lease
    /// because that is the minimum dnsmasq lease time. This test verifies that
    /// is_expired() returns true at the exact boundary so the DHCP state machine
    /// correctly transitions to the LeaseExpired path.
    #[test]
    fn test_is_expired_returns_true_at_exactly_120_second_dnsmasq_lease_boundary() {
        // renewal=60 (T1=50%), rebind=105 (T2=87.5%), acquired exactly 120s ago
        let lease = make_lease(120, 60, 105, 120, Ipv4Addr::new(255, 255, 255, 0));
        assert!(
            lease.is_expired(),
            "lease with lease_time=120 acquired exactly 120s ago must be expired"
        );
    }

    /// Scenario: Lease with 120-second duration is NOT expired at 119 seconds.
    ///
    /// One second before the dnsmasq minimum lease expires, the client is still
    /// in the REBINDING state — the address must remain applied.
    #[test]
    fn test_is_expired_returns_false_at_119_seconds_for_120s_dnsmasq_lease() {
        let lease = make_lease(120, 60, 105, 119, Ipv4Addr::new(255, 255, 255, 0));
        assert!(
            !lease.is_expired(),
            "lease with lease_time=120 acquired 119s ago must NOT be expired yet"
        );
    }

    /// Scenario: time_until_expiry returns ZERO for a lease that expired at 120 seconds.
    ///
    /// When the DHCP state machine calls time_until_expiry() and the result is
    /// ZERO, it transitions to LeaseExpired and reverts the factory state to pending.
    #[test]
    fn test_time_until_expiry_returns_zero_for_expired_120s_lease() {
        let lease = make_lease(120, 60, 105, 125, Ipv4Addr::new(255, 255, 255, 0));
        assert_eq!(
            lease.time_until_expiry(),
            Duration::ZERO,
            "expired 120s lease must report ZERO time until expiry"
        );
    }

    /// Timer ordering holds for a 120-second lease with dnsmasq default timers.
    ///
    /// T1=60s (50%), T2=105s (87.5%) — the RFC 2131 ordering T1 < T2 < lease_time
    /// must hold for a freshly acquired 120s lease.
    #[test]
    fn test_timer_ordering_holds_for_120s_dnsmasq_lease() {
        let lease = make_lease(120, 60, 105, 0, Ipv4Addr::new(255, 255, 255, 0));

        let renewal = lease.time_until_renewal();
        let rebind = lease.time_until_rebind();
        let expiry = lease.time_until_expiry();

        assert!(
            renewal < rebind,
            "T1 (renewal={:?}) must be before T2 (rebind={:?}) for 120s lease",
            renewal,
            rebind,
        );
        assert!(
            rebind < expiry,
            "T2 (rebind={:?}) must be before expiry ({:?}) for 120s lease",
            rebind,
            expiry,
        );
    }
}

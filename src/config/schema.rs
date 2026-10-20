//! Configuration structs — 1:1 mirror of Go `config_manager` package.
//!
//! These structs serialize/deserialize to the exact same JSON as the Go
//! binary. Field names, casing, and `omitempty` behavior must match.

use serde::{Deserialize, Serialize};

// ── Main Config ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub config_version: String,
    #[serde(default = "default_log_level")]
    pub log_level: String,
    #[serde(default)]
    pub accepted_mints: Vec<MintConfig>,
    #[serde(default)]
    pub profit_share: Vec<ProfitShareConfig>,
    #[serde(default)]
    pub step_size: u64,
    #[serde(default)]
    pub margin: Option<f64>,
    #[serde(default = "default_metric")]
    pub metric: String,
    #[serde(default)]
    pub show_setup: bool,
    #[serde(default)]
    pub reseller_mode: bool,
    #[serde(default)]
    pub redirect_url: Option<String>,
    #[serde(default)]
    pub auth_delay_seconds: Option<i32>,
    #[serde(default)]
    pub upstream_detector: UpstreamDetectorConfig,
    #[serde(default)]
    pub upstream_session_manager: UpstreamSessionManagerConfig,
    #[serde(default)]
    pub upstream_wifi: UpstreamWifiConfig,
    /// ADR-0007 §3(a): identity is the TCP peer (default, the safe one). Set to
    /// false ONLY when a proxy fronts the module and is responsible for
    /// overwriting the identity headers — then the header is trusted by contract.
    #[serde(default = "default_identity_from_socket")]
    pub identity_from_socket: bool,
}

fn default_identity_from_socket() -> bool {
    true
}

fn default_log_level() -> String {
    "info".to_string()
}

fn default_metric() -> String {
    "bytes".to_string()
}

impl Default for Config {
    fn default() -> Self {
        Self::new_default()
    }
}

impl Config {
    pub fn new_default() -> Self {
        Config {
            config_version: "v0.0.8".to_string(),
            log_level: "info".to_string(),
            accepted_mints: vec![MintConfig::default_production("https://mint.coinos.io")],
            profit_share: vec![
                ProfitShareConfig {
                    factor: 0.79,
                    identity: "owner".to_string(),
                },
                ProfitShareConfig {
                    factor: 0.07,
                    identity: "c08r4d0r".to_string(),
                },
                ProfitShareConfig {
                    factor: 0.07,
                    identity: "amperstrand".to_string(),
                },
                ProfitShareConfig {
                    factor: 0.07,
                    identity: "origami74".to_string(),
                },
            ],
            step_size: 22020096, // 21 MiB
            margin: Some(0.1),
            metric: "bytes".to_string(),
            show_setup: true,
            reseller_mode: false,
            redirect_url: None,
            auth_delay_seconds: None,
            upstream_detector: UpstreamDetectorConfig::default(),
            upstream_session_manager: UpstreamSessionManagerConfig::default(),
            upstream_wifi: UpstreamWifiConfig::default(),
            identity_from_socket: true,
        }
    }

    /// Validate that profit_share factors sum to ~1.0.
    pub fn validate_profit_share(&self) -> Result<(), String> {
        if self.profit_share.is_empty() {
            return Err("profit_share is empty: at least one entry required".to_string());
        }
        let sum: f64 = self.profit_share.iter().map(|p| p.factor).sum();
        if (sum - 1.0).abs() > 1e-6 {
            return Err(format!(
                "profit_share factors must sum to 1.0, got {} ({:.1}% will remain in wallet each payout cycle)",
                sum,
                (1.0 - sum) * 100.0
            ));
        }
        Ok(())
    }
}

// ── MintConfig ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MintConfig {
    pub url: String,
    #[serde(default)]
    pub min_balance: u64,
    #[serde(default)]
    pub balance_tolerance_percent: u64,
    #[serde(default)]
    pub payout_interval_seconds: u64,
    #[serde(default)]
    pub min_payout_amount: u64,
    #[serde(default = "default_price_per_step")]
    pub price_per_step: u64,
    #[serde(default = "default_price_unit")]
    pub price_unit: String,
    #[serde(default, rename = "purchase_min_steps")]
    pub min_purchase_steps: u64,
}

fn default_price_per_step() -> u64 {
    1
}
fn default_price_unit() -> String {
    "sats".to_string()
}

impl MintConfig {
    pub fn default_production(url: &str) -> Self {
        MintConfig {
            url: url.to_string(),
            min_balance: 64,
            balance_tolerance_percent: 10,
            payout_interval_seconds: 60,
            min_payout_amount: 128,
            price_per_step: 1,
            price_unit: "sats".to_string(),
            min_purchase_steps: 0,
        }
    }
}

// ── ProfitShareConfig ────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProfitShareConfig {
    pub factor: f64,
    pub identity: String,
}

// ── UpstreamDetectorConfig ───────────────────────────────────────────
/// Duration fields use Go's string format ("10s", "2s", "5m0s").
/// We keep them as strings for 1:1 JSON compat.

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpstreamDetectorConfig {
    #[serde(default = "default_probe_timeout")]
    pub probe_timeout: String,
    #[serde(default = "default_probe_retry_count")]
    pub probe_retry_count: i32,
    #[serde(default = "default_probe_retry_delay")]
    pub probe_retry_delay: String,
    #[serde(default)]
    pub require_valid_signature: bool,
    #[serde(default = "default_ignore_interfaces")]
    pub ignore_interfaces: Vec<String>,
    #[serde(default)]
    pub only_interfaces: Vec<String>,
    #[serde(default = "default_discovery_timeout")]
    pub discovery_timeout: String,
}

fn default_probe_timeout() -> String {
    "10s".to_string()
}
fn default_probe_retry_count() -> i32 {
    3
}
fn default_probe_retry_delay() -> String {
    "2s".to_string()
}
fn default_ignore_interfaces() -> Vec<String> {
    vec![
        "lo".into(),
        "docker0".into(),
        "br-lan".into(),
        "hostap0".into(),
    ]
}
fn default_discovery_timeout() -> String {
    "5m0s".to_string()
}

impl Default for UpstreamDetectorConfig {
    fn default() -> Self {
        UpstreamDetectorConfig {
            probe_timeout: default_probe_timeout(),
            probe_retry_count: default_probe_retry_count(),
            probe_retry_delay: default_probe_retry_delay(),
            require_valid_signature: true,
            ignore_interfaces: default_ignore_interfaces(),
            only_interfaces: vec![],
            discovery_timeout: default_discovery_timeout(),
        }
    }
}

// ── UpstreamSessionManagerConfig ─────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpstreamSessionManagerConfig {
    #[serde(default)]
    pub max_price_per_millisecond: f64,
    #[serde(default)]
    pub max_price_per_byte: f64,
    #[serde(default)]
    pub trust: TrustConfig,
    #[serde(default)]
    pub sessions: SessionConfig,
    #[serde(default)]
    pub usage_tracking: UsageTrackingConfig,
}

impl Default for UpstreamSessionManagerConfig {
    fn default() -> Self {
        UpstreamSessionManagerConfig {
            max_price_per_millisecond: 0.002777777778,
            max_price_per_byte: 0.00003725782414,
            trust: TrustConfig::default(),
            sessions: SessionConfig::default(),
            usage_tracking: UsageTrackingConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrustConfig {
    #[serde(default = "default_trust_policy")]
    pub default_policy: String,
    #[serde(default)]
    pub allowlist: Vec<String>,
    #[serde(default)]
    pub blocklist: Vec<String>,
}

fn default_trust_policy() -> String {
    "trust_all".to_string()
}

impl Default for TrustConfig {
    fn default() -> Self {
        TrustConfig {
            default_policy: default_trust_policy(),
            allowlist: vec![],
            blocklist: vec![],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionConfig {
    #[serde(default = "default_session_inc_ms")]
    pub preferred_session_increments_milliseconds: u64,
    #[serde(default = "default_session_inc_bytes")]
    pub preferred_session_increments_bytes: u64,
    #[serde(default = "default_ms_renewal_offset")]
    pub millisecond_renewal_offset: u64,
    #[serde(default = "default_bytes_renewal_offset")]
    pub bytes_renewal_offset: u64,
}

fn default_session_inc_ms() -> u64 {
    60000
}
fn default_session_inc_bytes() -> u64 {
    131100000
}
fn default_ms_renewal_offset() -> u64 {
    10000
}
fn default_bytes_renewal_offset() -> u64 {
    131100000
}

impl Default for SessionConfig {
    fn default() -> Self {
        SessionConfig {
            preferred_session_increments_milliseconds: default_session_inc_ms(),
            preferred_session_increments_bytes: default_session_inc_bytes(),
            millisecond_renewal_offset: default_ms_renewal_offset(),
            bytes_renewal_offset: default_bytes_renewal_offset(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageTrackingConfig {
    #[serde(default = "default_data_monitor_interval")]
    pub data_monitoring_interval: String,
}

fn default_data_monitor_interval() -> String {
    "0.5s".to_string()
}

impl Default for UsageTrackingConfig {
    fn default() -> Self {
        UsageTrackingConfig {
            data_monitoring_interval: default_data_monitor_interval(),
        }
    }
}

// ── UpstreamWifiConfig ───────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpstreamWifiConfig {
    #[serde(default = "default_scan_interval")]
    pub scan_interval_seconds: i32,
    #[serde(default = "default_fast_check")]
    pub fast_check_seconds: i32,
    #[serde(default = "default_lost_threshold")]
    pub lost_threshold: i32,
    #[serde(default = "default_hysteresis_db")]
    pub hysteresis_db: i32,
    #[serde(default = "default_signal_floor")]
    pub signal_floor: i32,
    #[serde(default = "default_blacklist_ttl")]
    pub blacklist_ttl_minutes: i32,
    #[serde(default = "default_emergency_penalty")]
    pub emergency_penalty: i32,
    #[serde(default = "default_max_failures")]
    pub max_consecutive_failures: i32,
    #[serde(default = "default_switch_cooldown")]
    pub switch_cooldown_minutes: i32,
    #[serde(default = "default_startup_grace")]
    pub startup_grace_seconds: i32,
    #[serde(default = "default_post_switch_wait")]
    pub post_switch_wait_seconds: i32,
    #[serde(default = "default_dhcp_timeout")]
    pub dhcp_timeout_seconds: i32,
    #[serde(default = "default_manual_pause")]
    pub manual_pause_seconds: i32,
}

fn default_scan_interval() -> i32 {
    300
}
fn default_fast_check() -> i32 {
    30
}
fn default_lost_threshold() -> i32 {
    2
}
fn default_hysteresis_db() -> i32 {
    12
}
fn default_signal_floor() -> i32 {
    -85
}
fn default_blacklist_ttl() -> i32 {
    60
}
fn default_emergency_penalty() -> i32 {
    20
}
fn default_max_failures() -> i32 {
    3
}
fn default_switch_cooldown() -> i32 {
    10
}
fn default_startup_grace() -> i32 {
    90
}
fn default_post_switch_wait() -> i32 {
    5
}
fn default_dhcp_timeout() -> i32 {
    180
}
fn default_manual_pause() -> i32 {
    120
}

impl Default for UpstreamWifiConfig {
    fn default() -> Self {
        UpstreamWifiConfig {
            scan_interval_seconds: default_scan_interval(),
            fast_check_seconds: default_fast_check(),
            lost_threshold: default_lost_threshold(),
            hysteresis_db: default_hysteresis_db(),
            signal_floor: default_signal_floor(),
            blacklist_ttl_minutes: default_blacklist_ttl(),
            emergency_penalty: default_emergency_penalty(),
            max_consecutive_failures: default_max_failures(),
            switch_cooldown_minutes: default_switch_cooldown(),
            startup_grace_seconds: default_startup_grace(),
            post_switch_wait_seconds: default_post_switch_wait(),
            dhcp_timeout_seconds: default_dhcp_timeout(),
            manual_pause_seconds: default_manual_pause(),
        }
    }
}

// ── IdentitiesConfig ─────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdentitiesConfig {
    #[serde(default)]
    pub config_version: String,
    #[serde(default)]
    pub owned_identities: Vec<OwnedIdentity>,
    #[serde(default)]
    pub public_identities: Vec<PublicIdentity>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OwnedIdentity {
    pub name: String,
    #[serde(rename = "privatekey")]
    pub privatekey: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PublicIdentity {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pubkey: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lightning_address: Option<String>,
}

// ── InstallConfig (install.json) ─────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InstallConfig {
    #[serde(default)]
    pub config_version: String,
    #[serde(default)]
    pub package_path: String,
    #[serde(default)]
    pub ip_address_randomized: bool,
    #[serde(default)]
    pub install_time: u64,
    #[serde(default)]
    pub download_time: u64,
    #[serde(default)]
    pub release_channel: String,
    #[serde(default)]
    pub ensure_default_timestamp: u64,
    #[serde(default)]
    pub installed_version: String,
}

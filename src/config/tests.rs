//! Tests for the config loader.
//!
//! Round-trip the exact Go `Config`, `IdentitiesConfig`, and `InstallConfig`
//! schemas so the same `/etc/tollgate/*.json` files load without modification.

use super::schema::{
    Config, MintConfig, OwnedIdentity, ProfitShareConfig, PublicIdentity, UpstreamDetectorConfig,
    UpstreamSessionManagerConfig, UpstreamWifiConfig,
};
use super::{
    config_dir, config_path, identities_path, install_path, load_config, load_identities,
    load_install,
};
use std::fs;

const PRODUCTION_CONFIG_JSON: &str = r#"{
  "config_version": "v0.0.8",
  "log_level": "info",
  "accepted_mints": [
    {
      "url": "https://mint.coinos.io",
      "min_balance": 64,
      "balance_tolerance_percent": 10,
      "payout_interval_seconds": 60,
      "min_payout_amount": 128,
      "price_per_step": 1,
      "price_unit": "sats",
      "purchase_min_steps": 0
    }
  ],
  "profit_share": [
    {"factor": 0.79, "identity": "owner"},
    {"factor": 0.21, "identity": "c08r4d0r"}
  ],
  "step_size": 22020096,
  "margin": 0.1,
  "metric": "bytes",
  "show_setup": true,
  "reseller_mode": false,
  "upstream_detector": {
    "probe_timeout": "10s",
    "probe_retry_count": 3,
    "probe_retry_delay": "2s",
    "require_valid_signature": true,
    "ignore_interfaces": ["lo", "docker0", "br-lan", "hostap0"],
    "only_interfaces": [],
    "discovery_timeout": "5m0s"
  },
  "upstream_session_manager": {
    "max_price_per_millisecond": 0.002777777778,
    "max_price_per_byte": 0.00003725782414,
    "trust": {"default_policy": "trust_all", "allowlist": [], "blocklist": []},
    "sessions": {
      "preferred_session_increments_milliseconds": 60000,
      "preferred_session_increments_bytes": 131100000,
      "millisecond_renewal_offset": 10000,
      "bytes_renewal_offset": 131100000
    },
    "usage_tracking": {"data_monitoring_interval": "500ms"}
  },
  "upstream_wifi": {
    "scan_interval_seconds": 300, "fast_check_seconds": 30, "lost_threshold": 2,
    "hysteresis_db": 12, "signal_floor": -85, "blacklist_ttl_minutes": 60,
    "emergency_penalty": 20, "max_consecutive_failures": 3, "switch_cooldown_minutes": 10,
    "startup_grace_seconds": 90, "post_switch_wait_seconds": 5, "dhcp_timeout_seconds": 180,
    "manual_pause_seconds": 120
  }
}
"#;

const IDENTITIES_JSON: &str = r#"{
  "config_version": "v0.0.1",
  "owned_identities": [
    {"name": "merchant", "privatekey": "0000000000000000000000000000000000000000000000000000000000000001"}
  ],
  "public_identities": [
    {"name": "owner", "pubkey": "abc", "lightning_address": "owner@example.com"}
  ]
}
"#;

const INSTALL_JSON: &str = r#"{
  "config_version": "v0.0.2",
  "package_path": "false",
  "ip_address_randomized": false,
  "install_time": 0,
  "download_time": 0,
  "release_channel": "stable",
  "ensure_default_timestamp": 1234567890,
  "installed_version": "0.0.0"
}
"#;

fn rand_hex() -> String {
    use std::time::SystemTime;
    let nanos = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .subsec_nanos();
    format!("{nanos:x}")
}

/// Serializes every test that touches `TOLLGATE_TEST_CONFIG_DIR`.
///
/// WHY: the config path is resolved from a PROCESS-GLOBAL environment variable, and cargo
/// runs tests in the same process on many threads. Six tests here set and unset it
/// concurrently, so one test's `remove_var` lands in the middle of another's assertions --
/// measured as intermittent failures in `loads_identities` and
/// `default_config_path_is_etc_tollgate` (~2 runs in 12).
///
/// A lock is the correct fix rather than retries: the shared mutable state is real, and no
/// amount of retrying makes concurrent mutation of one env var safe.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn env_guard() -> std::sync::MutexGuard<'static, ()> {
    // A poisoned lock only means some other test panicked; the env var itself is still
    // usable, and failing here would replace one flake with another.
    ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// NOTE: this deliberately does NOT take `env_guard`. Callers must hold it, because
/// `std::sync::Mutex` is not reentrant: when both the helper and its caller locked,
/// every `loads_*` test blocked forever and the suite hung (measured 2026-10-20).
/// The lock is taken at exactly one level — the test — and this helper just mutates.
fn with_test_dir(files: &[(&str, &str)]) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "tollgate-test-{}-{}",
        std::process::id(),
        rand_hex()
    ));
    fs::create_dir_all(&dir).expect("create test dir");
    for (name, contents) in files {
        fs::write(dir.join(name), contents).expect("write file");
    }
    std::env::set_var("TOLLGATE_TEST_CONFIG_DIR", &dir);
    dir
}

#[test]
fn default_config_path_is_etc_tollgate() {
    let _g = env_guard();
    std::env::remove_var("TOLLGATE_TEST_CONFIG_DIR");
    assert_eq!(
        config_path(),
        std::path::Path::new("/etc/tollgate/config.json")
    );
    assert_eq!(
        install_path(),
        std::path::Path::new("/etc/tollgate/install.json")
    );
    assert_eq!(
        identities_path(),
        std::path::Path::new("/etc/tollgate/identities.json")
    );
}

#[test]
fn config_path_honors_test_config_dir() {
    let _g = env_guard();
    let dir = std::env::temp_dir().join(format!("tollgate-cfg-test-{}", rand_hex()));
    fs::create_dir_all(&dir).unwrap();
    std::env::set_var("TOLLGATE_TEST_CONFIG_DIR", &dir);
    assert_eq!(config_path(), dir.join("config.json"));
    assert_eq!(install_path(), dir.join("install.json"));
    assert_eq!(identities_path(), dir.join("identities.json"));
    std::env::remove_var("TOLLGATE_TEST_CONFIG_DIR");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn config_dir_reports_correct_directory() {
    let _g = env_guard();
    std::env::remove_var("TOLLGATE_TEST_CONFIG_DIR");
    assert_eq!(config_dir(), std::path::Path::new("/etc/tollgate"));
    let dir = std::env::temp_dir().join(format!("tollgate-cfg-test-{}", rand_hex()));
    fs::create_dir_all(&dir).unwrap();
    std::env::set_var("TOLLGATE_TEST_CONFIG_DIR", &dir);
    assert_eq!(config_dir(), dir.as_path());
    std::env::remove_var("TOLLGATE_TEST_CONFIG_DIR");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn loads_full_production_config() {
    // with_test_dir mutates the process-global TOLLGATE_TEST_CONFIG_DIR; without the
    // guard this test raced the other env-mutating tests and flaked ~1 in 15 runs.
    let _g = env_guard();
    with_test_dir(&[
        ("config.json", PRODUCTION_CONFIG_JSON),
        ("identities.json", IDENTITIES_JSON),
        ("install.json", INSTALL_JSON),
    ]);

    let cfg = load_config()
        .expect("config should load")
        .expect("config should be Some");

    assert_eq!(cfg.config_version, "v0.0.8");
    assert_eq!(cfg.log_level, "info");
    assert_eq!(cfg.step_size, 22020096);
    assert_eq!(cfg.metric, "bytes");
    assert!(!cfg.reseller_mode);
    assert!(cfg.show_setup);
    assert_eq!(cfg.margin, Some(0.1));

    assert_eq!(cfg.accepted_mints.len(), 1);
    let m: &MintConfig = &cfg.accepted_mints[0];
    assert_eq!(m.url, "https://mint.coinos.io");
    assert_eq!(m.min_balance, 64);
    assert_eq!(m.balance_tolerance_percent, 10);
    assert_eq!(m.payout_interval_seconds, 60);
    assert_eq!(m.min_payout_amount, 128);
    assert_eq!(m.price_per_step, 1);
    assert_eq!(m.price_unit, "sats");
    assert_eq!(m.min_purchase_steps, 0);

    assert_eq!(cfg.profit_share.len(), 2);
    assert_eq!(
        cfg.profit_share[0],
        ProfitShareConfig {
            factor: 0.79,
            identity: "owner".into()
        }
    );
    assert_eq!(
        cfg.profit_share[1],
        ProfitShareConfig {
            factor: 0.21,
            identity: "c08r4d0r".into()
        }
    );

    assert_eq!(cfg.upstream_detector.probe_timeout, "10s");
    assert_eq!(cfg.upstream_detector.probe_retry_count, 3);
    assert!(cfg.upstream_detector.require_valid_signature);
    assert_eq!(
        cfg.upstream_detector.ignore_interfaces,
        vec!["lo", "docker0", "br-lan", "hostap0"]
    );

    let usm: &UpstreamSessionManagerConfig = &cfg.upstream_session_manager;
    assert!((usm.max_price_per_millisecond - 0.002777777778).abs() < 1e-12);
    assert_eq!(usm.trust.default_policy, "trust_all");
    assert_eq!(
        usm.sessions.preferred_session_increments_milliseconds,
        60000
    );
    assert_eq!(usm.usage_tracking.data_monitoring_interval, "500ms");

    let wifi: &UpstreamWifiConfig = &cfg.upstream_wifi;
    assert_eq!(wifi.scan_interval_seconds, 300);
    assert_eq!(wifi.signal_floor, -85);
    assert_eq!(wifi.manual_pause_seconds, 120);

    // Round-trip
    let json = serde_json::to_string(&cfg).expect("serialise");
    let cfg2: Config = serde_json::from_str(&json).expect("deserialise");
    assert_eq!(cfg2.config_version, cfg.config_version);
    assert_eq!(cfg2.accepted_mints.len(), cfg.accepted_mints.len());
}

#[test]
fn missing_config_file_returns_none() {
    let _g = env_guard();
    let dir = std::env::temp_dir().join(format!("tollgate-missing-{}", rand_hex()));
    fs::create_dir_all(&dir).unwrap();
    std::env::set_var("TOLLGATE_TEST_CONFIG_DIR", &dir);
    let loaded = load_config().expect("missing file should not error");
    assert!(loaded.is_none());
    std::env::remove_var("TOLLGATE_TEST_CONFIG_DIR");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn empty_config_file_returns_none() {
    let _g = env_guard();
    let dir = std::env::temp_dir().join(format!("tollgate-empty-{}", rand_hex()));
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("config.json"), "").unwrap();
    std::env::set_var("TOLLGATE_TEST_CONFIG_DIR", &dir);
    let loaded = load_config().expect("empty file should not error");
    assert!(loaded.is_none());
    std::env::remove_var("TOLLGATE_TEST_CONFIG_DIR");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn loads_identities() {
    let _g = env_guard();
    with_test_dir(&[("identities.json", IDENTITIES_JSON)]);
    let ids = load_identities()
        .expect("identities should load")
        .expect("Some");
    assert_eq!(ids.config_version, "v0.0.1");
    assert_eq!(ids.owned_identities.len(), 1);
    assert_eq!(
        ids.owned_identities[0],
        OwnedIdentity {
            name: "merchant".into(),
            privatekey: "0000000000000000000000000000000000000000000000000000000000000001".into()
        }
    );
    assert_eq!(ids.public_identities.len(), 1);
    assert_eq!(
        ids.public_identities[0],
        PublicIdentity {
            name: "owner".into(),
            pubkey: Some("abc".into()),
            lightning_address: Some("owner@example.com".into())
        }
    );
}

#[test]
fn loads_install_config() {
    let _g = env_guard();
    with_test_dir(&[("install.json", INSTALL_JSON)]);
    let inst = load_install().expect("install should load").expect("Some");
    assert_eq!(inst.config_version, "v0.0.2");
    assert_eq!(inst.package_path, "false");
    assert!(!inst.ip_address_randomized);
    assert_eq!(inst.release_channel, "stable");
    assert_eq!(inst.ensure_default_timestamp, 1234567890);
    assert_eq!(inst.installed_version, "0.0.0");
}

#[test]
fn deserializes_upstream_detector_and_session_defaults() {
    let minimal = r#"{
        "config_version": "v0.0.8", "log_level": "info", "metric": "bytes",
        "step_size": 1024, "show_setup": false, "reseller_mode": false,
        "upstream_detector": {"probe_timeout": "10s", "probe_retry_count": 1, "probe_retry_delay": "1s",
            "require_valid_signature": false, "discovery_timeout": "1m0s"},
        "upstream_session_manager": {"max_price_per_millisecond": 0.001, "max_price_per_byte": 0.0001,
            "trust": {"default_policy": "trust_none"},
            "sessions": {"preferred_session_increments_milliseconds": 1000, "preferred_session_increments_bytes": 1000,
                         "millisecond_renewal_offset": 100, "bytes_renewal_offset": 100},
            "usage_tracking": {"data_monitoring_interval": "1s"}},
        "upstream_wifi": {"scan_interval_seconds": 60}
    }"#;
    let cfg: Config = serde_json::from_str(minimal).expect("minimal config should parse");
    let _: &UpstreamDetectorConfig = &cfg.upstream_detector;
    let _: &UpstreamWifiConfig = &cfg.upstream_wifi;
}

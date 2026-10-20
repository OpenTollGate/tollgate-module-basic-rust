//! Identity management — load/generate secp256k1 keypairs for Nostr signing.
//!
//! Mirrors Go's identities.json model. On first run, generates a merchant
//! keypair and stores it. On subsequent runs, loads from disk. File mode 0600.

use crate::config::schema::{IdentitiesConfig, OwnedIdentity};
use secp256k1::{Secp256k1, SecretKey};

/// A Nostr keypair for event signing.
#[derive(Debug, Clone)]
pub struct MerchantIdentity {
    pub name: String,
    pub secret_key: SecretKey,
}

impl MerchantIdentity {
    /// Load the merchant identity from identities.json, or generate a new one.
    pub fn load_or_generate() -> Result<Self, String> {
        let identities = crate::config::load_identities();

        if let Ok(Some(config)) = identities {
            // Go parity: owned_identities[0] IS the merchant key, regardless of
            // its name field. Requiring name=="merchant" made a nameless file
            // silently fork identity (Rust generated a new key and overwrote the
            // file while Go kept reading [0]) — measured on hardware, fixed here.
            if let Some(owned) = config.owned_identities.first() {
                let secret_key = SecretKey::from_str(&owned.privatekey)
                    .map_err(|e| format!("invalid merchant private key: {e}"))?;
                return Ok(MerchantIdentity {
                    name: "merchant".to_string(),
                    secret_key,
                });
            }
        }

        // Generate new keypair
        let secp = Secp256k1::new();
        let (secret_key, public_key) = secp.generate_keypair(&mut rand::thread_rng());
        let pub_hex = public_key.to_string();

        tracing::info!(pubkey = %pub_hex, "generated new merchant identity");

        // Save to identities.json
        let new_config = IdentitiesConfig {
            config_version: "v0.0.1".to_string(),
            owned_identities: vec![OwnedIdentity {
                name: "merchant".to_string(),
                privatekey: secret_key.display_secret().to_string(),
            }],
            public_identities: vec![],
        };

        let json = serde_json::to_string_pretty(&new_config)
            .map_err(|e| format!("serialize identities: {e}"))?;

        let path = crate::config::identities_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::write(&path, json).map_err(|e| e.to_string())?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .map_err(|e| e.to_string())?;
        }

        Ok(MerchantIdentity {
            name: "merchant".to_string(),
            secret_key,
        })
    }

    /// Get the public key as hex.
    pub fn pubkey_hex(&self) -> String {
        let secp = Secp256k1::new();
        let pubkey = secp256k1::PublicKey::from_secret_key(&secp, &self.secret_key);
        pubkey.x_only_public_key().0.to_string()
    }
}

// Re-export SecretKey for convenience
use std::str::FromStr;
#[cfg(test)]
mod go_parity_tests {
    use super::*;

    /// Regression (measured on hardware): a nameless identities.json must still
    /// load owned_identities[0] as the merchant key — Go's semantics. Requiring
    /// name=="merchant" silently forked identity between the two modules (Rust
    /// regenerated and OVERWROTE the file while Go kept reading [0]).
    #[test]
    fn nameless_owned_identities_loads_first_entry() {
        let dir = std::env::temp_dir().join(format!("tg-idn-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _g = crate::config::tests::env_lock_for_external();
        std::env::set_var("TOLLGATE_TEST_CONFIG_DIR", &dir);
        // nameless entry with a FIXED key
        std::fs::write(
            dir.join("identities.json"),
            r#"{"owned_identities":[{"privatekey":"d1a1ff3ac0ffee00000000000000000000000000000000000000000000000000"}]}"#,
        )
        .unwrap();
        let id = MerchantIdentity::load_or_generate().expect("loads");
        let key = id.secret_key.display_secret().to_string();
        assert_eq!(
            key,
            "d1a1ff3ac0ffee00000000000000000000000000000000000000000000000000"
        );
        // and the file was NOT overwritten
        let raw = std::fs::read_to_string(dir.join("identities.json")).unwrap();
        assert!(
            raw.contains("d1a1ff3ac0ffee"),
            "file must not be rewritten on load"
        );
        std::env::remove_var("TOLLGATE_TEST_CONFIG_DIR");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

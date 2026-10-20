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
            if let Some(owned) = config
                .owned_identities
                .iter()
                .find(|o| o.name == "merchant")
            {
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

//! Nostr event creation and signing.
//!
//! Minimal implementation: creates and signs NIP-01 events using secp256k1.
//! Only what we need for kind 10021 discovery events in Phase 1.

use hex;
use secp256k1::{Keypair, Message, Secp256k1, SecretKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// A Nostr event (NIP-01).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NostrEvent {
    pub id: String,
    pub pubkey: String,
    pub created_at: u64,
    pub kind: u32,
    pub tags: Vec<Vec<String>>,
    pub content: String,
    pub sig: String,
}

/// Compute the NIP-01 event id for an already-built event.
///
/// The id is `SHA256(JSON([0, pubkey, created_at, kind, tags, content]))`.
/// Exposed so callers (and tests) can verify an event's id independently of how
/// it was produced.
pub fn compute_id(event: &NostrEvent) -> String {
    let id_array = serde_json::json!([
        0,
        event.pubkey,
        event.created_at,
        event.kind,
        event.tags,
        event.content
    ]);
    let id_str = serde_json::to_string(&id_array).unwrap_or_default();
    hex::encode(Sha256::digest(id_str.as_bytes()))
}

/// Create and sign a Nostr event.
pub fn create_event(
    kind: u32,
    tags: Vec<Vec<String>>,
    content: &str,
    secret_key: &SecretKey,
) -> NostrEvent {
    let secp = Secp256k1::new();
    let keypair = Keypair::from_secret_key(&secp, secret_key);
    let pubkey = keypair.public_key();
    let pubkey_hex = pubkey.x_only_public_key().0.to_string();

    let created_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    // Compute event ID = SHA256 of serialized array: [0, pubkey, created_at, kind, tags, content]
    let mut event = NostrEvent {
        id: String::new(),
        pubkey: pubkey_hex,
        created_at,
        kind,
        tags,
        content: content.to_string(),
        sig: String::new(),
    };
    event.id = compute_id(&event);
    let id_hash = Sha256::digest(hex::decode(&event.id).unwrap_or_default());

    // Sign the ID hash
    let msg = Message::from_digest(id_hash.into());
    let sig = secp.sign_schnorr_no_aux_rand(&msg, &keypair);
    let sig_hex = sig.to_string();

    event.sig = sig_hex;
    event
}

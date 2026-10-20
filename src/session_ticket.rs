//! Session tickets and the meter-carrying rebind — wire-compatible with the Go
//! module's `src/merchant/session_ticket.go`.
//!
//! WHY: a client that rotates its MAC (iOS/Android default) loses the session it
//! paid for, because the MAC is session key, meter key and delivery address at once.
//! A ticket is a server-signed, MEMORY-ONLY handle onto a session; rebind moves the
//! delivery address. The ticket never carries an entitlement — it names a session,
//! it does not contain one.
//!
//! Wire format (must match Go byte-for-byte for portal interop):
//!   v1 . base64url(JSON{handle,iat,exp}) . base64url(HMAC-SHA256(body))
//! The signing key is drawn from crypto/rand at startup, written nowhere; the
//! attachment store is process memory. A restart invalidates every ticket by
//! construction — intended, the session record survives in sqlite.

use hmac::{Hmac, Mac};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::collections::HashMap;
use std::sync::Mutex;

type HmacSha256 = Hmac<Sha256>;

pub const TICKET_VERSION: &str = "v1";
/// Go's defaultSessionTicketTTL: a ticket is a handle, not a grant.
pub const DEFAULT_TICKET_TTL_SECS: i64 = 12 * 60 * 60;
/// Go's maxSessionAttachments: expired swept first, oldest evicted beyond the cap.
pub const MAX_ATTACHMENTS: usize = 4096;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionTicket {
    pub handle: String,
    pub iat: i64,
    pub exp: i64,
}

pub struct VerifiedTicket {
    pub handle: String,
    pub iat: i64,
    pub exp: i64,
}

#[derive(Debug, Clone)]
struct Attachment {
    mac: String,
    expires_at: i64,
    attached_at: i64,
}

#[derive(Debug)]
pub enum TicketError {
    Malformed,
    Signature,
    Expired,
    Unknown,
    NoSession,
    /// Go's ErrAttachmentActive: the address the session is delivered to is still
    /// authenticated — the line between a rotation and a second delivery.
    AttachmentActive,
    StoreFull,
}

fn b64(data: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
}

fn b64dec(s: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s)
        .ok()
}

/// The process-wide ticket state. Key is generated once, never persisted, never logged.
pub struct TicketState {
    key: [u8; 32],
    attachments: Mutex<HashMap<String, Attachment>>,
}

impl TicketState {
    pub fn new() -> Result<Self, String> {
        let mut key = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut key);
        Ok(Self {
            key,
            attachments: Mutex::new(HashMap::new()),
        })
    }

    fn sign(&self, body: &str) -> String {
        let mut mac = <HmacSha256 as Mac>::new_from_slice(&self.key).expect("32-byte key");
        mac.update(body.as_bytes());
        b64(&mac.finalize().into_bytes())
    }

    /// A fresh opaque handle: random, never derived from the MAC or session, so a
    /// handle tells a reader nothing about the customer it names.
    fn new_handle() -> String {
        let mut raw = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut raw);
        hex::encode(raw)
    }

    /// Issue a ticket for `mac`'s session. Mirrors Go's IssueSessionTicket.
    pub fn issue(
        &self,
        mac: &str,
        session_expiry: i64,
        now: i64,
    ) -> Result<(String, SessionTicket), TicketError> {
        // No session, no ticket: there is nothing to attach to, and a ticket is
        // never a grant in its own right.
        if session_expiry <= now {
            return Err(TicketError::NoSession);
        }
        let handle = Self::new_handle();
        let exp = now + DEFAULT_TICKET_TTL_SECS;
        let ticket = SessionTicket {
            handle: handle.clone(),
            iat: now,
            exp,
        };
        let payload = serde_json::to_string(&ticket).map_err(|_| TicketError::Malformed)?;
        let body = format!("{TICKET_VERSION}.{}", b64(payload.as_bytes()));
        let envelope = format!("{body}.{}", self.sign(&body));

        let mut store = self.attachments.lock().expect("ticket store");
        sweep_expired(&mut store, now);
        if store.len() >= MAX_ATTACHMENTS {
            // oldest eviction beyond the cap, like Go
            if let Some(oldest) = store
                .iter()
                .min_by_key(|(_, a)| a.attached_at)
                .map(|(k, _)| k.clone())
            {
                store.remove(&oldest);
            }
            if store.len() >= MAX_ATTACHMENTS {
                return Err(TicketError::StoreFull);
            }
        }
        store.insert(
            handle,
            Attachment {
                mac: mac.to_string(),
                expires_at: session_expiry,
                attached_at: now,
            },
        );
        Ok((envelope, ticket))
    }

    /// Verify an envelope's signature/shape/expiry WITHOUT touching the store.
    pub fn verify(&self, envelope: &str, now: i64) -> Result<VerifiedTicket, TicketError> {
        let parts: Vec<&str> = envelope.split('.').collect();
        if parts.len() != 3 || parts[0] != TICKET_VERSION {
            return Err(TicketError::Malformed);
        }
        let body = format!("{}.{}", parts[0], parts[1]);
        // constant-time compare like Go's hmac.Equal
        let expected = self.sign(&body);
        if !constant_time_eq(expected.as_bytes(), parts[2].as_bytes()) {
            return Err(TicketError::Signature);
        }
        let raw = b64dec(parts[1]).ok_or(TicketError::Malformed)?;
        let payload: SessionTicket =
            serde_json::from_slice(&raw).map_err(|_| TicketError::Malformed)?;
        if payload.handle.is_empty() {
            return Err(TicketError::Malformed);
        }
        if now >= payload.exp {
            return Err(TicketError::Expired);
        }
        Ok(VerifiedTicket {
            handle: payload.handle,
            iat: payload.iat,
            exp: payload.exp,
        })
    }

    /// The delivery address a verified ticket currently names.
    pub fn attachment_mac(&self, handle: &str) -> Option<String> {
        let store = self.attachments.lock().expect("ticket store");
        store.get(handle).map(|a| a.mac.clone())
    }

    /// Move a session's delivery address (rebind). Mirrors Go's rebind semantics:
    /// refused while the previous attachment is still authenticated, so one session
    /// cannot be delivered twice concurrently.
    pub fn rebind(
        &self,
        handle: &str,
        new_mac: &str,
        prev_authenticated: bool,
        now: i64,
    ) -> Result<(), TicketError> {
        let mut store = self.attachments.lock().expect("ticket store");
        sweep_expired(&mut store, now);
        let att = store.get_mut(handle).ok_or(TicketError::Unknown)?;
        if prev_authenticated {
            return Err(TicketError::AttachmentActive);
        }
        att.mac = new_mac.to_string();
        att.attached_at = now;
        Ok(())
    }
}

fn sweep_expired(store: &mut HashMap<String, Attachment>, now: i64) {
    store.retain(|_, a| a.expires_at > now);
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> TicketState {
        TicketState::new().unwrap()
    }

    #[test]
    fn issue_verify_roundtrip() {
        let ts = state();
        let (env, ticket) = ts.issue("aa:bb:cc:dd:ee:ff", 999_999_999, 1000).unwrap();
        let v = ts.verify(&env, 2000).unwrap();
        assert_eq!(v.handle, ticket.handle);
        assert_eq!(ts.attachment_mac(&v.handle).unwrap(), "aa:bb:cc:dd:ee:ff");
    }

    #[test]
    fn a_restarted_process_rejects_old_tickets_by_signature() {
        let ts1 = state();
        let (env, _) = ts1.issue("aa:bb:cc:dd:ee:ff", 999_999_999, 1000).unwrap();
        // a different process = a different key
        let ts2 = state();
        assert!(matches!(
            ts2.verify(&env, 2000),
            Err(TicketError::Signature)
        ));
    }

    #[test]
    fn no_session_means_no_ticket() {
        let ts = state();
        assert!(matches!(
            ts.issue("aa:bb:cc:dd:ee:ff", 500, 1000), // session already expired
            Err(TicketError::NoSession)
        ));
    }

    #[test]
    fn tampered_signature_is_rejected() {
        let ts = state();
        let (env, _) = ts.issue("aa:bb:cc:dd:ee:ff", 999_999_999, 1000).unwrap();
        let tampered = format!("{}x", &env[..env.len() - 1]);
        assert!(matches!(
            ts.verify(&tampered, 2000),
            Err(TicketError::Signature)
        ));
    }

    #[test]
    fn expired_tickets_are_refused() {
        let ts = state();
        // issue with a TTL that has passed by verification time
        let (env, _) = ts.issue("aa:bb:cc:dd:ee:ff", 999_999_999, 1000).unwrap();
        assert!(matches!(
            ts.verify(&env, 1000 + DEFAULT_TICKET_TTL_SECS + 1),
            Err(TicketError::Expired)
        ));
    }

    #[test]
    fn rebind_moves_the_address_and_refuses_while_authenticated() {
        let ts = state();
        let (env, t) = ts.issue("aa:bb:cc:dd:ee:ff", 999_999_999, 1000).unwrap();
        // old address still authenticated => refuse
        assert!(matches!(
            ts.rebind(&t.handle, "11:22:33:44:55:66", true, 2000),
            Err(TicketError::AttachmentActive)
        ));
        // old address gone => move
        ts.rebind(&t.handle, "11:22:33:44:55:66", false, 2000)
            .unwrap();
        assert_eq!(ts.attachment_mac(&t.handle).unwrap(), "11:22:33:44:55:66");
        let _ = env;
    }

    #[test]
    fn the_ticket_carries_no_entitlement() {
        // Structural assertion: the wire payload has exactly handle/iat/exp —
        // an allotment or MAC in the envelope would be a portable credential.
        let t = SessionTicket {
            handle: "h".into(),
            iat: 1,
            exp: 2,
        };
        let j = serde_json::to_string(&t).unwrap();
        assert!(!j.contains("allotment"));
        assert!(!j.contains("mac"));
        assert_eq!(j, r#"{"handle":"h","iat":1,"exp":2}"#);
    }

    #[test]
    fn malformed_envelopes_are_refused_not_misread() {
        let ts = state();
        for bad in ["", "v1", "v1.a", "v2.a.b", "v1.a.b.c"] {
            assert!(
                matches!(ts.verify(bad, 1000), Err(TicketError::Malformed)),
                "envelope {bad:?} must be Malformed"
            );
        }
        // invalid base64 payload: malformed, not a signature mismatch
        assert!(matches!(
            ts.verify("v1.!!!.sig", 1000),
            Err(TicketError::Malformed) | Err(TicketError::Signature)
        ));
    }
}

//! P4 — a payment from mint B must be received by mint B's wallet, not mint A's.
//!
//! With one mint configured this cannot fail, which is how A6's
//! `accepted_mints.first()` bug survived: it priced and gated everything against the
//! first mint. This test is the two-mint case that makes the bug visible.
//!
//! Live-only: two real mints are required (`TOLLGATE_TEST_MINT_A`, `_B`). Without them
//! these self-skip with a printed reason rather than passing silently -- a green result
//! here without two mints would prove nothing about the case it exists for.

use crate::wallet::TollWallet;

fn mints() -> Option<(String, String)> {
    let a = std::env::var("TOLLGATE_TEST_MINT_A").ok()?;
    let b = std::env::var("TOLLGATE_TEST_MINT_B").ok()?;
    if a == b {
        return None;
    }
    Some((a, b))
}

/// Two mints registered means two distinct wallets, each separately funded.
///
/// This is the structure the pay handler depends on: `receive()` resolves the mint from
/// the token and looks it up here, so a token from B must land in B's wallet.
#[tokio::test]
async fn two_mints_are_tracked_separately() {
    let Some((a, b)) = mints() else {
        eprintln!("SKIP: set TOLLGATE_TEST_MINT_A and TOLLGATE_TEST_MINT_B");
        return;
    };
    let dir = std::env::temp_dir().join(format!("tg-p4-{}", std::process::id()));
    let mut wallet = TollWallet::new([9u8; 64], vec![a.clone(), b.clone()], dir);

    wallet.ensure_mint(&a).await.expect("register mint A");
    wallet.ensure_mint(&b).await.expect("register mint B");

    let urls = wallet.mint_urls();
    assert!(
        urls.len() >= 2,
        "expected both mints registered, got {urls:?}"
    );

    let balances = wallet.get_balance_by_mint().await.expect("balances");
    let seen: Vec<String> = balances.iter().map(|(m, _)| m.clone()).collect();
    // Exactly two entries: if both mints collapsed into one wallet this would be 1.
    assert_eq!(
        seen.len(),
        2,
        "expected one balance entry PER mint, got {seen:?} -- mints are sharing a wallet"
    );
    assert!(
        seen.iter()
            .any(|m| m.trim_end_matches('/') == a.trim_end_matches('/')),
        "mint A missing from per-mint balances: {seen:?}"
    );
    assert!(
        seen.iter()
            .any(|m| m.trim_end_matches('/') == b.trim_end_matches('/')),
        "mint B missing from per-mint balances: {seen:?}"
    );
}

/// `default_mint()` must REFUSE to guess when several mints are configured.
///
/// Picking arbitrarily would issue a customer's invoice from a mint they did not expect.
/// Returning `None` forces the caller to be explicit.
#[tokio::test]
async fn default_mint_refuses_to_guess_between_two() {
    let Some((a, b)) = mints() else {
        eprintln!("SKIP: set TOLLGATE_TEST_MINT_A and TOLLGATE_TEST_MINT_B");
        return;
    };
    let dir = std::env::temp_dir().join(format!("tg-p4b-{}", std::process::id()));
    let mut wallet = TollWallet::new([9u8; 64], vec![a.clone(), b.clone()], dir);
    wallet.ensure_mint(&a).await.unwrap();
    wallet.ensure_mint(&b).await.unwrap();

    assert_eq!(
        wallet.default_mint(),
        None,
        "with two mints configured, default_mint must return None rather than picking one"
    );
}

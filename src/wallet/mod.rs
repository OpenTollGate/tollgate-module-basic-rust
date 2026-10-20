//! CDK wallet integration — TollWallet wrapping cdk::Wallet.
//!
//! This module implements the full wallet lifecycle using CDK (Cashu Dev Kit)
//! instead of gonuts. CDK uses a saga pattern internally — operations are
//! atomic, eliminating the swap-counter race that bricked gonuts wallets
//! in v0.7.1–v0.7.3.
//!
//! # Architecture
//!
//! - `TollWallet` wraps `cdk::Wallet` inside `Arc<tokio::sync::Mutex<Wallet>>`
//!   for thread-safe serialized access.
//! - Persistence via `cdk-sqlite` at `/etc/tollgate/wallet.sqlite`.
//! - Seed (64 bytes) persisted at `/etc/tollgate/wallet_seed.bin` mode 0600.
//! - All receive/send/melt operations wrapped in `tokio::time::timeout(30s)`.
//!   On timeout the future is dropped cleanly — CDK saga ensures no partial
//!   state is persisted.
//!
//! # Mapping (13 gonuts call sites → CDK)
//!
//! | gonuts method           | CDK equivalent                                    |
//! |-------------------------|---------------------------------------------------|
//! | `wallet.LoadWallet`     | `Wallet::new(mint_url, unit, localstore, seed)`   |
//! | `wallet.AddMint`        | `Wallet::new` for that mint (multi-mint via map)  |
//! | `wallet.Shutdown`       | drop `Wallet` (closes DB)                         |
//! | `wallet.Receive`        | `wallet.receive(token_str, ReceiveOptions)`       |
//! | `wallet.Send`           | `wallet.prepare_send(amount, opts).confirm()`     |
//! | `wallet.SendWithOptions`| `prepare_send` with `SendKind::OnlineTolerance`   |
//! | `wallet.RequestMint`    | `wallet.mint_quote(PaymentMethod::BOLT11, ...)`   |
//! | `wallet.MintQuoteState` | `wallet.check_mint_quote_status(&id)`            |
//! | `wallet.MintTokens`    | `wallet.mint(&id, amount, opts)`                  |
//! | `wallet.GetBalance`     | `wallet.total_balance()`                          |
//! | `wallet.GetBalanceByMints` | per-wallet `total_balance()`                   |
//! | `wallet.RequestMeltQuote`| `wallet.prepare_melt(BOLT11, invoice)`           |
//! | `wallet.Melt`           | `prepared_melt.confirm()`                         |

pub mod verify;
#[allow(clippy::module_inception)]
pub mod wallet;

pub use wallet::TollWallet;

#[cfg(test)]
mod per_mint_test;

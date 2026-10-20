//! HTTP route handlers — each route in its own submodule.

pub mod balance;
pub mod discovery;
#[cfg(test)]
mod discovery_test;
pub mod ln_invoice;
#[cfg(test)]
mod ln_invoice_quote_test;
pub mod pay;
#[cfg(test)]
mod pay_body_test;
#[cfg(test)]
mod pay_notice_test;
#[cfg(test)]
mod pay_session_test;
pub mod quote;
pub mod select;
#[cfg(test)]
mod select_test;
pub mod usage;
pub mod whoami;

#[cfg(test)]
mod ln_invoice_contract_test;

#[cfg(test)]
mod balance_contract_test;

#[cfg(test)]
mod ln_invoice_identity_test;

#[cfg(test)]
mod purchase_path_test;

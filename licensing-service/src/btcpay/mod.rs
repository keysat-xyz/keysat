//! BTCPay Server integration.
//!
//! - [`client`] creates invoices via the BTCPay Greenfield API.
//! - [`webhook`] verifies and parses incoming webhook calls from BTCPay.
//!
//! BTCPay's Greenfield API is documented at
//! <https://docs.btcpayserver.org/API/Greenfield/v1/>.

pub mod client;
pub mod config;
pub mod network;
pub mod webhook;

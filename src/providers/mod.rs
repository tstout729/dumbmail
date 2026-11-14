//! Delivery providers. Email defaults to Amazon SES ($0.10 per 1,000); SMS defaults to Telnyx.
//! `log` providers print instead of sending and keep the last messages in memory (dev and tests).

pub mod email;
pub mod sms;

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct DnsRecord {
    #[serde(rename = "type")]
    pub kind: String,
    pub name: String,
    pub value: String,
    pub status: String,
}

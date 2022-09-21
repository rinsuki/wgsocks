use std::net::{SocketAddr, Ipv4Addr};

use serde::{Deserialize};

#[derive(Deserialize, Clone, Debug)]
pub struct Config {
    pub private_key: String,
    pub peer: PeerConfig,
    pub mtu: usize,
    pub ip4: Ipv4Addr,
    pub dns: Option<DNSConfig>,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(untagged)]
pub enum DNSConfig {
    Special(SpecialDNSTypes),
    // TODO: Support IPv6 and use IpAddr instead
    // Custom(Ipv4Addr),
}

#[derive(Deserialize, Clone, Debug)]
pub enum SpecialDNSTypes {
    #[serde(rename = "system")]
    System,
}

#[derive(Deserialize, Clone, Debug)]
pub struct PeerConfig {
    pub endpoint: SocketAddr,
    pub public_key: String,
}

pub fn decode_base64_key(key: &str) -> [u8; 32] {
    let mut decoded = [0u8; 32];
    let size = base64::decode_config_slice(key, base64::STANDARD, &mut decoded).unwrap();
    assert_eq!(size, 32);
    decoded
}

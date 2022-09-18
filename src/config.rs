use std::net::{SocketAddr, Ipv4Addr};

use serde::Deserialize;

#[derive(Deserialize, Clone)]
pub struct Config {
    pub private_key: String,
    pub peer: PeerConfig,
    pub mtu: usize,
    pub ip4: Ipv4Addr,
}

#[derive(Deserialize, Clone)]
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

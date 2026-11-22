//! Join codes: the host's iroh `EndpointAddr` (public key, relay URL, direct addresses), postcard-encoded
//! and base32'd so it survives copy and paste through any chat app. The direct addresses also make LAN
//! play work without the relay.

use data_encoding::BASE32_NOPAD;
use iroh::EndpointAddr;

const PREFIX: &str = "NO1";
/// A code longer than this is not ours.
const MAX_LEN: usize = 2048;

pub fn encode(addr: &EndpointAddr) -> String {
    let bytes = postcard::to_allocvec(addr).expect("serializing to a Vec cannot fail");
    format!("{PREFIX}{}", BASE32_NOPAD.encode(&bytes))
}

pub fn decode(code: &str) -> Result<EndpointAddr, String> {
    let clean: String = code.chars().filter(|c| !c.is_whitespace() && *c != '-').collect::<String>().to_ascii_uppercase();
    if clean.len() > MAX_LEN {
        return Err("join code is too long".into());
    }
    let body = clean.strip_prefix(PREFIX).ok_or("that is not a nier-online join code")?;
    let bytes = BASE32_NOPAD.decode(body.as_bytes()).map_err(|_| "join code is damaged (bad characters)")?;
    let (addr, rest) = postcard::take_from_bytes::<EndpointAddr>(&bytes).map_err(|_| "join code is damaged")?;
    if !rest.is_empty() {
        return Err("join code is damaged (extra data)".into());
    }
    Ok(addr)
}

#[cfg(test)]
mod tests {
    use super::*;
    use iroh::{SecretKey, TransportAddr};

    #[test]
    fn roundtrip() {
        let id = SecretKey::from_bytes(&[7u8; 32]).public();
        let mut addr = EndpointAddr::new(id);
        addr.addrs.insert(TransportAddr::Ip("192.168.1.20:4433".parse().unwrap()));
        addr.addrs.insert(TransportAddr::Relay("https://relay.example.org./".parse().unwrap()));
        let code = encode(&addr);
        assert!(code.starts_with(PREFIX));
        assert_eq!(decode(&code).unwrap(), addr);
        // Pasting often adds whitespace or lowercases.
        assert_eq!(decode(&format!("  {}\n", code.to_lowercase())).unwrap(), addr);
    }

    #[test]
    fn rejects_garbage() {
        assert!(decode("").is_err());
        assert!(decode("hello").is_err());
        assert!(decode("NO1!!!!").is_err());
        assert!(decode("NO1AAAA").is_err());
        assert!(decode(&format!("NO1{}", "A".repeat(5000))).is_err());
    }
}

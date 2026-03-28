use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6, ToSocketAddrs};
use std::str::FromStr;

/// Unspecified IPv4/IPv6 socket addresses for local binding (port 0).
const UNSPECIFIED_V4: SocketAddr =
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0));
const UNSPECIFIED_V6: SocketAddr =
    SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, 0, 0, 0));

/// Parsed Aeron UDP channel URI.
///
/// Examples:
///   `aeron:udp?endpoint=localhost:40123`
///   `aeron:udp?endpoint=224.0.1.1:40456|interface=192.168.1.5`
///   `aeron:udp?control=224.0.1.1:40456|control-mode=dynamic`
#[derive(Debug, Clone)]
pub struct UdpChannel {
    pub remote_data: SocketAddr,
    pub local_data: SocketAddr,
    pub remote_control: SocketAddr,
    pub local_control: SocketAddr,
    pub interface_addr: Option<IpAddr>,
    pub multicast_ttl: Option<u8>,
    pub is_multicast: bool,
    pub is_manual_control_mode: bool,
    pub is_dynamic_control_mode: bool,
    pub is_response: bool,
    pub tag: Option<i64>,
    pub params: HashMap<String, String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ChannelError {
    #[error("invalid channel URI: {0}")]
    InvalidUri(String),
    #[error("missing required param: {0}")]
    MissingParam(String),
    #[error("dns resolve failed: {0}")]
    Resolve(String),
}

impl UdpChannel {
    pub fn parse(uri: &str) -> Result<Self, ChannelError> {
        if !uri.starts_with("aeron:udp?") {
            return Err(ChannelError::InvalidUri(uri.to_string()));
        }

        let query = &uri["aeron:udp?".len()..];
        let params = Self::parse_params(query);

        let endpoint = params
            .get("endpoint")
            .ok_or_else(|| ChannelError::MissingParam("endpoint".into()))?;

        let remote_data = Self::resolve_addr(endpoint)?;

        let is_multicast = match remote_data.ip() {
            IpAddr::V4(ip) => ip.is_multicast(),
            IpAddr::V6(ip) => ip.is_multicast(),
        };

        let local_data = match remote_data {
            SocketAddr::V4(_) => UNSPECIFIED_V4,
            SocketAddr::V6(_) => UNSPECIFIED_V6,
        };

        let control_str = params.get("control");
        let remote_control = if let Some(c) = control_str {
            Self::resolve_addr(c)?
        } else {
            remote_data
        };
        let local_control = local_data;

        let interface_addr = params
            .get("interface")
            .and_then(|s| IpAddr::from_str(s).ok());

        let multicast_ttl = params
            .get("ttl")
            .and_then(|s| s.parse::<u8>().ok());

        let control_mode = params.get("control-mode").map(|s| s.as_str());

        Ok(Self {
            remote_data,
            local_data,
            remote_control,
            local_control,
            interface_addr,
            multicast_ttl,
            is_multicast,
            is_manual_control_mode: control_mode == Some("manual"),
            is_dynamic_control_mode: control_mode == Some("dynamic"),
            is_response: params.get("response").map(|s| s == "true").unwrap_or(false),
            tag: params.get("tag").and_then(|s| s.parse().ok()),
            params,
        })
    }

    fn parse_params(query: &str) -> HashMap<String, String> {
        let mut map = HashMap::new();
        for part in query.split('|') {
            if let Some((k, v)) = part.split_once('=') {
                map.insert(k.trim().to_string(), v.trim().to_string());
            }
        }
        map
    }

    fn resolve_addr(s: &str) -> Result<SocketAddr, ChannelError> {
        // Try direct parse first
        if let Ok(addr) = s.parse::<SocketAddr>() {
            return Ok(addr);
        }
        // Try DNS resolution
        s.to_socket_addrs()
            .map_err(|e| ChannelError::Resolve(format!("{s}: {e}")))?
            .next()
            .ok_or_else(|| ChannelError::Resolve(format!("no addresses for {s}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_unicast() {
        let ch = UdpChannel::parse("aeron:udp?endpoint=127.0.0.1:40123").unwrap();
        assert_eq!(ch.remote_data.port(), 40123);
        assert!(!ch.is_multicast);
    }

    #[test]
    fn parse_multicast() {
        let ch = UdpChannel::parse("aeron:udp?endpoint=224.0.1.1:40456|interface=192.168.1.5").unwrap();
        assert!(ch.is_multicast);
        assert_eq!(ch.interface_addr, Some(IpAddr::from([192, 168, 1, 5])));
    }
}
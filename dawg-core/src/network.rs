use serde::{Deserialize, Serialize};
use std::net::{IpAddr, Ipv4Addr};

/// User intent is persisted; observations and router leases are not configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NetworkConfig {
    /// A hostname already configured through DNS/DDNS, or an assigned public IP.
    pub public_host: Option<String>,
    /// Defaults to the Minecraft server port. Never allocate a random port.
    pub external_port: Option<u16>,
    pub upnp_enabled: bool,
    /// Use a provider-assigned stable Minecraft tunnel hostname.
    pub tunnel_enabled: bool,
}

impl NetworkConfig {
    pub fn external_port(&self, server_port: u16) -> u16 {
        self.external_port.unwrap_or(server_port)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.upnp_enabled && self.tunnel_enabled {
            return Err("Choose either router forwarding or a free tunnel, not both.".into());
        }
        if self.external_port == Some(0) {
            return Err("The public port must be between 1 and 65535.".into());
        }
        let Some(host) = &self.public_host else {
            return Ok(());
        };
        if host.is_empty() || host.len() > 253 || host.trim() != host {
            return Err("Enter a public IP or DNS hostname without spaces.".into());
        }
        if let Ok(ip) = host.parse::<IpAddr>() {
            return if is_public_ip(ip) {
                Ok(())
            } else {
                Err("A local, private or reserved IP is not a public join address.".into())
            };
        }
        let valid = host.contains('.')
            && host.split('.').all(|label| {
                !label.is_empty()
                    && label.len() <= 63
                    && !label.starts_with('-')
                    && !label.ends_with('-')
                    && label
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || c == b'-')
            });
        let lower = host.to_ascii_lowercase();
        if !valid
            || host.bytes().all(|c| c.is_ascii_digit() || c == b'.')
            || lower == "localhost"
            || lower.ends_with(".localhost")
            || lower.ends_with(".local")
        {
            return Err("Use a public DNS hostname or IP without a scheme, port or path.".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ForwardingStatus {
    #[default]
    Disabled,
    Pending,
    NeedsSetup,
    Active,
    Unavailable,
    Error,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct NetworkStatus {
    pub lan_address: Option<String>,
    pub external_ip: Option<String>,
    pub join_address: Option<String>,
    pub forwarding: ForwardingStatus,
    pub detail: Option<String>,
    pub checked_at_ms: Option<u64>,
    /// One-time provider setup shown only to the authenticated administrator.
    pub setup_url: Option<String>,
}

pub fn format_address(host: &str, port: u16) -> String {
    if host.parse::<std::net::Ipv6Addr>().is_ok() {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// Addresses that are unsuitable as an Internet endpoint, including CGNAT.
pub fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => public_v4(ip),
        IpAddr::V6(ip) => {
            if let Some(ip) = ip.to_ipv4_mapped() {
                return public_v4(ip);
            }
            let segments = ip.segments();
            !ip.is_unspecified()
                && !ip.is_loopback()
                && !ip.is_multicast()
                && segments[0] & 0xfe00 != 0xfc00
                && segments[0] & 0xffc0 != 0xfe80
                && !(segments[0] == 0x2001 && segments[1] == 0x0db8)
                && segments[0] & 0xe000 == 0x2000
        }
    }
}

fn public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, _, _] = ip.octets();
    !ip.is_unspecified()
        && !ip.is_loopback()
        && !ip.is_private()
        && !ip.is_link_local()
        && !ip.is_multicast()
        && !ip.is_broadcast()
        && !ip.is_documentation()
        && a != 0
        && a < 224
        && !(a == 100 && b & 0xc0 == 64)
        && !(a == 192 && b == 0)
        && !(a == 198 && (b == 18 || b == 19))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_and_cgnat_addresses_are_never_public_endpoints() {
        for ip in [
            "127.0.0.1",
            "192.168.1.4",
            "10.0.0.2",
            "172.31.0.4",
            "100.64.0.1",
            "100.127.255.254",
            "169.254.1.1",
            "0.0.0.0",
            "203.0.113.2",
            "198.18.1.1",
            "255.255.255.255",
            "::1",
            "fd00::1",
            "fe80::1",
            "::ffff:192.168.1.3",
        ] {
            assert!(!is_public_ip(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["8.8.8.8", "1.1.1.1", "2606:4700:4700::1111"] {
            assert!(is_public_ip(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn public_hostname_is_valid_but_a_url_or_local_address_is_not() {
        for host in [
            "play.example.net",
            "my-server.duckdns.org",
            "1.1.1.1",
            "2606:4700:4700::1111",
        ] {
            assert!(
                NetworkConfig {
                    public_host: Some(host.into()),
                    ..Default::default()
                }
                .validate()
                .is_ok()
            );
        }
        for host in [
            "localhost",
            "192.168.1.4",
            "100.64.1.1",
            "https://play.example.net",
            "play.example.net:25565",
            "-server.example.net",
            "server.local",
            " play.example.net",
        ] {
            assert!(
                NetworkConfig {
                    public_host: Some(host.into()),
                    ..Default::default()
                }
                .validate()
                .is_err(),
                "{host}"
            );
        }
    }

    #[test]
    fn endpoints_keep_the_requested_port_and_ipv6_is_bracketed() {
        assert_eq!(NetworkConfig::default().external_port(25565), 25565);
        assert_eq!(
            NetworkConfig {
                external_port: Some(25566),
                ..Default::default()
            }
            .external_port(25565),
            25566
        );
        assert_eq!(
            format_address("play.example.net", 25565),
            "play.example.net:25565"
        );
        assert_eq!(
            format_address("2606:4700:4700::1111", 25565),
            "[2606:4700:4700::1111]:25565"
        );
    }
}

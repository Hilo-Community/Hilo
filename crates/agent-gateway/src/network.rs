use std::{
    io,
    net::{IpAddr, SocketAddr},
    time::Duration,
};

/// Only DNS names. No URL syntax, wildcard, alternate port, local name, or IP literal.
pub fn valid_host(host: &str) -> bool {
    host.len() <= 253
        && host.contains('.')
        && !host.ends_with('.')
        && host.parse::<IpAddr>().is_err()
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
        && ![
            ".localhost",
            ".local",
            ".internal",
            ".test",
            ".invalid",
            ".example",
        ]
        .iter()
        .any(|suffix| host.ends_with(suffix))
}

pub fn public_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(ip) => {
            let [a, b, c, _] = ip.octets();
            !(a == 0
                || a == 10
                || a == 127
                || a >= 224
                || (a == 100 && (64..=127).contains(&b))
                || (a == 169 && b == 254)
                || (a == 172 && (16..=31).contains(&b))
                || (a == 192 && (b == 168 || b == 0 || (b == 88 && c == 99)))
                || (a == 198 && (b == 18 || b == 19 || (b == 51 && c == 100)))
                || (a == 203 && b == 0 && c == 113))
        }
        IpAddr::V6(ip) => {
            let segments = ip.segments();
            // Native global unicast only. Refuse mapped IPv4, NAT64, 6to4, special-use
            // 2001::/16 (including Teredo and documentation), and 3fff::/20 documentation.
            (segments[0] & 0xe000) == 0x2000
                && segments[0] != 0x2001
                && segments[0] != 0x2002
                && !(segments[0] == 0x3fff && segments[1] & 0xf000 == 0)
        }
    }
}

pub(crate) async fn resolve(host: &str) -> io::Result<Vec<SocketAddr>> {
    if !valid_host(host) {
        return Err(io::Error::other("invalid upstream hostname"));
    }
    let addresses: Vec<_> = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::net::lookup_host((host, 443)),
    )
    .await
    .map_err(|_| io::Error::other("DNS timeout"))??
    .collect();
    if addresses.is_empty()
        || addresses
            .iter()
            .any(|address| !public_address(address.ip()))
    {
        return Err(io::Error::other("DNS returned a non-public address"));
    }
    Ok(addresses)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn literal_private_metadata_and_reserved_addresses_are_refused() {
        for ip in [
            "0.0.0.0",
            "10.0.0.1",
            "100.64.0.1",
            "127.0.0.1",
            "169.254.169.254",
            "172.16.1.1",
            "192.168.1.1",
            "192.0.2.1",
            "198.18.1.1",
            "198.51.100.1",
            "203.0.113.1",
            "224.0.0.1",
            "255.255.255.255",
            "::1",
            "::ffff:127.0.0.1",
            "fe80::1",
            "fc00::1",
            "2001:db8::1",
            "2002:7f00:1::",
        ] {
            assert!(!public_address(ip.parse().unwrap()), "{ip}");
            assert!(!valid_host(ip), "{ip}");
        }
        for ip in ["1.1.1.1", "8.8.8.8", "2606:4700:4700::1111"] {
            assert!(public_address(ip.parse().unwrap()));
        }
        for host in [
            "localhost",
            "api.example.test",
            "api.openai.com:443",
            "user@api.openai.com",
            "*.openai.com",
            "api.openai.com.",
            "API.OPENAI.COM",
            "api..openai.com",
            "-api.openai.com",
        ] {
            assert!(!valid_host(host), "{host}");
        }
        assert!(valid_host("api.openai.com"));
    }
}

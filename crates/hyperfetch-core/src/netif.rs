//! The network interfaces downloads can spread their connections over.

use std::net::IpAddr;

/// A network interface and the local address connections bind to on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetInterface {
    pub name: String,
    pub address: std::net::IpAddr,
}

/// The interfaces that reach the internet: up, not loopback, not link-local, with a gateway.
/// One address each, IPv4 when it has one: most servers still answer on IPv4 only.
pub fn usable() -> Vec<NetInterface> {
    netdev::get_interfaces()
        .into_iter()
        .filter(|i| i.is_up() && !i.is_loopback() && i.gateway.is_some())
        .filter_map(|i| {
            let mut addresses: Vec<IpAddr> = i.ip_addrs().into_iter().filter(|a| bindable(*a)).collect();
            addresses.sort_by_key(IpAddr::is_ipv6);
            let name = i.friendly_name.clone().unwrap_or_else(|| i.name.clone());
            Some(NetInterface { name, address: *addresses.first()? })
        })
        .collect()
}

/// An address a connection can leave from: not loopback, link-local or unspecified.
fn bindable(address: IpAddr) -> bool {
    !address.is_loopback()
        && !address.is_unspecified()
        && match address {
            IpAddr::V4(a) => !a.is_link_local(),
            IpAddr::V6(a) => (a.segments()[0] & 0xffc0) != 0xfe80,
        }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_addresses_that_leave_the_machine() {
        for a in ["127.0.0.1", "169.254.3.4", "0.0.0.0", "::1", "fe80::1", "::"] {
            assert!(!bindable(a.parse().unwrap()), "{a}");
        }
        for a in ["192.168.1.20", "10.0.0.5", "2001:db8::7"] {
            assert!(bindable(a.parse().unwrap()), "{a}");
        }
        // Whatever this machine has, none of it is loopback.
        assert!(usable().iter().all(|i| bindable(i.address)));
    }
}

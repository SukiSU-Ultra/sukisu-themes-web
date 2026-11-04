//! Outbound address-family selection for HTTP clients.
//!
//! `Auto` keeps the platform resolver's ordering. `Ipv4`/`Ipv6` install a
//! resolver wrapper that filters A/AAAA answers, so a dual-stack host can
//! deliberately use one family even when DNS returns both.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IpFamily {
    #[default]
    Auto,
    Ipv4,
    Ipv6,
}

impl IpFamily {
    pub fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "ipv4" | "v4" | "4" => Self::Ipv4,
            "ipv6" | "v6" | "6" => Self::Ipv6,
            _ => Self::Auto,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Ipv4 => "ipv4",
            Self::Ipv6 => "ipv6",
        }
    }
}

struct FamilyResolver {
    family: IpFamily,
}

impl reqwest::dns::Resolve for FamilyResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let family = self.family;
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addresses = tokio::net::lookup_host((host.as_str(), 0)).await?;
            let filtered: Vec<SocketAddr> = addresses
                .filter(|address| match family {
                    IpFamily::Auto => true,
                    IpFamily::Ipv4 => address.is_ipv4(),
                    IpFamily::Ipv6 => address.is_ipv6(),
                })
                .collect();
            if filtered.is_empty() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("no {} address for {host}", family.as_str()),
                )
                .into());
            }
            Ok(Box::new(filtered.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// Install the family filter on a reqwest builder. `Auto` is a no-op so the
/// platform resolver's ordering and /etc/hosts behavior stay untouched.
pub fn apply_client_family(
    builder: reqwest::ClientBuilder,
    family: IpFamily,
) -> reqwest::ClientBuilder {
    match family {
        IpFamily::Auto => builder,
        IpFamily::Ipv4 => builder
            .local_address(IpAddr::V4(Ipv4Addr::UNSPECIFIED))
            .dns_resolver(Arc::new(FamilyResolver { family })),
        IpFamily::Ipv6 => builder
            .local_address(IpAddr::V6(Ipv6Addr::UNSPECIFIED))
            .dns_resolver(Arc::new(FamilyResolver { family })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::dns::Resolve;

    #[test]
    fn parses_family_aliases() {
        assert_eq!(IpFamily::parse("ipv4"), IpFamily::Ipv4);
        assert_eq!(IpFamily::parse("V4"), IpFamily::Ipv4);
        assert_eq!(IpFamily::parse("6"), IpFamily::Ipv6);
        assert_eq!(IpFamily::parse(""), IpFamily::Auto);
        assert_eq!(IpFamily::parse("anything"), IpFamily::Auto);
        assert_eq!(IpFamily::Ipv4.as_str(), "ipv4");
    }

    #[tokio::test]
    async fn resolver_filters_family() {
        let name: reqwest::dns::Name = "localhost".parse().unwrap();
        if let Ok(addresses) = (FamilyResolver {
            family: IpFamily::Ipv4,
        })
        .resolve(name)
        .await
        {
            let addresses: Vec<_> = addresses.collect();
            assert!(!addresses.is_empty());
            assert!(addresses.iter().all(|address| address.is_ipv4()));
        }
        let name: reqwest::dns::Name = "localhost".parse().unwrap();
        if let Ok(addresses) = (FamilyResolver {
            family: IpFamily::Ipv6,
        })
        .resolve(name)
        .await
        {
            let addresses: Vec<_> = addresses.collect();
            assert!(addresses.iter().all(|address| address.is_ipv6()));
        }
    }
}

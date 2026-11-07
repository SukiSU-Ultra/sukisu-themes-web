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

/// PEM file holding extra CAs that distribution probes must trust. Edges reached
/// over a private network (EasyTier, LAN) usually present a self-signed certificate
/// that no public root authority covers.
pub const EXTRA_CA_ENV: &str = "THEME_DISTRIBUTION_CA_FILE";

/// Trust the bundled roots plus the PEM bundle named by `THEME_DISTRIBUTION_CA_FILE`.
/// Without the variable the builder is untouched, so unrelated traffic cannot be
/// downgraded by a stray file.
pub fn apply_extra_cas(builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    let Some(path) = std::env::var_os(EXTRA_CA_ENV).filter(|value| !value.is_empty()) else {
        return builder;
    };
    let mut builder = builder;
    match std::fs::read(&path) {
        Ok(pem) => {
            // A PEM bundle concatenates certificates; split on the END marker so each
            // block is added as its own trust anchor.
            let text = String::from_utf8_lossy(&pem).into_owned();
            let marker = "-----END CERTIFICATE-----";
            let mut blocks = 0;
            let mut start = 0usize;
            while let Some(index) = text[start..].find(marker) {
                let end = start + index + marker.len();
                let block = &text[start..end];
                if let Some(begin) = block.find("-----BEGIN CERTIFICATE-----") {
                    if let Ok(cert) = reqwest::Certificate::from_pem(block[begin..].as_bytes()) {
                        builder = builder.add_root_certificate(cert);
                        blocks += 1;
                    }
                }
                start = end;
            }
            tracing::info!(blocks, path = %std::path::Path::new(&path).display(), "loaded extra distribution CAs");
        }
        Err(error) => {
            tracing::warn!(%error, path = %std::path::Path::new(&path).display(), "failed to read distribution CA file");
        }
    }
    builder
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

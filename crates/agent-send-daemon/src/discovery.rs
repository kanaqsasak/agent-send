//! LAN peer discovery.
//!
//! mDNS advertisements are hints only: callers must feed them into the peer
//! registry and require an explicit pairing confirmation before opening a peer
//! channel. No trust material is placed in DNS-SD TXT records.

use crate::PeerAdvertisement;
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::net::{IpAddr, SocketAddr};
use thiserror::Error;

pub const MDNS_SERVICE_TYPE: &str = "_agent-send._tcp.local.";

#[derive(Debug, Error)]
pub enum DiscoveryError {
    #[error("mDNS operation failed: {0}")]
    Mdns(#[from] mdns_sd::Error),
    #[error("peer advertisement has an invalid address: {0}")]
    InvalidAddress(String),
    #[error("peer advertisement is missing {0}")]
    MissingField(&'static str),
    #[error("peer advertisement has an unsupported protocol version: {0}")]
    UnsupportedVersion(u32),
    #[error("peer advertisement is too large for DNS-SD")]
    AdvertisementTooLarge,
    #[error("mDNS event stream closed")]
    EventStreamClosed,
}

/// Publishes local presence and polls for LAN presence announcements.
///
/// Implementations return untrusted [`PeerAdvertisement`] values. Keeping this
/// boundary independent of `PeerRegistry` makes tests deterministic and makes
/// a manual-entry adapter possible without changing pairing or trust policy.
pub trait PeerDiscovery: Send {
    fn publish(&mut self, advertisement: &PeerAdvertisement) -> Result<(), DiscoveryError>;
    fn discover(&mut self) -> Result<Vec<PeerAdvertisement>, DiscoveryError>;
}

/// DNS-SD/mDNS adapter used on supported desktop LANs.
///
/// It advertises only an endpoint, peer ID, alias, and protocol version. The
/// endpoint is selected from mDNS address records while resolving a peer, not
/// trusted from TXT metadata.
pub struct MdnsDiscovery {
    daemon: ServiceDaemon,
    events: mdns_sd::Receiver<ServiceEvent>,
    published_id: Option<String>,
    published_fullname: Option<String>,
}

impl MdnsDiscovery {
    pub fn new() -> Result<Self, DiscoveryError> {
        let daemon = ServiceDaemon::new()?;
        let events = daemon.browse(MDNS_SERVICE_TYPE)?;
        Ok(Self {
            daemon,
            events,
            published_id: None,
            published_fullname: None,
        })
    }
}

impl PeerDiscovery for MdnsDiscovery {
    fn publish(&mut self, advertisement: &PeerAdvertisement) -> Result<(), DiscoveryError> {
        validate_advertisement(advertisement)?;
        let endpoint = advertisement
            .address
            .parse::<SocketAddr>()
            .map_err(|_| DiscoveryError::InvalidAddress(advertisement.address.clone()))?;
        if endpoint.port() == 0 {
            return Err(DiscoveryError::InvalidAddress(
                advertisement.address.clone(),
            ));
        }
        if advertisement.id.len() > 200 || advertisement.alias.len() > 200 {
            return Err(DiscoveryError::AdvertisementTooLarge);
        }

        let instance = format!("agent-send-{}", id_digest(&advertisement.id));
        let hostname = format!("{instance}.local.");
        let api_version = advertisement.api_version.to_string();
        let properties = [
            ("id", advertisement.id.as_str()),
            ("alias", advertisement.alias.as_str()),
            ("api_version", api_version.as_str()),
        ];
        let service = ServiceInfo::new(
            MDNS_SERVICE_TYPE,
            &instance,
            &hostname,
            "",
            endpoint.port(),
            &properties[..],
        )
        .map_err(DiscoveryError::Mdns)?
        // Do not publish a caller-provided address: the mDNS library enumerates
        // current LAN interfaces, including changes after startup.
        .enable_addr_auto();
        let fullname = service.get_fullname().to_owned();
        if let Some(previous) = self.published_fullname.replace(fullname) {
            let _ = self.daemon.unregister(&previous);
        }
        self.daemon.register(service)?;
        self.published_id = Some(advertisement.id.clone());
        Ok(())
    }

    fn discover(&mut self) -> Result<Vec<PeerAdvertisement>, DiscoveryError> {
        let mut advertisements = Vec::new();
        loop {
            match self.events.try_recv() {
                Ok(ServiceEvent::ServiceResolved(info)) => {
                    if let Some(advertisement) = advertisement_from_service(&info) {
                        if self.published_id.as_deref() != Some(&advertisement.id) {
                            advertisements.push(advertisement);
                        }
                    }
                }
                Ok(_) => {}
                Err(_) if self.events.is_disconnected() => {
                    return Err(DiscoveryError::EventStreamClosed)
                }
                Err(_) => break,
            }
        }
        advertisements.sort_by(|left, right| left.id.cmp(&right.id));
        advertisements.dedup_by(|left, right| left.id == right.id);
        Ok(advertisements)
    }
}

impl Drop for MdnsDiscovery {
    fn drop(&mut self) {
        if let Some(fullname) = &self.published_fullname {
            let _ = self.daemon.unregister(fullname);
        }
        let _ = self.daemon.stop_browse(MDNS_SERVICE_TYPE);
        let _ = self.daemon.shutdown();
    }
}

/// Deterministic discovery adapter for tests and in-process callers.
#[derive(Debug, Default)]
pub struct MockPeerDiscovery {
    published: Vec<PeerAdvertisement>,
    discovered: VecDeque<PeerAdvertisement>,
}

impl MockPeerDiscovery {
    pub fn inject(&mut self, advertisement: PeerAdvertisement) {
        self.discovered.push_back(advertisement);
    }

    pub fn published(&self) -> &[PeerAdvertisement] {
        &self.published
    }
}

impl PeerDiscovery for MockPeerDiscovery {
    fn publish(&mut self, advertisement: &PeerAdvertisement) -> Result<(), DiscoveryError> {
        validate_advertisement(advertisement)?;
        self.published.push(advertisement.clone());
        Ok(())
    }

    fn discover(&mut self) -> Result<Vec<PeerAdvertisement>, DiscoveryError> {
        Ok(self.discovered.drain(..).collect())
    }
}

fn validate_advertisement(advertisement: &PeerAdvertisement) -> Result<(), DiscoveryError> {
    if advertisement.id.trim().is_empty() {
        return Err(DiscoveryError::MissingField("id"));
    }
    if advertisement.alias.trim().is_empty() {
        return Err(DiscoveryError::MissingField("alias"));
    }
    if advertisement.api_version != crate::API_VERSION {
        return Err(DiscoveryError::UnsupportedVersion(
            advertisement.api_version,
        ));
    }
    Ok(())
}

fn advertisement_from_service(info: &ServiceInfo) -> Option<PeerAdvertisement> {
    let id = info.get_property_val_str("id")?.to_owned();
    let alias = info.get_property_val_str("alias")?.to_owned();
    let api_version = info.get_property_val_str("api_version")?.parse().ok()?;
    let address = preferred_address(info.get_addresses(), info.get_port())?;
    let advertisement = PeerAdvertisement {
        id,
        alias,
        address,
        api_version,
    };
    validate_advertisement(&advertisement).ok()?;
    Some(advertisement)
}

fn preferred_address(addresses: &std::collections::HashSet<IpAddr>, port: u16) -> Option<String> {
    let mut addresses: Vec<_> = addresses
        .iter()
        .copied()
        .filter(|address| !address.is_loopback() && !address.is_unspecified())
        .collect();
    addresses.sort();
    addresses
        .first()
        .map(|address| SocketAddr::new(*address, port).to_string())
}

fn id_digest(id: &str) -> String {
    Sha256::digest(id.as_bytes())
        .iter()
        .take(12)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn advertisement(id: &str) -> PeerAdvertisement {
        PeerAdvertisement {
            id: id.into(),
            alias: format!("peer {id}"),
            address: "192.0.2.4:8742".into(),
            api_version: crate::API_VERSION,
        }
    }

    #[test]
    fn mock_discovery_is_deterministic_and_validates_publications() {
        let mut discovery = MockPeerDiscovery::default();
        let local = advertisement("local");
        discovery.publish(&local).unwrap();
        discovery.inject(advertisement("second"));
        discovery.inject(advertisement("first"));

        assert_eq!(discovery.published(), &[local]);
        assert_eq!(
            discovery
                .discover()
                .unwrap()
                .into_iter()
                .map(|advertisement| advertisement.id)
                .collect::<Vec<_>>(),
            vec!["second", "first"]
        );
        assert!(discovery.discover().unwrap().is_empty());
    }

    #[test]
    fn discovery_rejects_unusable_metadata() {
        let mut discovery = MockPeerDiscovery::default();
        let mut invalid = advertisement("peer");
        invalid.alias.clear();
        assert!(matches!(
            discovery.publish(&invalid),
            Err(DiscoveryError::MissingField("alias"))
        ));
        invalid.alias = "peer".into();
        invalid.api_version += 1;
        assert!(matches!(
            discovery.publish(&invalid),
            Err(DiscoveryError::UnsupportedVersion(_))
        ));
    }

    #[test]
    fn chooses_a_stable_non_loopback_resolved_address() {
        let addresses = [
            "127.0.0.1".parse().unwrap(),
            "192.0.2.9".parse().unwrap(),
            "192.0.2.3".parse().unwrap(),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            preferred_address(&addresses, 8742).as_deref(),
            Some("192.0.2.3:8742")
        );
    }
}

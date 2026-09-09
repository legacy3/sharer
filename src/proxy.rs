//! Detection of local SOCKS5 services on Tor's standard ports.

use std::{
    io::{Read as _, Write as _},
    net::SocketAddr,
    time::Duration,
};

use socket2::{Domain, Protocol, Socket, Type};

use crate::upload::UploadRoute;

const PROBE_TIMEOUT: Duration = Duration::from_millis(150);
const TOR_PORTS: [u16; 2] = [9050, 9150];

trait SocksProbe {
    fn is_socks5(&self, address: SocketAddr) -> bool;
}

#[derive(Clone, Copy, Debug)]
struct SystemSocksProbe;

impl SocksProbe for SystemSocksProbe {
    fn is_socks5(&self, address: SocketAddr) -> bool {
        let Ok(mut socket) = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP)) else {
            return false;
        };

        if socket.set_read_timeout(Some(PROBE_TIMEOUT)).is_err()
            || socket.set_write_timeout(Some(PROBE_TIMEOUT)).is_err()
            || socket
                .connect_timeout(&address.into(), PROBE_TIMEOUT)
                .is_err()
            || socket.write_all(&[5, 1, 0]).is_err()
        {
            return false;
        }

        let mut response = [0_u8; 2];

        socket.read_exact(&mut response).is_ok() && response == [5, 0]
    }
}

/// A verified local SOCKS5 endpoint on a standard Tor port.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TorProxy {
    url: String,
}

impl TorProxy {
    /// Detect a local Tor-compatible SOCKS5 endpoint.
    #[must_use]
    pub fn detect() -> Option<Self> {
        detect_with(&SystemSocksProbe)
    }

    /// Return the `socks5h` URL, which keeps DNS resolution inside the proxy.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }
}

/// Prefer a detected Tor proxy, optionally refusing a direct fallback.
///
/// # Errors
///
/// Returns an error when Tor is required but no standard local endpoint responds.
pub fn preferred_upload_route(require_tor: bool) -> Result<UploadRoute, TorUnavailableError> {
    if let Some(proxy) = TorProxy::detect() {
        Ok(UploadRoute::Socks5(proxy.url().to_owned()))
    } else if require_tor {
        Err(TorUnavailableError)
    } else {
        Ok(UploadRoute::Direct)
    }
}

/// No verified local Tor SOCKS5 endpoint is available.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("Tor is required, but no SOCKS5 service was found on localhost:9050 or :9150")]
pub struct TorUnavailableError;

fn detect_with(probe: &impl SocksProbe) -> Option<TorProxy> {
    TOR_PORTS.iter().find_map(|port| {
        let address = SocketAddr::from(([127, 0, 0, 1], *port));

        probe.is_socks5(address).then(|| TorProxy {
            url: format!("socks5h://{address}"),
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct FixedProbe {
        open_port: u16,
    }

    impl SocksProbe for FixedProbe {
        fn is_socks5(&self, address: SocketAddr) -> bool {
            address.port() == self.open_port
        }
    }

    #[test]
    fn prefers_the_tor_service_port() {
        let proxy = detect_with(&FixedProbe { open_port: 9050 }).unwrap();

        assert_eq!(proxy.url(), "socks5h://127.0.0.1:9050");
    }

    #[test]
    fn detects_the_tor_browser_port() {
        let proxy = detect_with(&FixedProbe { open_port: 9150 }).unwrap();

        assert_eq!(proxy.url(), "socks5h://127.0.0.1:9150");
    }
}

//! Bound installer DNS work even when the operating system cannot cancel a lookup.

use std::{
    io,
    net::{IpAddr, SocketAddr, ToSocketAddrs as _},
    sync::{
        Arc, LazyLock,
        atomic::{AtomicBool, Ordering},
    },
};

use reqwest::dns::{Addrs, Name, Resolve, Resolving};

static LOOKUP_IN_FLIGHT: LazyLock<Arc<AtomicBool>> =
    LazyLock::new(|| Arc::new(AtomicBool::new(false)));

pub(super) struct InstallerResolver;

impl Resolve for InstallerResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str();

        // Numeric proxy addresses need no OS lookup and must remain usable
        // while an earlier direct connection's DNS request is still finishing.
        if let Ok(address) = host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<IpAddr>()
        {
            let addresses: Addrs = Box::new(std::iter::once(SocketAddr::new(address, 0)));

            return Box::pin(std::future::ready(Ok(addresses)));
        }

        resolve_with(host.to_owned(), Arc::clone(&LOOKUP_IN_FLIGHT), |host| {
            (host.as_str(), 0).to_socket_addrs().map(Iterator::collect)
        })
    }
}

struct LookupPermit(Arc<AtomicBool>);

impl LookupPermit {
    fn acquire(in_flight: Arc<AtomicBool>) -> io::Result<Self> {
        if in_flight
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "A previous FFmpeg network address lookup is still finishing. Wait a moment, then retry the download",
            ));
        }

        Ok(Self(in_flight))
    }
}

impl Drop for LookupPermit {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

struct AbortLookup(tokio::task::AbortHandle);

impl Drop for AbortLookup {
    fn drop(&mut self) {
        // Aborting prevents queued work from starting. A running lookup cannot be
        // stopped, so its permit remains inside the blocking closure instead.
        self.0.abort();
    }
}

fn resolve_with(
    host: String,
    in_flight: Arc<AtomicBool>,
    lookup: impl FnOnce(String) -> io::Result<Vec<SocketAddr>> + Send + 'static,
) -> Resolving {
    Box::pin(async move {
        // Admission happens before spawning, so repeated Cancel/Retry cannot
        // accumulate either blocking threads or queued resolver allocations.
        let permit = LookupPermit::acquire(in_flight)?;
        let task = tokio::task::spawn_blocking(move || {
            let _permit = permit;

            lookup(host)
        });
        let _abort = AbortLookup(task.abort_handle());
        let addresses = task.await.map_err(|error| {
            io::Error::other(format!("FFmpeg address lookup failed: {error}"))
        })??;

        Ok(Box::new(addresses.into_iter()) as Addrs)
    })
}

#[cfg(test)]
mod tests;

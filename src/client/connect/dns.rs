//! DNS Resolution used by the `HttpConnector`.
//!
//! This module contains:
//!
//! - A [`GaiResolver`](dns::GaiResolver) that is the default resolver for the
//!   `HttpConnector`.
//! - The `Name` type used as an argument to custom resolvers.
//!
//! # Resolvers are `Service`s
//!
//! A resolver is just a
//! `Service<Name, Response = impl Iterator<Item = IpAddr>>`.
//!
//! A simple resolver that ignores the name and always returns a specific
//! address:
//!
//! ```rust,ignore
//! use std::{convert::Infallible, iter, net::IpAddr};
//!
//! let resolver = tower::service_fn(|_name| async {
//!     Ok::<_, Infallible>(iter::once(IpAddr::from([127, 0, 0, 1])))
//! });
//! ```
use std::error::Error;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6, ToSocketAddrs};
use std::str::FromStr;
use std::task::{self, Poll};
use std::pin::Pin;
use std::future::Future;
use std::{fmt, io, vec};

use tokio::task::JoinHandle;
use tower_service::Service;

pub(super) use self::sealed::Resolve;

/// A domain name to resolve into IP addresses.
#[derive(Clone, Hash, Eq, PartialEq)]
pub struct Name {
    host: String,
}

/// A resolver using blocking `getaddrinfo` calls in a threadpool.
#[derive(Clone)]
pub struct GaiResolver {
    _priv: (),
}

/// An iterator of IP addresses returned from `getaddrinfo`.
pub struct GaiAddrs {
    inner: IpAddrs,
}

/// A future to resolve a name returned by `GaiResolver`.
pub struct GaiFuture {
    inner: JoinHandle<Result<IpAddrs, io::Error>>,
}

impl Name {
    pub(super) fn new(host: String) -> Name {
        Name { host }
    }

    /// View the hostname as a string slice.
    pub fn as_str(&self) -> &str {
        &self.host
    }
}

impl fmt::Debug for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.host, f)
    }
}

impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.host, f)
    }
}

impl FromStr for Name {
    type Err = InvalidNameError;

    fn from_str(host: &str) -> Result<Self, Self::Err> {
        // Possibly add validation later
        Ok(Name::new(host.to_owned()))
    }
}

/// Error indicating a given string was not a valid domain name.
#[derive(Debug)]
pub struct InvalidNameError(());

impl fmt::Display for InvalidNameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Not a valid domain name")
    }
}

impl Error for InvalidNameError {}

impl GaiResolver {
    /// Construct a new `GaiResolver`.
    pub fn new() -> Self {
        GaiResolver { _priv: () }
    }
}

impl Service<Name> for GaiResolver {
    type Response = GaiAddrs;
    type Error = io::Error;
    type Future = GaiFuture;

    fn poll_ready(&mut self, _cx: &mut task::Context<'_>) -> Poll<Result<(), io::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, name: Name) -> Self::Future {
        let blocking = tokio::task::spawn_blocking(move || {
            debug!("resolving host={:?}", name.host);
            (&*name.host, 0)
                .to_socket_addrs()
                .map(|i| IpAddrs { iter: i })
        });

        GaiFuture { inner: blocking }
    }
}

impl fmt::Debug for GaiResolver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad("GaiResolver")
    }
}

impl Future for GaiFuture {
    type Output = Result<GaiAddrs, io::Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.inner).poll(cx).map(|res| match res {
            Ok(Ok(addrs)) => Ok(GaiAddrs { inner: addrs }),
            Ok(Err(err)) => Err(err),
            Err(join_err) => panic!("gai background task failed: {:?}", join_err),
        })
    }
}

impl fmt::Debug for GaiFuture {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad("GaiFuture")
    }
}

impl Iterator for GaiAddrs {
    type Item = IpAddr;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|sa| sa.ip())
    }
}

impl fmt::Debug for GaiAddrs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad("GaiAddrs")
    }
}

pub(super) struct IpAddrs {
    iter: vec::IntoIter<SocketAddr>,
}

impl IpAddrs {
    pub(super) fn new(addrs: Vec<SocketAddr>) -> Self {
        IpAddrs {
            iter: addrs.into_iter(),
        }
    }

    pub(super) fn try_parse(host: &str, port: u16) -> Option<IpAddrs> {
        if let Ok(addr) = host.parse::<Ipv4Addr>() {
            let addr = SocketAddrV4::new(addr, port);
            return Some(IpAddrs {
                iter: vec![SocketAddr::V4(addr)].into_iter(),
            });
        }
        let host = host.trim_start_matches('[').trim_end_matches(']');
        if let Ok(addr) = host.parse::<Ipv6Addr>() {
            let addr = SocketAddrV6::new(addr, port, 0, 0);
            return Some(IpAddrs {
                iter: vec![SocketAddr::V6(addr)].into_iter(),
            });
        }
        None
    }

    pub(super) fn split_by_preference(self, local_addr: Option<IpAddr>) -> (IpAddrs, IpAddrs) {
        if let Some(local_addr) = local_addr {
            let preferred = self
                .iter
                .filter(|addr| addr.is_ipv6() == local_addr.is_ipv6())
                .collect();

            (IpAddrs::new(preferred), IpAddrs::new(vec![]))
        } else {
            let preferring_v6 = self
                .iter
                .as_slice()
                .first()
                .map(SocketAddr::is_ipv6)
                .unwrap_or(false);

            let (preferred, fallback) = self
                .iter
                .partition::<Vec<_>, _>(|addr| addr.is_ipv6() == preferring_v6);

            (IpAddrs::new(preferred), IpAddrs::new(fallback))
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.iter.as_slice().is_empty()
    }

    pub(super) fn len(&self) -> usize {
        self.iter.as_slice().len()
    }
}

impl Iterator for IpAddrs {
    type Item = SocketAddr;
    #[inline]
    fn next(&mut self) -> Option<SocketAddr> {
        self.iter.next()
    }
}

/*
/// A resolver using `getaddrinfo` calls via the `tokio_executor::threadpool::blocking` API.
///
/// Unlike the `GaiResolver` this will not spawn dedicated threads, but only works when running on the
/// multi-threaded Tokio runtime.
#[cfg(feature = "runtime")]
#[derive(Clone, Debug)]
pub struct TokioThreadpoolGaiResolver(());

/// The future returned by `TokioThreadpoolGaiResolver`.
#[cfg(feature = "runtime")]
#[derive(Debug)]
pub struct TokioThreadpoolGaiFuture {
    name: Name,
}

#[cfg(feature = "runtime")]
impl TokioThreadpoolGaiResolver {
    /// Creates a new DNS resolver that will use tokio threadpool's blocking
    /// feature.
    ///
    /// **Requires** its futures to be run on the threadpool runtime.
    pub fn new() -> Self {
        TokioThreadpoolGaiResolver(())
    }
}

#[cfg(feature = "runtime")]
impl Service<Name> for TokioThreadpoolGaiResolver {
    type Response = GaiAddrs;
    type Error = io::Error;
    type Future = TokioThreadpoolGaiFuture;

    fn poll_ready(&mut self, _cx: &mut task::Context<'_>) -> Poll<Result<(), io::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, name: Name) -> Self::Future {
        TokioThreadpoolGaiFuture { name }
    }
}

#[cfg(feature = "runtime")]
impl Future for TokioThreadpoolGaiFuture {
    type Output = Result<GaiAddrs, io::Error>;

    fn poll(self: Pin<&mut Self>, _cx: &mut task::Context<'_>) -> Poll<Self::Output> {
        match ready!(tokio_executor::threadpool::blocking(|| (
            self.name.as_str(),
            0
        )
            .to_socket_addrs()))
        {
            Ok(Ok(iter)) => Poll::Ready(Ok(GaiAddrs {
                inner: IpAddrs { iter },
            })),
            Ok(Err(e)) => Poll::Ready(Err(e)),
            // a BlockingError, meaning not on a tokio_executor::threadpool :(
            Err(e) => Poll::Ready(Err(io::Error::new(io::ErrorKind::Other, e))),
        }
    }
}
*/

mod sealed {
    use super::{IpAddr, Name};
    use crate::common::{task, Future, Poll};
    use tower_service::Service;

    // "Trait alias" for `Service<Name, Response = Addrs>`
    pub trait Resolve {
        type Addrs: Iterator<Item = IpAddr>;
        type Error: Into<Box<dyn std::error::Error + Send + Sync>>;
        type Future: Future<Output = Result<Self::Addrs, Self::Error>>;

        fn poll_ready(&mut self, cx: &mut task::Context<'_>) -> Poll<Result<(), Self::Error>>;
        fn resolve(&mut self, name: Name) -> Self::Future;
    }

    impl<S> Resolve for S
    where
        S: Service<Name>,
        S::Response: Iterator<Item = IpAddr>,
        S::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    {
        type Addrs = S::Response;
        type Error = S::Error;
        type Future = S::Future;

        fn poll_ready(&mut self, cx: &mut task::Context<'_>) -> Poll<Result<(), Self::Error>> {
            Service::poll_ready(self, cx)
        }

        fn resolve(&mut self, name: Name) -> Self::Future {
            Service::call(self, name)
        }
    }
}

pub(crate) async fn resolve<R>(resolver: &mut R, name: Name) -> Result<R::Addrs, R::Error>
where
    R: Resolve,
{
    futures_util::future::poll_fn(|cx| resolver.poll_ready(cx)).await?;
    resolver.resolve(name).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn test_ip_addrs_split_by_preference() {
        let v4_addr = (Ipv4Addr::new(127, 0, 0, 1), 80).into();
        let v6_addr = (Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 1), 80).into();

        let (mut preferred, mut fallback) = IpAddrs {
            iter: vec![v4_addr, v6_addr].into_iter(),
        }
        .split_by_preference(None);
        assert!(preferred.next().unwrap().is_ipv4());
        assert!(fallback.next().unwrap().is_ipv6());

        let (mut preferred, mut fallback) = IpAddrs {
            iter: vec![v6_addr, v4_addr].into_iter(),
        }
        .split_by_preference(None);
        assert!(preferred.next().unwrap().is_ipv6());
        assert!(fallback.next().unwrap().is_ipv4());

        let (mut preferred, fallback) = IpAddrs {
            iter: vec![v4_addr, v6_addr].into_iter(),
        }
        .split_by_preference(Some(v4_addr.ip()));
        assert!(preferred.next().unwrap().is_ipv4());
        assert!(fallback.is_empty());

        let (mut preferred, fallback) = IpAddrs {
            iter: vec![v4_addr, v6_addr].into_iter(),
        }
        .split_by_preference(Some(v6_addr.ip()));
        assert!(preferred.next().unwrap().is_ipv6());
        assert!(fallback.is_empty());
    }

    #[test]
    fn test_name_from_str() {
        const DOMAIN: &str = "test.example.com";
        let name = Name::from_str(DOMAIN).expect("Should be a valid domain");
        assert_eq!(name.as_str(), DOMAIN);
        assert_eq!(name.to_string(), DOMAIN);
    }

    #[test]
    fn ip_addrs_try_parse_v6() {
        let uri = ::http::Uri::from_static("http://[::1]:8080/");
        let dst = super::super::Destination { uri };

        let mut addrs =
            IpAddrs::try_parse(dst.host(), dst.port().expect("port")).expect("try_parse");

        let expected = "[::1]:8080".parse::<SocketAddr>().expect("expected");

        assert_eq!(addrs.next(), Some(expected));
    }
}

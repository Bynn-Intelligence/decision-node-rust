//! Transport settings for the HTTP client the SDK builds.

use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use crate::error::{Error, ErrorKind, Result};

type Step = Box<dyn FnOnce(reqwest::ClientBuilder) -> reqwest::ClientBuilder + Send + Sync>;

/// Transport settings for the HTTP client the SDK builds, passed to
/// [`crate::ClientBuilder::http_settings`].
///
/// The SDK builds its own `reqwest` client and takes no client of your
/// making: it could not show that such a client refuses redirects or keeps
/// its TLS checks on. Every setting here keeps both. The client never follows
/// a redirect, and certificate and hostname checks stay on: there is no
/// setting that turns them off, and a client certificate or extra root
/// certificates add to the checks rather than replace them.
///
/// The settings apply to every HTTP call, uploads to storage included. A
/// session's WebSocket connects directly with the platform's root
/// certificates.
///
/// ```
/// use std::time::Duration;
/// use decisionnode::{reqwest, HttpSettings};
///
/// let settings = HttpSettings::new()
///     .proxy(reqwest::Proxy::https("http://proxy.example:3128").unwrap())
///     .connect_timeout(Duration::from_secs(3))
///     .pool_max_idle_per_host(4);
/// let dn = decisionnode::DecisionNode::builder().http_settings(settings).build()?;
/// # Ok::<(), decisionnode::Error>(())
/// ```
#[derive(Default)]
#[must_use]
pub struct HttpSettings {
    names: Vec<&'static str>,
    steps: Vec<Step>,
}

impl fmt::Debug for HttpSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpSettings").field("set", &self.names).finish()
    }
}

impl HttpSettings {
    /// The defaults of `reqwest`, with redirects off.
    pub fn new() -> HttpSettings {
        HttpSettings::default()
    }

    fn step(
        mut self,
        name: &'static str,
        f: impl FnOnce(reqwest::ClientBuilder) -> reqwest::ClientBuilder + Send + Sync + 'static,
    ) -> HttpSettings {
        self.names.push(name);
        self.steps.push(Box::new(f));
        self
    }

    /// Sends through a proxy. An `https` request goes through the proxy in a
    /// tunnel, so TLS still runs end to end with the API.
    pub fn proxy(self, proxy: reqwest::Proxy) -> HttpSettings {
        self.step("proxy", move |b| b.proxy(proxy))
    }

    /// Ignores proxies from the environment (`HTTPS_PROXY` and the others).
    pub fn no_proxy(self) -> HttpSettings {
        self.step("no_proxy", |b| b.no_proxy())
    }

    /// The longest wait for a connection to open. The client's own timeout
    /// still covers the whole attempt.
    pub fn connect_timeout(self, timeout: Duration) -> HttpSettings {
        self.step("connect_timeout", move |b| b.connect_timeout(timeout))
    }

    /// How long an idle connection is kept in the pool; `None` keeps it.
    pub fn pool_idle_timeout(self, timeout: Option<Duration>) -> HttpSettings {
        self.step("pool_idle_timeout", move |b| b.pool_idle_timeout(timeout))
    }

    /// The most idle connections kept per host.
    pub fn pool_max_idle_per_host(self, max: usize) -> HttpSettings {
        self.step("pool_max_idle_per_host", move |b| b.pool_max_idle_per_host(max))
    }

    /// TCP keepalive on each connection; `None` turns it off.
    pub fn tcp_keepalive(self, interval: Option<Duration>) -> HttpSettings {
        self.step("tcp_keepalive", move |b| b.tcp_keepalive(interval))
    }

    /// Sets `TCP_NODELAY` on each connection.
    pub fn tcp_nodelay(self, enabled: bool) -> HttpSettings {
        self.step("tcp_nodelay", move |b| b.tcp_nodelay(enabled))
    }

    /// The local address to connect from.
    pub fn local_address(self, address: IpAddr) -> HttpSettings {
        self.step("local_address", move |b| b.local_address(address))
    }

    /// Uses HTTP/1.1 only.
    pub fn http1_only(self) -> HttpSettings {
        self.step("http1_only", |b| b.http1_only())
    }

    /// Resolves `domain` to `address` instead of asking DNS. The certificate
    /// is still checked against `domain`.
    pub fn resolve(self, domain: impl Into<String>, address: SocketAddr) -> HttpSettings {
        let domain = domain.into();
        self.step("resolve", move |b| b.resolve(&domain, address))
    }

    /// Trusts one more root certificate, for example a company proxy's.
    /// Certificates are still checked.
    #[cfg(any(feature = "rustls-tls", feature = "native-tls"))]
    pub fn add_root_certificate(self, certificate: reqwest::Certificate) -> HttpSettings {
        self.step("add_root_certificate", move |b| b.add_root_certificate(certificate))
    }

    /// Whether the built-in root certificates are trusted (default `true`).
    /// Turn them off to trust only the roots you add with
    /// [`HttpSettings::add_root_certificate`]; certificates are still checked.
    #[cfg(any(feature = "rustls-tls", feature = "native-tls"))]
    pub fn tls_built_in_root_certs(self, enabled: bool) -> HttpSettings {
        self.step("tls_built_in_root_certs", move |b| b.tls_built_in_root_certs(enabled))
    }

    /// A client certificate, for a proxy or gateway that asks for one.
    #[cfg(any(feature = "rustls-tls", feature = "native-tls"))]
    pub fn identity(self, identity: reqwest::Identity) -> HttpSettings {
        self.step("identity", move |b| b.identity(identity))
    }

    /// Builds the client: the settings, then the rules no setting can change.
    pub(crate) fn build(self, https: bool) -> Result<reqwest::Client> {
        let mut builder = reqwest::Client::builder();
        for step in self.steps {
            builder = step(builder);
        }
        // Never follow a redirect, so the key never reaches another URL.
        builder = builder.redirect(reqwest::redirect::Policy::none());
        #[cfg(any(feature = "rustls-tls", feature = "native-tls"))]
        {
            builder = builder.danger_accept_invalid_certs(false).danger_accept_invalid_hostnames(false);
        }
        if https {
            builder = builder.https_only(true);
        }
        builder
            .build()
            .map_err(|e| Error::new(ErrorKind::Connection, "the HTTP client could not be created").with_source(e))
    }
}

//! Routing and the middleware chain.
//!
//! Routing decisions belong to Gatel, not to Salvo's router: the ordering rules
//! are part of the configuration contract, they are decided once at compile
//! time, and `gatel explain` must be able to print the same table the request
//! path walks. Salvo supplies `Request`, `Response`, `Handler` and `FlowCtrl`;
//! this module supplies the route table.

use std::net::IpAddr;
use std::sync::Arc;

use gatel_model::ResourceId;
use gatel_observe::metrics::{Listener as ListenerMetrics, Route as RouteMetrics};
use gatel_plan::{HeaderLookup, HostMatch, MatchContext, Matcher, MethodMatch, PathMatch};
use salvo_core::http::{Request, Response, StatusCode};
use salvo_core::{Depot, FlowCtrl, Handler, async_trait};

/// One route, with its chain already built into handlers.
pub struct PreparedRoute {
    pub id: ResourceId,
    pub execution: gatel_plan::ExecutionIdentity,
    pub path: PathMatch,
    pub methods: MethodMatch,
    pub matchers: Vec<Matcher>,
    /// Middleware in execution order, with the terminal last. Built once per
    /// generation; a request never assembles a chain.
    pub chain: Vec<Arc<dyn Handler>>,
    pub terminal_name: &'static str,
    /// Resolved once, during prepare. The request path holds this handle and
    /// has no way to name a different one.
    pub metrics: Arc<RouteMetrics>,
}

impl std::fmt::Debug for PreparedRoute {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedRoute")
            .field("id", &self.id.name())
            .field("path", &self.path.describe())
            .field("methods", &self.methods.describe())
            .field("chain", &self.chain.len())
            .finish()
    }
}

impl PreparedRoute {
    fn accepts(&self, context: &MatchContext<'_>) -> bool {
        self.path.matches(context.path)
            && self.methods.matches(context.method)
            && self.matchers.iter().all(|matcher| matcher.matches(context))
    }
}

pub struct PreparedSite {
    pub fallback_execution: gatel_plan::ExecutionIdentity,
    pub id: ResourceId,
    pub host: HostMatch,
    pub routes: Vec<PreparedRoute>,
    /// The site's own directives, plus a terminal that writes the 404, for a
    /// request that reached this site and matched none of its routes.
    ///
    /// A site-wide `access-log` should see a request to an unknown path — those
    /// are the ones worth seeing — and a site-wide `error-pages` should be able
    /// to cover its own not-found.
    pub fallback: Vec<Arc<dyn Handler>>,
}

impl std::fmt::Debug for PreparedSite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedSite")
            .field("id", &self.id.name())
            .field("host", &self.host.describe())
            .field("routes", &self.routes)
            .field("fallback", &self.fallback.len())
            .finish()
    }
}

/// The route table for one listener, most specific first.
#[derive(Debug)]
pub struct Dispatcher {
    pub sites: Vec<PreparedSite>,
    /// Whether this listener terminates TLS, which `match tls=` reads.
    pub tls: bool,
    /// Networks whose forwarding headers are believed.
    pub trusted_proxies: Vec<(IpAddr, u8)>,
    /// The largest request body anything behind this listener will read.
    pub max_body: u64,
    /// Outstanding ACME challenges, answered before anything else looks at the
    /// request.
    pub challenges: Arc<gatel_tls::Challenges>,
    pub metrics: Arc<ListenerMetrics>,
}

/// Why a request did not reach a terminal, so the two cases can be told apart
/// in a log instead of both showing as 404.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoRoute {
    /// No site claims this host.
    UnknownHost,
    /// The site exists but no route matched.
    NoMatch,
}

/// What routing found when it did not find a route.
///
/// The site is carried out, because a request that reached a site is still that
/// site's: its directives should see it even though none of its routes claimed
/// it.
#[derive(Debug)]
pub enum NoMatch<'a> {
    /// No site claims this host, so no site's directives apply.
    Host,
    /// This site claims the host, but none of its routes matched.
    Site(&'a PreparedSite),
}

struct SalvoHeaders<'a>(&'a Request);

impl HeaderLookup for SalvoHeaders<'_> {
    fn get(&self, name: &str) -> Option<&str> {
        self.0.headers().get(name).and_then(|value| value.to_str().ok())
    }

    fn cookie(&self, name: &str) -> Option<&str> {
        let header = self.get("cookie")?;
        header.split(';').find_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            (key.trim() == name).then_some(value.trim())
        })
    }
}

impl Dispatcher {
    /// The site and route that claim a request, or why none did.
    pub fn resolve<'a>(
        &'a self,
        request: &Request,
    ) -> Result<(&'a PreparedSite, &'a PreparedRoute), NoMatch<'a>> {
        let host = request_host(request);
        let method = request.method().as_str();
        let uri = request.uri();
        let path = uri.path();
        let query = uri.query();
        let client_ip = self.client_ip(request);

        let headers = SalvoHeaders(request);
        let context = MatchContext {
            host: &host,
            method,
            path,
            query,
            headers: &headers,
            client_ip,
            tls: self.tls,
        };

        let site = self
            .sites
            .iter()
            .find(|site| site.host.matches(&host))
            .ok_or(NoMatch::Host)?;
        let route = site
            .routes
            .iter()
            .find(|route| route.accepts(&context))
            .ok_or(NoMatch::Site(site))?;
        Ok((site, route))
    }

    /// The address to attribute the request to.
    ///
    /// A forwarding header is only believed when the immediate peer is a
    /// configured trusted proxy; otherwise any client could choose its own
    /// address and defeat every ip-based decision.
    pub fn client_ip(&self, request: &Request) -> Option<IpAddr> {
        let peer = peer_ip(request)?;
        if !is_trusted(peer, &self.trusted_proxies) {
            return Some(peer);
        }
        request
            .headers()
            .get("x-forwarded-for")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(',').next_back())
            .and_then(|value| value.trim().parse::<IpAddr>().ok())
            .or(Some(peer))
    }
}

/// Whether an address is one the configuration lets speak for somebody else.
///
/// Shared with the accept loop, which asks the same question about a PROXY
/// header before there is a request to ask it about.
pub fn is_trusted(address: IpAddr, trusted: &[(IpAddr, u8)]) -> bool {
    trusted
        .iter()
        .any(|(network, prefix)| in_network(address, *network, *prefix))
}

fn peer_ip(request: &Request) -> Option<IpAddr> {
    request
        .remote_addr()
        .clone()
        .into_std()
        .map(|address| address.ip())
}

fn in_network(ip: IpAddr, network: IpAddr, prefix: u8) -> bool {
    fn masked(bytes: &[u8], prefix: u8) -> Vec<u8> {
        let mut out = bytes.to_vec();
        let full = (prefix / 8) as usize;
        for (index, byte) in out.iter_mut().enumerate() {
            if index < full {
                continue;
            }
            if index == full {
                let remainder = prefix % 8;
                *byte &= if remainder == 0 {
                    0
                } else {
                    0xffu8 << (8 - remainder)
                };
            } else {
                *byte = 0;
            }
        }
        out
    }
    match (ip, network) {
        (IpAddr::V4(a), IpAddr::V4(b)) => masked(&a.octets(), prefix) == masked(&b.octets(), prefix),
        (IpAddr::V6(a), IpAddr::V6(b)) => masked(&a.octets(), prefix) == masked(&b.octets(), prefix),
        _ => false,
    }
}

/// The host a request is for: the `Host` header, or the authority for
/// HTTP/2, lowercased and without the port.
pub fn request_host(request: &Request) -> String {
    let raw = request
        .headers()
        .get(http::header::HOST)
        .and_then(|value| value.to_str().ok())
        .or_else(|| request.uri().host())
        .unwrap_or_default();
    let without_port = match raw.rsplit_once(':') {
        // Leave IPv6 literals alone; only a trailing `:port` is stripped.
        Some((head, tail)) if !head.ends_with(']') && tail.chars().all(|c| c.is_ascii_digit()) => head,
        _ => raw,
    };
    without_port
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase()
}

/// What Gatel answers when no route claimed the request.
///
/// Deliberately not Salvo's default catcher page: the body of a 404 is part of
/// what a gateway promises, and must not change with a dependency.
pub fn write_not_found(response: &mut Response, reason: NoRoute) {
    response.status_code(StatusCode::NOT_FOUND);
    let _ = response.write_body(match reason {
        NoRoute::UnknownHost => "404 no site is configured for this host\n",
        NoRoute::NoMatch => "404 no route matched\n",
    });
}

#[async_trait]
impl Handler for Dispatcher {
    async fn handle(
        &self,
        request: &mut Request,
        depot: &mut Depot,
        response: &mut Response,
        _ctrl: &mut FlowCtrl,
    ) {
        // The canonical client address is decided once, here, where the
        // trusted-proxy policy lives. Everything downstream reads it rather
        // than looking at headers again.
        if let Some(client_ip) = self.client_ip(request) {
            depot.insert("gatel.client_ip", client_ip);
        }
        depot.insert("gatel.tls", self.tls);

        // Before routing, and before every guard. A certificate authority
        // asking for a challenge is not a visitor: `force-https` must not
        // redirect it, a rate limit must not refuse it, and no route needs to
        // exist for it. It is answered here or nowhere.
        if let Some(answer) = self.challenges.answer_for_path(request.uri().path()) {
            response.headers_mut().insert(
                http::header::CONTENT_TYPE,
                http::HeaderValue::from_static("application/octet-stream"),
            );
            let _ = response.write_body(answer);
            return;
        }

        // Before routing, because the limit belongs to the listener rather than
        // to whichever route happens to claim the request, and because a client
        // that declared too much should be told so without reading a byte.
        gatel_plan::RequestBodyLimit::install(depot, self.max_body);
        if let crate::body::Declared::TooLarge(declared) = crate::body::limit(request, self.max_body) {
            self.metrics.unmatched_requests.increment();
            response.status_code(StatusCode::PAYLOAD_TOO_LARGE);
            depot.insert("gatel.generated", true);
            let _ = response.write_body(format!(
                "413 request body of {declared} bytes is larger than the {} byte limit
",
                self.max_body
            ));
            return;
        }

        // No route means no series to attribute a request to, and a path is
        // not a label, so an unmatched request is counted on the listener.
        let (chain, measure) = match self.resolve(request) {
            Ok((site, route)) => {
                depot.insert(gatel_plan::ExecutionIdentity::DEPOT_KEY, route.execution);
                depot.insert("gatel.site", site.id.name().to_string());
                depot.insert("gatel.route", route.id.name().to_string());
                (
                    route.chain.clone(),
                    Some(crate::measure::Measure::start(Arc::clone(&route.metrics))),
                )
            }
            Err(NoMatch::Site(site)) => {
                depot.insert(gatel_plan::ExecutionIdentity::DEPOT_KEY, site.fallback_execution);
                self.metrics.unmatched_requests.increment();
                depot.insert("gatel.site", site.id.name().to_string());
                (site.fallback.clone(), None)
            }
            Err(NoMatch::Host) => {
                // No site claimed the host, so no site's directives apply and
                // there is nothing to run.
                self.metrics.unmatched_requests.increment();
                depot.insert("gatel.generated", true);
                write_not_found(response, NoRoute::UnknownHost);
                return;
            }
        };
        // A fresh flow for the route's own chain: middleware sees exactly the
        // handlers this route declared, in the order the compiler fixed.
        let mut flow = FlowCtrl::new(chain);
        flow.call_next(request, depot, response).await;
        // Outermost, so what is counted is the body that actually goes out.
        if let Some(measure) = measure {
            crate::measure::observe(response, measure);
        }
    }
}

/// The terminal of a site's fallback chain.
///
/// A handler rather than a branch in the dispatcher, so the site's directives
/// wrap it the way they wrap a real terminal — which is what makes a site-wide
/// access log see the request and a site-wide error page cover the answer.
pub struct NotFound;

#[async_trait]
impl Handler for NotFound {
    async fn handle(
        &self,
        _request: &mut Request,
        depot: &mut Depot,
        response: &mut Response,
        _ctrl: &mut FlowCtrl,
    ) {
        depot.insert("gatel.generated", true);
        write_not_found(response, NoRoute::NoMatch);
    }
}

#[cfg(test)]
mod tests {
    use gatel_model::ResourceKind;
    use salvo_core::http::header::HeaderValue;

    use super::*;

    fn request(host: &str, method: &str, path: &str) -> Request {
        let mut request = Request::default();
        *request.method_mut() = method.parse().unwrap();
        *request.uri_mut() = path.parse().unwrap();
        request
            .headers_mut()
            .insert(http::header::HOST, HeaderValue::from_str(host).unwrap());
        request
    }

    fn route(id: &str, path: &str, methods: MethodMatch, matchers: Vec<Matcher>) -> PreparedRoute {
        PreparedRoute {
            execution: gatel_plan::ExecutionIdentity(gatel_model::Hasher::new().finish()),
            id: ResourceId::new(ResourceKind::Route, id),
            path: PathMatch::parse(path).unwrap(),
            methods,
            matchers,
            chain: Vec::new(),
            terminal_name: "respond",
            metrics: Arc::new(RouteMetrics::default()),
        }
    }

    fn site(host: &str, routes: Vec<PreparedRoute>) -> PreparedSite {
        PreparedSite {
            fallback_execution: gatel_plan::ExecutionIdentity(gatel_model::Hasher::new().finish()),
            id: ResourceId::new(ResourceKind::Site, host),
            host: HostMatch::parse(host).unwrap(),
            routes,
            fallback: Vec::new(),
        }
    }

    fn dispatcher(sites: Vec<PreparedSite>) -> Dispatcher {
        Dispatcher {
            sites,
            tls: false,
            trusted_proxies: Vec::new(),
            max_body: u64::MAX,
            challenges: gatel_tls::Challenges::new(),
            metrics: Arc::new(ListenerMetrics::default()),
        }
    }

    #[test]
    fn the_first_matching_route_wins_because_the_table_is_pre_sorted() {
        let dispatcher = dispatcher(vec![site(
            "*",
            vec![
                route("exact", "/api/health", MethodMatch::Any, vec![]),
                route("prefix", "/api/*", MethodMatch::Any, vec![]),
                route("catch-all", "/*", MethodMatch::Any, vec![]),
            ],
        )]);
        let resolved = |path: &str| {
            dispatcher
                .resolve(&request("example.com", "GET", path))
                .unwrap()
                .1
                .id
                .name()
                .to_string()
        };
        assert_eq!(resolved("/api/health"), "exact");
        assert_eq!(resolved("/api/users"), "prefix");
        assert_eq!(resolved("/other"), "catch-all");
    }

    #[test]
    fn an_unclaimed_host_is_told_apart_from_an_unmatched_path() {
        let dispatcher = dispatcher(vec![site(
            "example.com",
            vec![route("only", "/", MethodMatch::Any, vec![])],
        )]);
        assert!(matches!(
            dispatcher.resolve(&request("other.com", "GET", "/")),
            Err(NoMatch::Host)
        ));
        // The site still claimed the host, so its directives still apply.
        assert!(matches!(
            dispatcher.resolve(&request("example.com", "GET", "/missing")),
            Err(NoMatch::Site(_))
        ));
    }

    #[test]
    fn a_host_header_with_a_port_still_matches() {
        let dispatcher = dispatcher(vec![site(
            "example.com",
            vec![route("r", "/", MethodMatch::Any, vec![])],
        )]);
        assert!(
            dispatcher
                .resolve(&request("Example.com:8443", "GET", "/"))
                .is_ok()
        );
    }

    #[test]
    fn method_narrows_a_route_without_hiding_the_next_one() {
        let dispatcher = dispatcher(vec![site(
            "*",
            vec![
                route(
                    "writes",
                    "/api/*",
                    MethodMatch::parse(&["POST".into()]).unwrap(),
                    vec![],
                ),
                route(
                    "reads",
                    "/api/*",
                    MethodMatch::parse(&["GET".into()]).unwrap(),
                    vec![],
                ),
            ],
        )]);
        assert_eq!(
            dispatcher
                .resolve(&request("h", "POST", "/api/x"))
                .unwrap()
                .1
                .id
                .name(),
            "writes"
        );
        assert_eq!(
            dispatcher
                .resolve(&request("h", "GET", "/api/x"))
                .unwrap()
                .1
                .id
                .name(),
            "reads"
        );
        assert!(matches!(
            dispatcher.resolve(&request("h", "DELETE", "/api/x")),
            Err(NoMatch::Site(_))
        ));
    }

    #[test]
    fn a_matcher_narrows_two_otherwise_identical_routes() {
        let dispatcher = dispatcher(vec![site(
            "*",
            vec![
                route(
                    "canary",
                    "/api/*",
                    MethodMatch::Any,
                    vec![Matcher::Header {
                        name: "x-canary".into(),
                        value: Some("1".into()),
                    }],
                ),
                route("stable", "/api/*", MethodMatch::Any, vec![]),
            ],
        )]);
        assert_eq!(
            dispatcher
                .resolve(&request("h", "GET", "/api/x"))
                .unwrap()
                .1
                .id
                .name(),
            "stable"
        );

        let mut tagged = request("h", "GET", "/api/x");
        tagged
            .headers_mut()
            .insert("x-canary", HeaderValue::from_static("1"));
        assert_eq!(dispatcher.resolve(&tagged).unwrap().1.id.name(), "canary");
    }

    #[test]
    fn cookies_are_read_out_of_the_cookie_header() {
        let dispatcher = dispatcher(vec![site(
            "*",
            vec![route(
                "beta",
                "/",
                MethodMatch::Any,
                vec![Matcher::Cookie {
                    name: "flight".into(),
                    value: Some("beta".into()),
                }],
            )],
        )]);
        let mut request = request("h", "GET", "/");
        request
            .headers_mut()
            .insert("cookie", HeaderValue::from_static("a=1; flight=beta; b=2"));
        assert!(dispatcher.resolve(&request).is_ok());
    }

    #[test]
    fn a_forwarding_header_is_ignored_unless_the_peer_is_trusted() {
        let mut request = request("h", "GET", "/");
        request
            .headers_mut()
            .insert("x-forwarded-for", HeaderValue::from_static("203.0.113.9"));
        *request.remote_addr_mut() = "198.51.100.5:1234"
            .parse::<std::net::SocketAddr>()
            .unwrap()
            .into();

        let untrusting = dispatcher(vec![]);
        assert_eq!(
            untrusting.client_ip(&request),
            Some("198.51.100.5".parse().unwrap())
        );

        let trusting = Dispatcher {
            max_body: u64::MAX,
            challenges: gatel_tls::Challenges::new(),
            metrics: Arc::new(ListenerMetrics::default()),
            sites: vec![],
            tls: false,
            trusted_proxies: vec![("198.51.100.0".parse().unwrap(), 24)],
        };
        assert_eq!(trusting.client_ip(&request), Some("203.0.113.9".parse().unwrap()));
    }

    #[test]
    fn a_trusted_proxy_that_sends_no_header_falls_back_to_the_peer() {
        let mut request = request("h", "GET", "/");
        *request.remote_addr_mut() = "198.51.100.5:1234"
            .parse::<std::net::SocketAddr>()
            .unwrap()
            .into();
        let trusting = Dispatcher {
            max_body: u64::MAX,
            challenges: gatel_tls::Challenges::new(),
            metrics: Arc::new(ListenerMetrics::default()),
            sites: vec![],
            tls: false,
            trusted_proxies: vec![("198.51.100.0".parse().unwrap(), 24)],
        };
        assert_eq!(
            trusting.client_ip(&request),
            Some("198.51.100.5".parse().unwrap())
        );
    }

    #[test]
    fn the_last_forwarded_hop_is_the_one_believed() {
        // Everything to the left was written by someone we do not control.
        let mut request = request("h", "GET", "/");
        request.headers_mut().insert(
            "x-forwarded-for",
            HeaderValue::from_static("1.2.3.4, 203.0.113.9"),
        );
        *request.remote_addr_mut() = "198.51.100.5:1234"
            .parse::<std::net::SocketAddr>()
            .unwrap()
            .into();
        let trusting = Dispatcher {
            max_body: u64::MAX,
            challenges: gatel_tls::Challenges::new(),
            metrics: Arc::new(ListenerMetrics::default()),
            sites: vec![],
            tls: false,
            trusted_proxies: vec![("198.51.100.0".parse().unwrap(), 24)],
        };
        assert_eq!(trusting.client_ip(&request), Some("203.0.113.9".parse().unwrap()));
    }
}

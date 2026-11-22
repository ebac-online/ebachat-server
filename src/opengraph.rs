//! What a link in a message shows: its OpenGraph preview (or a YouTube oEmbed), or, for a picture, its size.
//! Fetched once per URL and hour, kept in the link cache and the `LinkMetadata` table as Go does, through an HTTP
//! client that does not reach the internal network.

// Based on Mattermost server/channels/app/post_metadata.go (getLinkMetadata and helpers), app/opengraph.go,
// app/oembed, platform/link_cache.go, public/model/link_metadata.go, public/shared/httpservice, and
// github.com/dyatlov/go-opengraph (MIT), Copyright (c) 2015-present Mattermost, Inc. Licensed under AGPL-3.0.

use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::app::App;
use crate::file_api::LinkMeta;
use crate::images;

/// `MaxOpenGraphResponseSize` (and `MaxMetadataImageSize`).
const MAX_RESPONSE: usize = 50 * 1024 * 1024;
const MAX_IMAGES: usize = 5;
const USER_AGENT: &str = "Mattermost-Bot/1.1";

/// `opengraph.OpenGraph` after `TruncateOpenGraph`, in Go's field order.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct OpenGraph {
    pub r#type: String,
    pub url: String,
    pub title: String,
    pub description: String,
    pub determiner: String,
    pub site_name: String,
    pub locale: String,
    pub locales_alternate: Option<Vec<String>>,
    pub images: Option<Vec<OgImage>>,
    pub audios: Option<Vec<Value>>,
    pub videos: Option<Vec<Value>>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct OgImage {
    pub url: String,
    pub secure_url: String,
    pub r#type: String,
    pub width: u64,
    pub height: u64,
}

/// `model.PostImage`.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct PostImage {
    pub width: i64,
    pub height: i64,
    pub format: String,
    pub frame_count: i64,
}

// ---------- the HTTP client ----------

/// `IsReservedIP`: private, loopback, link-local, documentation, multicast and the like.
pub fn is_reserved(ip: IpAddr) -> bool {
    const V4: &[(&str, u8)] = &[
        ("10.0.0.0", 8),
        ("172.16.0.0", 12),
        ("192.168.0.0", 16),
        ("127.0.0.0", 8),
        ("0.0.0.0", 8),
        ("169.254.0.0", 16),
        ("192.0.0.0", 24),
        ("192.0.2.0", 24),
        ("198.51.100.0", 24),
        ("203.0.113.0", 24),
        ("192.88.99.0", 24),
        ("198.18.0.0", 15),
        ("224.0.0.0", 4),
        ("240.0.0.0", 4),
        ("255.255.255.255", 32),
        ("100.64.0.0", 10),
    ];
    const V6: &[(&str, u8)] = &[
        ("::", 128),
        ("::1", 128),
        ("100::", 64),
        ("2001::", 23),
        ("2001:2::", 48),
        ("2001:db8::", 32),
        ("2001::", 32),
        ("fc00::", 7),
        ("fe80::", 10),
        ("ff00::", 8),
        ("2002::", 16),
        ("64:ff9b::", 96),
        ("2001:10::", 28),
        ("2001:20::", 28),
    ];
    let ip = match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
        v4 => v4,
    };
    let list = if ip.is_ipv4() { V4 } else { V6 };
    list.iter().any(|(net, bits)| in_cidr(ip, net.parse().unwrap(), *bits))
}

fn in_cidr(ip: IpAddr, net: IpAddr, bits: u8) -> bool {
    match (ip, net) {
        (IpAddr::V4(a), IpAddr::V4(b)) => {
            let mask = if bits == 0 { 0 } else { u32::MAX << (32 - bits as u32) };
            u32::from(a) & mask == u32::from(b) & mask
        }
        (IpAddr::V6(a), IpAddr::V6(b)) => {
            let mask = if bits == 0 { 0 } else { u128::MAX << (128 - bits as u32) };
            u128::from(a) & mask == u128::from(b) & mask
        }
        _ => false,
    }
}

/// `AllowedUntrustedInternalConnections`: hosts and CIDRs, space or comma separated.
#[derive(Clone, Default)]
struct Allowed(Vec<String>);

/// `ErrAddressForbidden`.
const ADDRESS_FORBIDDEN: &str = "address forbidden, you may need to set AllowedUntrustedInternalConnections to allow \
                                 an integration access to your internal network";

impl Allowed {
    /// `isAllowedInternalHost`: the host (without brackets) is listed as it is.
    fn host(&self, host: &str) -> bool {
        let host = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host);
        self.0.iter().any(|a| a == host)
    }
    /// `checkInternalIP`: an address that is neither reserved nor this host's own, or one inside an allowed CIDR.
    /// The interfaces are listed on every check, as Go does; if they cannot be, nothing is allowed.
    fn check_ip(&self, ip: IpAddr) -> Result<(), String> {
        self.check_ip_with(ip, is_own_ip)
    }
    fn check_ip_with(&self, ip: IpAddr, own: impl Fn(IpAddr) -> std::io::Result<bool>) -> Result<(), String> {
        let ip = canonical_ip(ip);
        let reserved = is_reserved(ip);
        let own = own(ip).map_err(|e| format!("unable to determine if IP is own IP: {e}"))?;
        if !reserved && !own {
            return Ok(());
        }
        let in_allowed_range = self.0.iter().any(|a| {
            let Some((net, bits)) = a.split_once('/') else { return false };
            matches!((net.parse::<IpAddr>(), bits.parse::<u8>()), (Ok(n), Ok(b)) if in_cidr(ip, canonical_ip(n), b))
        });
        if in_allowed_range {
            return Ok(());
        }
        if reserved {
            return Err(format!("IP {ip} is in a reserved range and not in AllowedUntrustedInternalConnections"));
        }
        Err(format!("IP {ip} is a self-assigned IP and not in AllowedUntrustedInternalConnections"))
    }
    fn ip(&self, ip: IpAddr) -> bool {
        self.check_ip(ip).is_ok()
    }
    /// A URL whose host is an IP literal: allowed as a resolved address would be.
    fn url(&self, u: &Url) -> bool {
        let Some(host) = u.host_str() else { return false };
        if self.host(host) {
            return true;
        }
        match host.trim_matches(['[', ']']).parse::<IpAddr>() {
            Ok(ip) => self.ip(ip),
            Err(_) => true,
        }
    }
    /// `dialContextFilter`'s error for addresses that were all refused.
    fn forbidden(reasons: &[String]) -> String {
        if reasons.is_empty() {
            return ADDRESS_FORBIDDEN.to_string();
        }
        format!("{ADDRESS_FORBIDDEN}: {}", reasons.join("; "))
    }
}

/// An IPv4-mapped IPv6 address as its IPv4 address (Go's `net.IP` compares them equal).
fn canonical_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
        v4 => v4,
    }
}

/// `IsOwnIP`: an address of one of this host's interfaces (`getifaddrs`).
pub fn is_own_ip(ip: IpAddr) -> std::io::Result<bool> {
    let ip = canonical_ip(ip);
    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills `head` with a list that freeifaddrs releases below; every node and address is read
    // only while the list is alive, and each sockaddr is read as the type its family says.
    unsafe {
        if libc::getifaddrs(&mut head) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut found = false;
        let mut cur = head;
        while !cur.is_null() && !found {
            let sa = (*cur).ifa_addr;
            if !sa.is_null() {
                let own = match (*sa).sa_family as i32 {
                    libc::AF_INET => {
                        let a = &*(sa as *const libc::sockaddr_in);
                        Some(IpAddr::V4(std::net::Ipv4Addr::from(u32::from_be(a.sin_addr.s_addr))))
                    }
                    libc::AF_INET6 => {
                        let a = &*(sa as *const libc::sockaddr_in6);
                        Some(canonical_ip(IpAddr::V6(std::net::Ipv6Addr::from(a.sin6_addr.s6_addr))))
                    }
                    _ => None,
                };
                found = own == Some(ip);
            }
            cur = (*cur).ifa_next;
        }
        libc::freeifaddrs(head);
        Ok(found)
    }
}

/// Resolves names to the addresses `AllowedUntrustedInternalConnections` lets through (`dialContextFilter`).
struct FilteringResolver(Allowed);

impl reqwest::dns::Resolve for FilteringResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let allowed = self.0.clone();
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            if allowed.host(&host) {
                return Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs);
            }
            let mut reasons = vec![];
            let mut ok = vec![];
            for a in addrs {
                match allowed.check_ip(a.ip()) {
                    Ok(()) => ok.push(a),
                    Err(r) => reasons.push(r),
                }
            }
            if ok.is_empty() {
                return Err(Allowed::forbidden(&reasons).into());
            }
            Ok(Box::new(ok.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// The connect-time check of every untrusted client (`dialContextFilter`), below the resolver: hyper connects to
/// an IP-literal host without asking the resolver, so the resolver alone lets `http://127.0.0.1/` through. A
/// literal host is checked before dialling; then the address really connected to (`HttpInfo::remote_addr`, the
/// proxy's when there is one, as Go's dialer sees it) is checked again, so no caller has to remember a pre-check.
#[derive(Clone)]
struct ConnectGuard(Allowed);

#[derive(Clone)]
struct Guarded<S> {
    inner: S,
    allowed: Allowed,
}

impl<S> tower_layer::Layer<S> for ConnectGuard {
    type Service = Guarded<S>;
    fn layer(&self, inner: S) -> Guarded<S> {
        Guarded { inner, allowed: self.0.clone() }
    }
}

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// The host of reqwest's connector request, from its `Debug` (`Unnameable(<uri>)`): reqwest does not name the
/// type. `None` when it cannot be read, which allows no host by name.
fn connect_host(req: &impl std::fmt::Debug) -> Option<String> {
    let s = format!("{req:?}");
    let uri = s.strip_prefix("Unnameable(")?.strip_suffix(')')?;
    let uri: http::Uri = uri.parse().ok()?;
    let host = uri.host()?;
    Some(host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host).to_string())
}

impl<S, R> tower_service::Service<R> for Guarded<S>
where
    R: std::fmt::Debug,
    S: tower_service::Service<R, Error = BoxError>,
    S::Response: hyper_util::client::legacy::connect::Connection + Send + 'static,
    S::Future: Send + 'static,
{
    type Response = S::Response;
    type Error = BoxError;
    type Future = std::pin::Pin<Box<dyn std::future::Future<Output = Result<S::Response, BoxError>> + Send>>;

    fn poll_ready(&mut self, cx: &mut std::task::Context<'_>) -> std::task::Poll<Result<(), BoxError>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: R) -> Self::Future {
        let host = connect_host(&req);
        let allowed = self.allowed.clone();
        if host.as_deref().is_some_and(|h| allowed.host(h)) {
            return Box::pin(self.inner.call(req));
        }
        if let Some(Ok(ip)) = host.as_deref().map(str::parse::<IpAddr>)
            && let Err(reason) = allowed.check_ip(ip)
        {
            return Box::pin(std::future::ready(Err(Allowed::forbidden(&[reason]).into())));
        }
        let connecting = self.inner.call(req);
        Box::pin(async move {
            let conn = connecting.await?;
            let mut extras = http::Extensions::new();
            hyper_util::client::legacy::connect::Connection::connected(&conn).get_extras(&mut extras);
            let Some(peer) = extras.get::<hyper_util::client::legacy::connect::HttpInfo>().map(|i| i.remote_addr())
            else {
                return Err(Allowed::forbidden(&[]).into());
            };
            allowed.check_ip(peer.ip()).map_err(|r| Allowed::forbidden(&[r]))?;
            Ok(conn)
        })
    }
}

impl App {
    /// `HTTPService.MakeClient(false)` with `makeLinkMetadataClient`'s redirect rules: 3 s to connect, at most 10
    /// redirects, each allowed for previews and not internal.
    fn link_client(&self) -> &'static reqwest::Client {
        static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
        CLIENT.get_or_init(|| {
            let allowed = Allowed(
                self.cfg
                    .str("ServiceSettings.AllowedUntrustedInternalConnections")
                    .split(|c: char| c.is_whitespace() || c == ',')
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect(),
            );
            let restrict = self.cfg.str("ServiceSettings.RestrictLinkPreviews").to_string();
            let redirect_allowed = allowed.clone();
            reqwest::Client::builder()
                .user_agent(USER_AGENT)
                .connect_timeout(Duration::from_secs(3))
                .timeout(Duration::from_secs(30))
                .danger_accept_invalid_certs(self.cfg.bool("ServiceSettings.EnableInsecureOutgoingConnections"))
                .connector_layer(ConnectGuard(allowed.clone()))
                .dns_resolver(Arc::new(FilteringResolver(allowed)))
                .redirect(reqwest::redirect::Policy::custom(move |a| {
                    if a.previous().len() >= 10 {
                        a.error("stopped after 10 redirects")
                    } else if !link_allowed(&restrict, a.url().as_str()) || !redirect_allowed.url(a.url()) {
                        a.error("redirect target is disabled for link previews")
                    } else {
                        a.follow()
                    }
                }))
                .build()
                .expect("http client")
        })
    }

    fn allowed_untrusted(&self) -> Allowed {
        Allowed(
            self.cfg
                .str("ServiceSettings.AllowedUntrustedInternalConnections")
                .split(|c: char| c.is_whitespace() || c == ',')
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect(),
        )
    }

    /// `HTTPService.MakeClient(false)` for integrations: no internal addresses unless allowed, Go's redirects.
    pub fn integration_client(&self) -> &'static reqwest::Client {
        static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
        CLIENT.get_or_init(|| {
            let allowed = self.allowed_untrusted();
            let redirect_allowed = allowed.clone();
            reqwest::Client::builder()
                .user_agent(USER_AGENT)
                .connect_timeout(Duration::from_secs(30))
                .danger_accept_invalid_certs(self.cfg.bool("ServiceSettings.EnableInsecureOutgoingConnections"))
                .connector_layer(ConnectGuard(allowed.clone()))
                .dns_resolver(Arc::new(FilteringResolver(allowed)))
                .redirect(reqwest::redirect::Policy::custom(move |a| {
                    if a.previous().len() >= 10 {
                        a.error("stopped after 10 redirects")
                    } else if !redirect_allowed.url(a.url()) {
                        a.error("address forbidden")
                    } else {
                        a.follow()
                    }
                }))
                .build()
                .expect("http client")
        })
    }

    /// `HTTPService.MakeClient(false)` that never follows a redirect (`getRedirectLocation`).
    pub fn no_redirect_client(&self) -> &'static reqwest::Client {
        static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
        CLIENT.get_or_init(|| {
            let allowed = self.allowed_untrusted();
            reqwest::Client::builder()
                .user_agent(USER_AGENT)
                .connect_timeout(Duration::from_secs(30))
                .danger_accept_invalid_certs(self.cfg.bool("ServiceSettings.EnableInsecureOutgoingConnections"))
                .connector_layer(ConnectGuard(allowed.clone()))
                .dns_resolver(Arc::new(FilteringResolver(allowed)))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("http client")
        })
    }

    /// `HTTPService.MakeClient(true)`: this server's own plugin routes.
    pub fn trusted_client(&self) -> &'static reqwest::Client {
        static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
        CLIENT.get_or_init(|| {
            reqwest::Client::builder()
                .user_agent(USER_AGENT)
                .connect_timeout(Duration::from_secs(30))
                .danger_accept_invalid_certs(self.cfg.bool("ServiceSettings.EnableInsecureOutgoingConnections"))
                .build()
                .expect("http client")
        })
    }

    /// An IP-literal URL passes the same checks as a resolved address (the resolver never sees it).
    pub fn untrusted_url_allowed(&self, u: &Url) -> bool {
        self.allowed_untrusted().url(u)
    }

    /// `isLinkAllowedForPreview`: not on a `RestrictLinkPreviews` domain.
    pub fn link_allowed_for_preview(&self, link: &str) -> bool {
        link_allowed(self.cfg.str("ServiceSettings.RestrictLinkPreviews"), link)
    }

    /// `ImageProxyAdder`: an image URL through `/api/v4/image` when the image proxy is on.
    pub fn proxy_image_url(&self, u: &str) -> String {
        if !self.cfg.bool("ImageProxySettings.Enable") || u.is_empty() {
            return u.to_string();
        }
        let site = self.cfg.str("ServiceSettings.SiteURL").trim_end_matches('/');
        if !site.is_empty() && u.starts_with(site) {
            return u.to_string();
        }
        format!("{site}/api/v4/image?url={}", query_escape(u))
    }

    /// `getLinkMetadata`: the link cache, then (for an old post) the `LinkMetadata` table, then the web.
    /// `timestamp` is the post's creation; links are kept per hour of it.
    pub async fn link_metadata_at(&self, url: &str, timestamp: i64, is_new_post: bool) -> Option<Arc<LinkMeta>> {
        if !self.cfg.bool("ServiceSettings.EnableLinkPreviews") {
            return None;
        }
        let site = self.cfg.str("ServiceSettings.SiteURL");
        let url = resolve_url(url, site)?;
        if url.to_ascii_lowercase().starts_with("data:image/") {
            return None;
        }
        let ts = floor_to_hour(timestamp);
        let key = format!("{:x}", link_hash(&url, ts));
        if let Some(m) = self.caches.open_graph.get(&key)
            && m.url == url
        {
            return some(m);
        }
        if !is_new_post && let Some(m) = self.link_metadata_from_db(&url, ts).await {
            let m = Arc::new(m);
            self.caches.open_graph.put(key, m.clone());
            return some(m);
        }
        if looks_like_permalink(&url, site) && self.cfg.bool("ServiceSettings.EnablePermalinkPreviews") {
            // Permalink previews are made by the posts code from the post itself; nothing fetched here.
            return None;
        }
        let m = if let Some(provider) = oembed_provider(&url) {
            LinkMeta { opengraph: self.fetch_oembed(&url, provider).await, url: url.clone(), ..Default::default() }
        } else {
            let (og, image) = self.fetch_link(&url).await;
            self.save_link_metadata(&url, ts, og.as_ref(), image.as_ref()).await;
            LinkMeta { opengraph: og, image, url: url.clone(), ..Default::default() }
        };
        let m = Arc::new(m);
        self.caches.open_graph.put(key, m.clone());
        some(m)
    }

    async fn link_metadata_from_db(&self, url: &str, ts: i64) -> Option<LinkMeta> {
        let row: Option<(String, Option<Value>)> = sqlx::query_as(
            "SELECT COALESCE(Type, ''), Data FROM LinkMetadata WHERE Hash = $1 AND URL = $2 AND Timestamp = $3",
        )
        .bind(link_hash(url, ts))
        .bind(url)
        .bind(ts)
        .fetch_optional(&self.db)
        .await
        .ok()?;
        let (kind, data) = row?;
        let mut m = LinkMeta { url: url.into(), ..Default::default() };
        match (kind.as_str(), data) {
            ("opengraph", Some(d)) => {
                let og = serde_json::from_value::<OpenGraph>(d).unwrap_or_default();
                m.opengraph = Some(truncate(og));
            }
            ("image", Some(d)) => {
                let img = serde_json::from_value::<PostImage>(d).ok()?;
                if img.format != "svg" && !is_svg_url(url) {
                    m.image = Some(img);
                }
            }
            _ => {}
        }
        Some(m)
    }

    /// `saveLinkMetadataToDatabase`.
    async fn save_link_metadata(&self, url: &str, ts: i64, og: Option<&OpenGraph>, image: Option<&PostImage>) {
        if url.is_empty() || url.len() > 2048 {
            return;
        }
        let (kind, data) = match (og, image) {
            (Some(og), _) => ("opengraph", Some(serde_json::to_value(og).unwrap())),
            (None, Some(i)) => ("image", Some(serde_json::to_value(i).unwrap())),
            _ => ("none", None),
        };
        let r = sqlx::query(
            "INSERT INTO LinkMetadata (Hash, URL, Timestamp, Type, Data) VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (Hash) DO UPDATE SET URL = $2, Timestamp = $3, Type = $4, Data = $5",
        )
        .bind(link_hash(url, ts))
        .bind(url)
        .bind(ts)
        .bind(kind)
        .bind(data)
        .execute(&self.db)
        .await;
        if let Err(e) = r {
            eprintln!("failed to write link metadata for {url}: {e}");
        }
    }

    /// `getLinkMetadataForURL`: an image's size, or an HTML page's OpenGraph data.
    async fn fetch_link(&self, url: &str) -> (Option<OpenGraph>, Option<PostImage>) {
        let Ok(u) = Url::parse(url) else { return (None, None) };
        if !matches!(u.scheme(), "http" | "https") || !self.untrusted_url_allowed(&u) {
            return (None, None);
        }
        let timeout =
            Duration::from_millis(self.cfg.i64("ExperimentalSettings.LinkMetadataTimeoutMilliseconds").max(1) as u64);
        let res = self
            .link_client()
            .get(u)
            .header("Accept", "image/*, text/html;q=0.8")
            .header("Accept-Language", self.cfg.str("LocalizationSettings.DefaultServerLocale"))
            .timeout(timeout)
            .send()
            .await;
        let Ok(mut res) = res else { return (None, None) };
        let mut content_type =
            res.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
        let mut body: Vec<u8> = vec![];
        let mut sniffed = false;
        while let Ok(Some(chunk)) = res.chunk().await {
            body.extend_from_slice(&chunk);
            if !sniffed && body.len() >= 512 {
                sniffed = true;
                if content_type.is_empty() {
                    content_type = sniff(&body).into();
                }
            }
            if body.len() >= MAX_RESPONSE
                || content_type.starts_with("text/html") && contains_ci(&body, b"</head")
                || content_type.starts_with("image")
                    && !content_type.starts_with("image/gif")
                    && body.len() >= 256 * 1024
            {
                break;
            }
        }
        body.truncate(MAX_RESPONSE);
        if content_type.is_empty() {
            content_type = sniff(&body).into();
        }
        if content_type.starts_with("image/svg+xml") {
            (None, None)
        } else if content_type.starts_with("image") {
            (None, parse_image(&body))
        } else if content_type.starts_with("text/html") {
            let og = self.parse_opengraph(url, &body, &content_type);
            if og.title.is_empty() && og.r#type.is_empty() && og.url.is_empty() {
                (None, None)
            } else {
                (Some(truncate(og)), None)
            }
        } else {
            (None, None)
        }
    }

    /// `parseOpenGraphMetadata`.
    fn parse_opengraph(&self, url: &str, body: &[u8], content_type: &str) -> OpenGraph {
        let text = to_utf8(body, content_type);
        let mut og = OpenGraph::default();
        for attrs in meta_tags(&text) {
            process_meta(&mut og, &attrs);
        }
        // makeOpenGraphURLsAbsolute
        if let Ok(base) = Url::parse(url) {
            let abs = |s: &mut String| {
                if !s.is_empty()
                    && Url::parse(s).is_err()
                    && let Ok(j) = base.join(s)
                {
                    *s = j.to_string();
                }
            };
            abs(&mut og.url);
            for i in og.images.iter_mut().flatten() {
                abs(&mut i.url);
                abs(&mut i.secure_url);
            }
        }
        og.title = html_escape::decode_html_entities(&og.title).into_owned();
        og.description = html_escape::decode_html_entities(&og.description).into_owned();
        og.images = og.images.map(filter_svg_images);
        if self.cfg.bool("ImageProxySettings.Enable") {
            for i in og.images.iter_mut().flatten() {
                let u = if i.secure_url.is_empty() { std::mem::take(&mut i.url) } else { i.secure_url.clone() };
                i.url.clear();
                i.secure_url = self.proxy_image_url(&u);
            }
        }
        if !og.url.is_empty() {
            og.url = url.to_string();
        }
        og
    }

    /// The local image proxy's fetch: another site's picture (at most 50 MB) and its type.
    pub async fn fetch_image(&self, u: &Url) -> Result<(bytes::Bytes, String), String> {
        if !matches!(u.scheme(), "http" | "https") || !self.untrusted_url_allowed(u) {
            return Err("address forbidden".into());
        }
        let mut res =
            self.link_client().get(u.clone()).header("Accept", "image/*").send().await.map_err(|e| e.to_string())?;
        if !res.status().is_success() {
            return Err(format!("status {}", res.status()));
        }
        let mut body: Vec<u8> = vec![];
        while let Some(chunk) = res.chunk().await.map_err(|e| e.to_string())? {
            body.extend_from_slice(&chunk);
            if body.len() > MAX_RESPONSE {
                return Err("image too large".into());
            }
        }
        let mut ct = res.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
        if ct.is_empty() || ct.starts_with("application/octet-stream") {
            ct = sniff(&body).into();
        }
        if !ct.starts_with("image/") || ct.starts_with("image/svg") {
            return Err(format!("not an image: {ct}"));
        }
        Ok((body.into(), ct))
    }

    /// `getLinkMetadataFromOEmbed` + `parseOpenGraphFromOEmbed`.
    async fn fetch_oembed(&self, url: &str, provider: &str) -> Option<OpenGraph> {
        let req = format!("{provider}?format=json&url={}", query_escape(url));
        let timeout =
            Duration::from_millis(self.cfg.i64("ExperimentalSettings.LinkMetadataTimeoutMilliseconds").max(1) as u64);
        let res = self
            .link_client()
            .get(req)
            .header("Accept", "application/json")
            .header("Accept-Language", self.cfg.str("LocalizationSettings.DefaultServerLocale"))
            .timeout(timeout)
            .send()
            .await
            .ok()?;
        let body = res.bytes().await.ok()?;
        let r: Value = serde_json::from_slice(&body[..body.len().min(MAX_RESPONSE)]).ok()?;
        if r["version"] != "1.0" || !matches!(r["type"].as_str(), Some("photo" | "video" | "link" | "rich")) {
            return None;
        }
        let mut og = OpenGraph {
            r#type: "opengraph".into(),
            title: r["title"].as_str().unwrap_or("").into(),
            url: url.into(),
            ..Default::default()
        };
        if let Some(t) = r["thumbnail_url"].as_str().filter(|t| !t.is_empty()) {
            og.images = Some(vec![OgImage {
                r#type: "image".into(),
                url: t.into(),
                width: r["thumbnail_width"].as_u64().unwrap_or(0),
                height: r["thumbnail_height"].as_u64().unwrap_or(0),
                ..Default::default()
            }]);
        }
        og.images = og.images.map(filter_svg_images);
        if self.cfg.bool("ImageProxySettings.Enable") {
            for i in og.images.iter_mut().flatten() {
                let u = std::mem::take(&mut i.url);
                i.secure_url = self.proxy_image_url(&u);
            }
        }
        Some(og)
    }
}

fn some(m: Arc<LinkMeta>) -> Option<Arc<LinkMeta>> {
    (m.opengraph.is_some() || m.image.is_some()).then_some(m)
}

/// `normalizeDomains` + `isLinkAllowedForPreview`.
fn link_allowed(restrict: &str, link: &str) -> bool {
    let domains: Vec<String> =
        restrict.replace(['@', ','], " ").to_lowercase().split_whitespace().map(str::to_string).collect();
    if domains.is_empty() {
        return true;
    }
    let Ok(u) = Url::parse(link) else { return false };
    let host = u.host_str().unwrap_or("").to_string();
    !domains.iter().any(|d| host.contains(d.as_str()))
}

/// `resolveMetadataURL`: relative to the site URL.
fn resolve_url(u: &str, site: &str) -> Option<String> {
    if let Ok(abs) = Url::parse(u) {
        return Some(abs.to_string());
    }
    Some(Url::parse(site).ok()?.join(u).ok()?.to_string())
}

fn looks_like_permalink(u: &str, site: &str) -> bool {
    let Some(path) = u.trim().strip_prefix(site) else { return false };
    let path = path.trim_start_matches('/');
    let Some((team, id)) = path.split_once("/pl/") else { return false };
    (1..=64).contains(&team.len())
        && team.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
        && id.len() == 26
        && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

/// The oEmbed providers Mattermost knows: YouTube.
fn oembed_provider(u: &str) -> Option<&'static str> {
    static YT: OnceLock<Vec<regex::Regex>> = OnceLock::new();
    let pats = YT.get_or_init(|| {
        [
            r"^https://[^/]*?\.youtube\.com/watch.*?$",
            r"^https://[^/]*?\.youtube\.com/v/.*?$",
            r"^https://youtu\.be/.*?$",
            r"^https://[^/]*?\.youtube\.com/playlist\?list=.*?$",
            r"^https://youtube\.com/playlist\?list=.*?$",
            r"^https://[^/]*?\.youtube\.com/shorts.*?$",
            r"^https://youtube\.com/shorts.*?$",
            r"^https://[^/]*?\.youtube\.com/embed/.*?$",
            r"^https://[^/]*?\.youtube\.com/live.*?$",
            r"^https://youtube\.com/live.*?$",
        ]
        .iter()
        .map(|p| regex::Regex::new(p).unwrap())
        .collect()
    });
    pats.iter().any(|p| p.is_match(u)).then_some("https://www.youtube.com/oembed")
}

/// `model.FloorToNearestHour`.
pub fn floor_to_hour(ms: i64) -> i64 {
    ms.div_euclid(3_600_000) * 3_600_000
}

/// `model.GenerateLinkMetadataHash`: FNV-1 32 of the timestamp (little endian) and the URL.
pub fn link_hash(url: &str, ts: i64) -> i64 {
    let mut h: u32 = 2166136261;
    for b in ts.to_le_bytes().iter().chain(url.as_bytes()) {
        h = h.wrapping_mul(16777619);
        h ^= *b as u32;
    }
    h as i64
}

/// `TruncateOpenGraph`: 300 characters of text, five images without SVGs, nothing else.
fn truncate(mut og: OpenGraph) -> OpenGraph {
    let cut = |s: &str| {
        if s.chars().count() > 300 {
            format!("{}[...]", s.chars().take(300).collect::<String>())
        } else {
            s.to_string()
        }
    };
    og.title = cut(&og.title);
    og.description = cut(&og.description);
    og.site_name = cut(&og.site_name);
    og.determiner.clear();
    og.locale.clear();
    og.locales_alternate = None;
    og.images = og.images.map(|mut i| {
        i.truncate(MAX_IMAGES);
        filter_svg_images(i)
    });
    og.audios = None;
    og.videos = None;
    og
}

/// `model.IsSVGImageURL`.
fn is_svg_url(u: &str) -> bool {
    let Ok(p) = Url::parse(u) else { return false };
    let path = p.path().to_lowercase();
    path.ends_with(".svg") || path.ends_with(".svgz")
}

fn filter_svg_images(images: Vec<OgImage>) -> Vec<OgImage> {
    images
        .into_iter()
        .filter(|i| !is_svg_url(&i.url) && !is_svg_url(&i.secure_url) && !i.r#type.starts_with("image/svg+xml"))
        .collect()
}

/// `parseImages`: format, size (upright), GIF frames; nothing for TIFF.
fn parse_image(body: &[u8]) -> Option<PostImage> {
    let h = images::header(body)?;
    let format = images::format_name(h.format);
    let (mut w, mut hh) = (h.width as i64, h.height as i64);
    if format == "jpeg" && h.quarter_turn {
        std::mem::swap(&mut w, &mut hh);
    }
    let frame_count = if format == "gif" { images::gif_frames(body)? as i64 } else { 0 };
    if format == "tiff" {
        return None;
    }
    Some(PostImage { width: w, height: hh, format: format.into(), frame_count })
}

/// `http.DetectContentType`, for what link previews tell apart.
fn sniff(b: &[u8]) -> &'static str {
    let b = &b[..b.len().min(512)];
    let t = b.iter().position(|c| !c.is_ascii_whitespace()).map_or(&b[..0], |i| &b[i..]);
    let lower: Vec<u8> = t.iter().take(16).map(u8::to_ascii_lowercase).collect();
    if [b"<!doctype html".as_slice(), b"<html", b"<head", b"<body", b"<title", b"<meta", b"<script"]
        .iter()
        .any(|p| lower.starts_with(p))
    {
        return "text/html; charset=utf-8";
    }
    if b.starts_with(b"\xFF\xD8\xFF") {
        "image/jpeg"
    } else if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        "image/gif"
    } else if b.starts_with(b"BM") {
        "image/bmp"
    } else if b.len() >= 12 && &b[..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        "image/webp"
    } else {
        "application/octet-stream"
    }
}

fn contains_ci(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w.eq_ignore_ascii_case(needle))
}

/// `charset.NewReader`: the Content-Type's charset, a BOM, a `<meta>` charset in the first 1024 bytes, else
/// UTF-8 when valid and windows-1252 otherwise.
fn to_utf8(body: &[u8], content_type: &str) -> String {
    let label = content_type
        .split(';')
        .find_map(|p| p.trim().strip_prefix("charset=").map(|v| v.trim_matches('"').to_string()))
        .or_else(|| meta_charset(&body[..body.len().min(1024)]));
    let enc = label
        .and_then(|l| encoding_rs::Encoding::for_label(l.as_bytes()))
        .or_else(|| encoding_rs::Encoding::for_bom(body).map(|(e, _)| e))
        .unwrap_or(if std::str::from_utf8(body).is_ok() { encoding_rs::UTF_8 } else { encoding_rs::WINDOWS_1252 });
    enc.decode(body).0.into_owned()
}

fn meta_charset(head: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(head).to_lowercase();
    let i = text.find("charset=")?;
    let rest = text[i + 8..].trim_start_matches(['"', '\'']);
    let end = rest.find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_')).unwrap_or(rest.len());
    (end > 0).then(|| rest[..end].to_string())
}

/// The attributes (lower-case names, unescaped values) of every `<meta>` tag.
fn meta_tags(html: &str) -> Vec<Vec<(String, String)>> {
    let lower = html.to_ascii_lowercase();
    let mut out = vec![];
    let mut at = 0;
    while let Some(p) = lower[at..].find("<meta") {
        let start = at + p + 5;
        at = start;
        if !lower[start..].starts_with(|c: char| c.is_whitespace() || c == '/' || c == '>') {
            continue;
        }
        let Some(end) = tag_end(&html[start..]) else { break };
        out.push(parse_attrs(&html[start..start + end]));
        at = start + end;
    }
    out
}

/// Where a tag ends: the first `>` outside quotes.
fn tag_end(s: &str) -> Option<usize> {
    let mut quote = None;
    for (i, c) in s.char_indices() {
        match (quote, c) {
            (None, '"' | '\'') => quote = Some(c),
            (Some(q), c) if c == q => quote = None,
            (None, '>') => return Some(i),
            _ => {}
        }
    }
    None
}

fn parse_attrs(s: &str) -> Vec<(String, String)> {
    let mut out = vec![];
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        while i < b.len() && (b[i].is_ascii_whitespace() || b[i] == b'/') {
            i += 1;
        }
        let ks = i;
        while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'=' && b[i] != b'/' {
            i += 1;
        }
        let key = s[ks..i].to_ascii_lowercase();
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        let mut val = String::new();
        if i < b.len() && b[i] == b'=' {
            i += 1;
            while i < b.len() && b[i].is_ascii_whitespace() {
                i += 1;
            }
            if i < b.len() && (b[i] == b'"' || b[i] == b'\'') {
                let q = b[i];
                let vs = i + 1;
                i = vs;
                while i < b.len() && b[i] != q {
                    i += 1;
                }
                val = s[vs..i.min(b.len())].to_string();
                i += 1;
            } else {
                let vs = i;
                while i < b.len() && !b[i].is_ascii_whitespace() {
                    i += 1;
                }
                val = s[vs..i].to_string();
            }
        }
        if !key.is_empty() {
            out.push((key, html_escape::decode_html_entities(&val).into_owned()));
        }
    }
    out
}

/// `OpenGraph.ProcessMeta` for what survives `TruncateOpenGraph`.
fn process_meta(og: &mut OpenGraph, attrs: &[(String, String)]) {
    let get = |k: &str| attrs.iter().rev().find(|(n, _)| n == k).map(|(_, v)| v.clone()).unwrap_or_default();
    let content = get("content");
    fn images(og: &mut OpenGraph) -> &mut Vec<OgImage> {
        let v = og.images.get_or_insert_with(Vec::new);
        if v.is_empty() {
            v.push(OgImage::default());
        }
        v
    }
    match get("property").as_str() {
        "og:description" => og.description = content,
        "og:type" => og.r#type = content,
        "og:title" => og.title = content,
        "og:url" => og.url = content,
        "og:determiner" => og.determiner = content,
        "og:site_name" => og.site_name = content,
        "og:locale" => og.locale = content,
        "og:locale:alternate" => og.locales_alternate.get_or_insert_with(Vec::new).push(content),
        "og:image" | "og:image:url" => {
            let v = og.images.get_or_insert_with(Vec::new);
            if v.last().is_none_or(|l| !l.url.is_empty() && l.url != content) {
                v.push(OgImage::default());
            }
            v.last_mut().unwrap().url = content;
        }
        "og:image:secure_url" => images(og).last_mut().unwrap().secure_url = content,
        "og:image:type" => images(og).last_mut().unwrap().r#type = content,
        "og:image:width" => {
            if let Ok(w) = content.parse() {
                images(og).last_mut().unwrap().width = w;
            }
        }
        "og:image:height" => {
            if let Ok(h) = content.parse() {
                images(og).last_mut().unwrap().height = h;
            }
        }
        _ => {}
    }
}

/// `url.QueryEscape`.
pub fn query_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allowed(list: &[&str]) -> Allowed {
        Allowed(list.iter().map(|s| s.to_string()).collect())
    }

    /// `checkInternalIP`, with the interface list given.
    #[test]
    fn internal_ips() {
        let own = |ip: IpAddr| Ok(ip == "203.0.114.7".parse::<IpAddr>().unwrap());
        let none = allowed(&[]);
        assert_eq!(none.check_ip_with("8.8.8.8".parse().unwrap(), own), Ok(()));
        assert_eq!(
            none.check_ip_with("127.0.0.1".parse().unwrap(), own),
            Err("IP 127.0.0.1 is in a reserved range and not in AllowedUntrustedInternalConnections".into())
        );
        // IPv4-mapped IPv6 is the IPv4 address.
        assert_eq!(
            none.check_ip_with("::ffff:10.0.0.1".parse().unwrap(), own),
            Err("IP 10.0.0.1 is in a reserved range and not in AllowedUntrustedInternalConnections".into())
        );
        // A public address of this host's own (IsOwnIP).
        assert_eq!(
            none.check_ip_with("203.0.114.7".parse().unwrap(), own),
            Err("IP 203.0.114.7 is a self-assigned IP and not in AllowedUntrustedInternalConnections".into())
        );
        assert_eq!(
            none.check_ip_with("::ffff:203.0.114.7".parse().unwrap(), own),
            Err("IP 203.0.114.7 is a self-assigned IP and not in AllowedUntrustedInternalConnections".into())
        );
        let some = allowed(&["10.0.0.0/8", "203.0.114.0/24", "intranet"]);
        assert_eq!(some.check_ip_with("10.1.2.3".parse().unwrap(), own), Ok(()));
        assert_eq!(some.check_ip_with("203.0.114.7".parse().unwrap(), own), Ok(()));
        assert!(some.host("intranet") && !some.host("10.1.2.3"));
        // No interface list: nothing is allowed, not even a public address.
        let broken = |_: IpAddr| Err(std::io::Error::other("no interfaces"));
        assert_eq!(
            some.check_ip_with("8.8.8.8".parse().unwrap(), broken),
            Err("unable to determine if IP is own IP: no interfaces".into())
        );
        // The loopback interface is this host's.
        assert!(is_own_ip("127.0.0.1".parse().unwrap()).unwrap());
        assert!(is_own_ip("::ffff:127.0.0.1".parse().unwrap()).unwrap());
        assert!(!is_own_ip("192.0.2.200".parse().unwrap()).unwrap());
    }

    #[test]
    fn connector_request_host() {
        #[derive(Debug)]
        struct Unnameable(#[allow(dead_code)] http::Uri);
        let host = |u: &str| connect_host(&Unnameable(u.parse().unwrap()));
        assert_eq!(host("http://127.0.0.1:8080/"), Some("127.0.0.1".into()));
        assert_eq!(host("https://[::1]:443"), Some("::1".into()));
        assert_eq!(host("http://example.com"), Some("example.com".into()));
        assert_eq!(connect_host(&"something else"), None);
    }

    /// The guard refuses an IP-literal host before dialling, and a name that resolved to an internal address
    /// after connecting, whatever resolver the client has; an allowed host or CIDR goes through.
    #[tokio::test]
    async fn guard_refuses_internal_addresses() {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        let accepted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let n = accepted.clone();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            while let Ok((mut s, _)) = l.accept().await {
                n.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                tokio::spawn(async move {
                    // Read the request first: closing with it unread would reset the connection.
                    let mut buf = vec![0u8; 4096];
                    let mut got = vec![];
                    while !got.windows(4).any(|w| w == b"\r\n\r\n") {
                        match s.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(k) => got.extend_from_slice(&buf[..k]),
                        }
                    }
                    let _ = s
                        .write_all(
                            b"HTTP/1.1 302 Found\r\nLocation: /x\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .await;
                    let _ = s.shutdown().await;
                });
            }
        });
        let client = |a: Allowed| {
            reqwest::Client::builder()
                .connector_layer(ConnectGuard(a))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap()
        };
        let err = client(allowed(&[])).head(format!("http://127.0.0.1:{port}/")).send().await.unwrap_err();
        let chain = format!("{:?}", err);
        assert!(chain.contains("IP 127.0.0.1 is in a reserved range"), "{chain}");
        assert_eq!(accepted.load(std::sync::atomic::Ordering::SeqCst), 0, "dialled a refused literal");
        let err = client(allowed(&[])).head(format!("http://localhost:{port}/")).send().await.unwrap_err();
        assert!(format!("{err:?}").contains("address forbidden"), "{err:?}");
        let ok = client(allowed(&["127.0.0.0/8"])).head(format!("http://127.0.0.1:{port}/")).send().await.unwrap();
        assert_eq!(ok.status(), 302);
        let ok = client(allowed(&["localhost"])).head(format!("http://localhost:{port}/")).send().await.unwrap();
        assert_eq!(ok.status(), 302);
    }

    #[test]
    fn hash_matches_go() {
        // fnv.New32 over binary.LittleEndian int64 0 then "a": 8 zero bytes then 'a'.
        let mut h: u32 = 2166136261;
        for b in [0u8; 8].iter().chain(b"a") {
            h = h.wrapping_mul(16777619) ^ *b as u32;
        }
        assert_eq!(link_hash("a", 0), h as i64);
    }

    #[test]
    fn reserved_ips() {
        assert!(is_reserved("10.1.2.3".parse().unwrap()));
        assert!(is_reserved("127.0.0.1".parse().unwrap()));
        assert!(is_reserved("::1".parse().unwrap()));
        assert!(is_reserved("::ffff:192.168.1.1".parse().unwrap()));
        assert!(!is_reserved("8.8.8.8".parse().unwrap()));
        assert!(!is_reserved("2a00:1450::1".parse().unwrap()));
    }

    #[test]
    fn opengraph_from_html() {
        let html = r#"<html><head><meta property="og:title" content="A &amp;amp; B">
            <meta property='og:type' content=website><meta property="og:image" content="/i.png">
            <meta property="og:image:width" content="10"><meta property="og:image" content="/x.svg">
            <meta property="og:url" content="https://x.test/canonical"></head>"#;
        let mut og = OpenGraph::default();
        for a in meta_tags(html) {
            process_meta(&mut og, &a);
        }
        assert_eq!(og.title, "A &amp; B");
        assert_eq!(og.r#type, "website");
        let imgs = og.images.clone().unwrap();
        assert_eq!(imgs.len(), 2);
        assert_eq!((imgs[0].url.as_str(), imgs[0].width), ("/i.png", 10));
        assert_eq!(filter_svg_images(vec![OgImage { url: "https://a/x.svg".into(), ..Default::default() }]).len(), 0);
    }

    #[test]
    fn permalinks_and_oembed() {
        assert!(looks_like_permalink("https://mm.test/team-a/pl/abcdefghijklmnopqrstuvwxyz", "https://mm.test"));
        assert!(!looks_like_permalink("https://mm.test/team-a/channels/x", "https://mm.test"));
        assert!(oembed_provider("https://www.youtube.com/watch?v=1").is_some());
        assert!(oembed_provider("https://example.com/").is_none());
        assert_eq!(query_escape("a b&c/д"), "a+b%26c%2F%D0%B4");
    }
}

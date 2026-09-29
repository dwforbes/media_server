//! GENA eventing (UPnP Device Architecture 1.1, chapter 4): the push half
//! of the ContentDirectory. A client SUBSCRIBEs at a service's event URL
//! naming a callback URL of its own; it gets a subscription id (SID) and
//! a lease, and from then on every change to an evented state variable
//! reaches it as an HTTP NOTIFY to that callback, so a listing it holds
//! can be refreshed the moment the scanner commits rather than when the
//! viewer next happens to browse. Clients renew by re-SUBSCRIBEing with
//! the SID, or let the lease lapse.
//!
//! ContentDirectory events `SystemUpdateID`, the counter the catalog
//! watcher bumps (see main.rs), and nothing else: `ContainerUpdateIDs`
//! is optional in CDS:1 and the SCPD does not declare it. Every Browse
//! reply also carries the current id, which is what clients compare.
//! ConnectionManager's three evented variables never change after the
//! initial event, so its subscriptions only ever see that one.
//!
//! Callbacks are delivered with a hand-rolled NOTIFY over a plain TCP
//! connection (one request, one status line, done) rather than an HTTP
//! client dependency, and only to private, link-local or loopback
//! addresses: a LAN client subscribing is the whole use, and it keeps a
//! stray SUBSCRIBE from steering NOTIFYs at the wider internet.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderValue, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::didl::xml_escape;
use crate::http::AppState;

/// The longest lease granted; clients asking for more (or "infinite")
/// get this and renew. Long enough that a set-top box renews rarely,
/// short enough that one switched off is forgotten within the hour.
const MAX_TIMEOUT: Duration = Duration::from_secs(1800);
const MIN_TIMEOUT: Duration = Duration::from_secs(30);
/// Subscriptions kept at once, over both services; a LAN has a handful
/// of clients, and each holds one per service.
const MAX_SUBSCRIPTIONS: usize = 64;
/// Patience per callback attempt: connect, send, and the status line.
/// The spec allows 30 s; a client that slow is holding up the others.
const DELIVERY_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Service {
    ContentDirectory,
    ConnectionManager,
}

/// One URL from a CALLBACK header, taken apart for the NOTIFY.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Callback {
    addr: SocketAddr,
    /// The authority as the client wrote it, for the HOST header.
    host: String,
    path: String,
}

struct Subscription {
    service: Service,
    callbacks: Vec<Callback>,
    expires: Instant,
    /// Event key of the next NOTIFY: 0 for the initial event, then
    /// counting up, and around to 1 rather than 0 at the end.
    seq: u32,
}

/// The subscription table plus the wake-up the catalog watcher rings.
#[derive(Default)]
pub struct Publisher {
    subs: Mutex<HashMap<String, Subscription>>,
    changed: tokio::sync::Notify,
}

/// What a SUBSCRIBE/UNSUBSCRIBE could not accept, with the status the
/// spec assigns it.
#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    /// SID together with NT or CALLBACK, or UNSUBSCRIBE carrying those.
    Incompatible,
    /// Missing or malformed NT, CALLBACK or SID, or a SID nobody holds.
    Precondition,
    /// The table is full.
    Full,
}

impl Refusal {
    fn status(&self) -> StatusCode {
        match self {
            Refusal::Incompatible => StatusCode::BAD_REQUEST,
            Refusal::Precondition => StatusCode::PRECONDITION_FAILED,
            Refusal::Full => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

/// The lease a client asked for, from `TIMEOUT: Second-1800` (or
/// `Second-infinite`); absent or unreadable means the maximum.
pub fn requested_timeout(header: Option<&str>) -> Duration {
    let Some(value) = header.map(str::trim) else { return MAX_TIMEOUT };
    let Some(rest) = value.strip_prefix("Second-").or_else(|| value.strip_prefix("second-")) else {
        return MAX_TIMEOUT;
    };
    match rest.trim().parse::<u64>() {
        Ok(secs) => Duration::from_secs(secs).clamp(MIN_TIMEOUT, MAX_TIMEOUT),
        Err(_) => MAX_TIMEOUT, // "infinite", or nonsense
    }
}

/// The CALLBACK header: one or more `<http://host:port/path>` in order
/// of preference. Only http, only IP-literal hosts, only private ones.
pub fn parse_callbacks(header: &str) -> Vec<Callback> {
    let mut out = Vec::new();
    for part in header.split('<').skip(1) {
        let Some(url) = part.split('>').next() else { continue };
        if let Some(cb) = parse_callback(url.trim()) {
            out.push(cb);
        }
    }
    out
}

fn parse_callback(url: &str) -> Option<Callback> {
    let rest = url.strip_prefix("http://")?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    if authority.is_empty() || authority.contains('@') {
        return None;
    }
    let (host, port) = if let Some(bracketed) = authority.strip_prefix('[') {
        let (ip, after) = bracketed.split_once(']')?;
        let port = match after.strip_prefix(':') {
            Some(p) => p.parse::<u16>().ok()?,
            None if after.is_empty() => 80,
            None => return None,
        };
        (ip.parse::<IpAddr>().ok()?, port)
    } else {
        match authority.rsplit_once(':') {
            Some((ip, p)) => (ip.parse::<IpAddr>().ok()?, p.parse::<u16>().ok()?),
            None => (authority.parse::<IpAddr>().ok()?, 80),
        }
    };
    if !is_local(host) {
        return None;
    }
    Some(Callback { addr: SocketAddr::new(host, port), host: authority.to_string(), path: path.to_string() })
}

/// Addresses a LAN client can have: private, link-local, loopback,
/// unique-local. Nothing routable.
fn is_local(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_private() || v4.is_loopback() || v4.is_link_local(),
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                || v6.to_ipv4_mapped().is_some_and(|v4| is_local(IpAddr::V4(v4)))
        }
    }
}

/// What a SUBSCRIBE was granted: the SID to answer with, the lease, and
/// whether an initial event is owed (a new subscription, not a renewal).
#[derive(Debug)]
pub struct Granted {
    pub sid: String,
    pub timeout: Duration,
    pub initial: bool,
}

impl Publisher {
    /// The catalog watcher's ring: something changed, tell subscribers.
    /// Safe from any thread; rounds are coalesced.
    pub fn changed(&self) {
        self.changed.notify_one();
    }

    fn purge(subs: &mut HashMap<String, Subscription>, now: Instant) {
        subs.retain(|_, s| s.expires > now);
    }

    /// A new subscription (NT + CALLBACK, no SID) or a renewal (SID, no
    /// NT/CALLBACK), per the spec's header rules.
    pub fn subscribe(
        &self,
        service: Service,
        sid: Option<&str>,
        nt: Option<&str>,
        callback: Option<&str>,
        timeout: Duration,
        now: Instant,
    ) -> Result<Granted, Refusal> {
        let mut subs = self.subs.lock().unwrap_or_else(|e| e.into_inner());
        Self::purge(&mut subs, now);
        match sid {
            Some(sid) => {
                if nt.is_some() || callback.is_some() {
                    return Err(Refusal::Incompatible);
                }
                let sub = subs.get_mut(sid).filter(|s| s.service == service).ok_or(Refusal::Precondition)?;
                sub.expires = now + timeout;
                Ok(Granted { sid: sid.to_string(), timeout, initial: false })
            }
            None => {
                if nt.map(str::trim) != Some("upnp:event") {
                    return Err(Refusal::Precondition);
                }
                let callbacks = parse_callbacks(callback.ok_or(Refusal::Precondition)?);
                if callbacks.is_empty() {
                    return Err(Refusal::Precondition);
                }
                if subs.len() >= MAX_SUBSCRIPTIONS {
                    return Err(Refusal::Full);
                }
                let sid = format!("uuid:{}", uuid::Uuid::new_v4());
                subs.insert(sid.clone(), Subscription { service, callbacks, expires: now + timeout, seq: 0 });
                Ok(Granted { sid, timeout, initial: true })
            }
        }
    }

    pub fn unsubscribe(&self, service: Service, sid: Option<&str>, nt: Option<&str>, callback: Option<&str>) -> Result<(), Refusal> {
        if nt.is_some() || callback.is_some() {
            return Err(Refusal::Incompatible);
        }
        let sid = sid.ok_or(Refusal::Precondition)?;
        let mut subs = self.subs.lock().unwrap_or_else(|e| e.into_inner());
        match subs.get(sid) {
            Some(s) if s.service == service => {
                subs.remove(sid);
                Ok(())
            }
            _ => Err(Refusal::Precondition),
        }
    }

    /// Live subscriptions to a service, each with the event key its next
    /// NOTIFY carries (taken here, so two rounds never share one).
    fn take_round(&self, service: Service, only: Option<&str>, now: Instant) -> Vec<(String, u32, Vec<Callback>)> {
        let mut subs = self.subs.lock().unwrap_or_else(|e| e.into_inner());
        Self::purge(&mut subs, now);
        subs.iter_mut()
            .filter(|(sid, s)| s.service == service && only.is_none_or(|o| o == sid.as_str()))
            .map(|(sid, s)| {
                let seq = s.seq;
                s.seq = if s.seq == u32::MAX { 1 } else { s.seq + 1 };
                (sid.clone(), seq, s.callbacks.clone())
            })
            .collect()
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.subs.lock().unwrap().len()
    }
}

/// The event body: every listed variable, as the spec's property set.
pub fn property_set(vars: &[(&str, &str)]) -> String {
    let mut xml = String::from("<?xml version=\"1.0\"?>\n<e:propertyset xmlns:e=\"urn:schemas-upnp-org:event-1-0\">");
    for (name, value) in vars {
        xml.push_str(&format!("<e:property><{name}>{}</{name}></e:property>", xml_escape(value)));
    }
    xml.push_str("</e:propertyset>");
    xml
}

fn cds_properties(state: &AppState) -> String {
    let id = state.update_id.load(std::sync::atomic::Ordering::Relaxed).to_string();
    property_set(&[("SystemUpdateID", id.as_str())])
}

fn cms_properties() -> String {
    let source = crate::http::source_protocol_info();
    property_set(&[("SourceProtocolInfo", source.as_str()), ("SinkProtocolInfo", ""), ("CurrentConnectionIDs", "0")])
}

/// One NOTIFY to one subscriber: the callbacks in order until one takes
/// it. Failure is logged and otherwise forgiven — the lease decides when
/// a subscriber is gone, as the spec has it.
async fn deliver(sid: &str, seq: u32, callbacks: &[Callback], body: &str) {
    for cb in callbacks {
        match tokio::time::timeout(DELIVERY_TIMEOUT, notify_once(cb, sid, seq, body)).await {
            Ok(Ok(status)) if (200..300).contains(&status) => {
                tracing::debug!("event {seq} to {} ({sid}) accepted", cb.addr);
                return;
            }
            Ok(Ok(status)) => tracing::debug!("event {seq} to {} ({sid}) answered {status}", cb.addr),
            Ok(Err(err)) => tracing::debug!("event {seq} to {} ({sid}) failed: {err}", cb.addr),
            Err(_) => tracing::debug!("event {seq} to {} ({sid}) timed out", cb.addr),
        }
    }
    tracing::info!("event {seq} undelivered to {sid} ({} callback(s))", callbacks.len());
}

async fn notify_once(cb: &Callback, sid: &str, seq: u32, body: &str) -> std::io::Result<u16> {
    let mut stream = tokio::net::TcpStream::connect(cb.addr).await?;
    let request = format!(
        "NOTIFY {path} HTTP/1.1\r\nHOST: {host}\r\nCONTENT-TYPE: text/xml; charset=\"utf-8\"\r\n\
         CONTENT-LENGTH: {len}\r\nNT: upnp:event\r\nNTS: upnp:propchange\r\nSID: {sid}\r\nSEQ: {seq}\r\n\
         CONNECTION: close\r\n\r\n{body}",
        path = cb.path,
        host = cb.host,
        len = body.len(),
    );
    stream.write_all(request.as_bytes()).await?;
    let mut buf = Vec::with_capacity(256);
    let mut chunk = [0u8; 256];
    loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(2).any(|w| w == b"\r\n") || buf.len() > 4096 {
            break;
        }
    }
    let line = String::from_utf8_lossy(&buf);
    line.split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| std::io::Error::other(format!("no status line in {:?}", line.lines().next().unwrap_or(""))))
}

/// The task that turns catalog changes into NOTIFYs: waits for the
/// watcher's ring, then sends the current SystemUpdateID to every
/// ContentDirectory subscriber at once.
pub async fn publish_loop(state: Arc<AppState>) {
    loop {
        state.events.changed.notified().await;
        // Let a burst of commits settle into one round.
        tokio::time::sleep(Duration::from_millis(500)).await;
        let body = cds_properties(&state);
        let round = state.events.take_round(Service::ContentDirectory, None, Instant::now());
        if round.is_empty() {
            continue;
        }
        tracing::debug!("notifying {} subscriber(s) of SystemUpdateID change", round.len());
        let mut sends = tokio::task::JoinSet::new();
        for (sid, seq, cbs) in round {
            let body = body.clone();
            sends.spawn(async move { deliver(&sid, seq, &cbs, &body).await });
        }
        while sends.join_next().await.is_some() {}
    }
}

/// The initial event owed to a new subscriber, sent after the SUBSCRIBE
/// response has gone out (the spec's order; the small delay is for that).
fn send_initial(state: Arc<AppState>, service: Service, sid: String) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let body = match service {
            Service::ContentDirectory => cds_properties(&state),
            Service::ConnectionManager => cms_properties(),
        };
        for (sid, seq, cbs) in state.events.take_round(service, Some(&sid), Instant::now()) {
            deliver(&sid, seq, &cbs, &body).await;
        }
    });
}

pub async fn cds(State(state): State<Arc<AppState>>, req: Request<Body>) -> Response {
    handle(state, Service::ContentDirectory, req)
}

pub async fn cms(State(state): State<Arc<AppState>>, req: Request<Body>) -> Response {
    handle(state, Service::ConnectionManager, req)
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

fn handle(state: Arc<AppState>, service: Service, req: Request<Body>) -> Response {
    let headers = req.headers();
    let sid = header(headers, "sid");
    let nt = header(headers, "nt");
    let callback = header(headers, "callback");
    match req.method().as_str() {
        "SUBSCRIBE" => {
            let timeout = requested_timeout(header(headers, "timeout"));
            match state.events.subscribe(service, sid, nt, callback, timeout, Instant::now()) {
                Ok(granted) => {
                    if granted.initial {
                        tracing::info!("{service:?} subscriber {} via {}", granted.sid, callback.unwrap_or(""));
                        send_initial(state.clone(), service, granted.sid.clone());
                    }
                    let mut res = StatusCode::OK.into_response();
                    let h = res.headers_mut();
                    if let Ok(v) = HeaderValue::from_str(&granted.sid) {
                        h.insert("sid", v);
                    }
                    if let Ok(v) = HeaderValue::from_str(&format!("Second-{}", granted.timeout.as_secs())) {
                        h.insert("timeout", v);
                    }
                    stamp(h);
                    res
                }
                Err(refusal) => {
                    tracing::debug!("{service:?} SUBSCRIBE refused: {refusal:?}");
                    refusal.status().into_response()
                }
            }
        }
        "UNSUBSCRIBE" => match state.events.unsubscribe(service, sid, nt, callback) {
            Ok(()) => {
                tracing::info!("{service:?} subscriber {} left", sid.unwrap_or(""));
                let mut res = StatusCode::OK.into_response();
                stamp(res.headers_mut());
                res
            }
            Err(refusal) => refusal.status().into_response(),
        },
        _ => StatusCode::METHOD_NOT_ALLOWED.into_response(),
    }
}

fn stamp(h: &mut HeaderMap) {
    if let Ok(v) = HeaderValue::from_str(crate::ssdp::SERVER_ID) {
        h.insert(header::SERVER, v);
    }
    if let Ok(v) = HeaderValue::from_str(&httpdate::fmt_http_date(std::time::SystemTime::now())) {
        h.insert(header::DATE, v);
    }
    h.insert(header::CONTENT_LENGTH, HeaderValue::from_static("0"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callbacks_parse_in_order_and_only_local_http() {
        let cbs = parse_callbacks("<http://192.168.1.20:49152/notify><http://[fe80::1]:8080/><http://8.8.8.8/x><https://192.168.1.20/>");
        assert_eq!(cbs.len(), 2);
        assert_eq!(cbs[0].addr, "192.168.1.20:49152".parse().unwrap());
        assert_eq!(cbs[0].path, "/notify");
        assert_eq!(cbs[0].host, "192.168.1.20:49152");
        assert_eq!(cbs[1].addr, "[fe80::1]:8080".parse().unwrap());
        assert_eq!(cbs[1].path, "/");
        assert_eq!(parse_callback("http://10.0.0.5"), Some(Callback { addr: "10.0.0.5:80".parse().unwrap(), host: "10.0.0.5".into(), path: "/".into() }));
        assert!(parse_callback("http://example.com/").is_none(), "hostnames are not callbacks");
        assert!(parse_callback("http://user@10.0.0.5/").is_none());
        assert!(parse_callbacks("garbage").is_empty());
    }

    #[test]
    fn timeouts_are_clamped() {
        assert_eq!(requested_timeout(Some("Second-300")), Duration::from_secs(300));
        assert_eq!(requested_timeout(Some("Second-infinite")), MAX_TIMEOUT);
        assert_eq!(requested_timeout(Some("Second-99999")), MAX_TIMEOUT);
        assert_eq!(requested_timeout(Some("Second-1")), MIN_TIMEOUT);
        assert_eq!(requested_timeout(None), MAX_TIMEOUT);
    }

    #[test]
    fn subscribe_renew_expire_unsubscribe() {
        let p = Publisher::default();
        let t0 = Instant::now();
        let cb = Some("<http://192.168.1.9:1234/ev>");
        assert_eq!(p.subscribe(Service::ContentDirectory, None, None, cb, MAX_TIMEOUT, t0).unwrap_err(), Refusal::Precondition, "NT required");
        assert_eq!(p.subscribe(Service::ContentDirectory, None, Some("upnp:event"), None, MAX_TIMEOUT, t0).unwrap_err(), Refusal::Precondition, "callback required");
        assert_eq!(p.subscribe(Service::ContentDirectory, None, Some("upnp:event"), Some("<http://8.8.8.8/>"), MAX_TIMEOUT, t0).unwrap_err(), Refusal::Precondition, "public callback refused");

        let g = p.subscribe(Service::ContentDirectory, None, Some("upnp:event"), cb, Duration::from_secs(60), t0).unwrap();
        assert!(g.initial && g.sid.starts_with("uuid:"));
        assert_eq!(p.len(), 1);

        // The initial event is key 0; the next round is 1.
        let round = p.take_round(Service::ContentDirectory, Some(&g.sid), t0);
        assert_eq!(round[0].1, 0);
        let round = p.take_round(Service::ContentDirectory, None, t0);
        assert_eq!(round[0].1, 1);
        assert!(p.take_round(Service::ConnectionManager, None, t0).is_empty(), "other service's rounds leave it alone");

        assert_eq!(p.subscribe(Service::ContentDirectory, Some(&g.sid), Some("upnp:event"), None, MAX_TIMEOUT, t0).unwrap_err(), Refusal::Incompatible);
        assert_eq!(p.subscribe(Service::ConnectionManager, Some(&g.sid), None, None, MAX_TIMEOUT, t0).unwrap_err(), Refusal::Precondition, "a SID belongs to its service");
        let renewed = p.subscribe(Service::ContentDirectory, Some(&g.sid), None, None, Duration::from_secs(120), t0 + Duration::from_secs(50)).unwrap();
        assert!(!renewed.initial && renewed.sid == g.sid);

        // Past the renewed lease it is gone; before, it is not.
        assert_eq!(p.take_round(Service::ContentDirectory, None, t0 + Duration::from_secs(160)).len(), 1);
        assert!(p.take_round(Service::ContentDirectory, None, t0 + Duration::from_secs(171)).is_empty());
        assert_eq!(p.subscribe(Service::ContentDirectory, Some(&g.sid), None, None, MAX_TIMEOUT, t0 + Duration::from_secs(171)).unwrap_err(), Refusal::Precondition);

        let g = p.subscribe(Service::ConnectionManager, None, Some("upnp:event"), cb, MAX_TIMEOUT, t0).unwrap();
        assert_eq!(p.unsubscribe(Service::ConnectionManager, Some(&g.sid), Some("upnp:event"), None).unwrap_err(), Refusal::Incompatible);
        assert_eq!(p.unsubscribe(Service::ConnectionManager, None, None, None).unwrap_err(), Refusal::Precondition);
        assert_eq!(p.unsubscribe(Service::ContentDirectory, Some(&g.sid), None, None).unwrap_err(), Refusal::Precondition);
        p.unsubscribe(Service::ConnectionManager, Some(&g.sid), None, None).unwrap();
        assert_eq!(p.len(), 0);
    }

    #[test]
    fn table_fills_up() {
        let p = Publisher::default();
        let t0 = Instant::now();
        for _ in 0..MAX_SUBSCRIPTIONS {
            p.subscribe(Service::ContentDirectory, None, Some("upnp:event"), Some("<http://10.0.0.1/>"), MAX_TIMEOUT, t0).unwrap();
        }
        assert_eq!(p.subscribe(Service::ContentDirectory, None, Some("upnp:event"), Some("<http://10.0.0.1/>"), MAX_TIMEOUT, t0).unwrap_err(), Refusal::Full);
        // Expired ones make room.
        assert!(p.subscribe(Service::ContentDirectory, None, Some("upnp:event"), Some("<http://10.0.0.1/>"), MAX_TIMEOUT, t0 + MAX_TIMEOUT + Duration::from_secs(1)).is_ok());
        assert_eq!(p.len(), 1);
    }

    #[test]
    fn seq_wraps_past_zero() {
        let p = Publisher::default();
        let t0 = Instant::now();
        let g = p.subscribe(Service::ContentDirectory, None, Some("upnp:event"), Some("<http://10.0.0.1/>"), MAX_TIMEOUT, t0).unwrap();
        p.subs.lock().unwrap().get_mut(&g.sid).unwrap().seq = u32::MAX;
        assert_eq!(p.take_round(Service::ContentDirectory, None, t0)[0].1, u32::MAX);
        assert_eq!(p.take_round(Service::ContentDirectory, None, t0)[0].1, 1);
    }

    #[test]
    fn property_set_is_the_spec_shape() {
        let xml = property_set(&[("SystemUpdateID", "7"), ("SinkProtocolInfo", "")]);
        assert!(xml.starts_with("<?xml version=\"1.0\"?>\n<e:propertyset xmlns:e=\"urn:schemas-upnp-org:event-1-0\">"));
        assert!(xml.contains("<e:property><SystemUpdateID>7</SystemUpdateID></e:property>"));
        assert!(xml.contains("<e:property><SinkProtocolInfo></SinkProtocolInfo></e:property>"));
        assert!(xml.ends_with("</e:propertyset>"));
        assert!(property_set(&[("X", "a<b")]).contains("a&lt;b"));
    }
}

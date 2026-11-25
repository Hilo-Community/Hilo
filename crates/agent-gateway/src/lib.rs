//! One Host-owned proxy per agent session. The sandbox can only reach this loopback port.
//! Key mode has a fixed upstream and injects the OS-keychain key; account mode permits only
//! CONNECT to exact approved hosts on port 443. All resolved addresses must be public.
pub mod credentials;
pub mod network;

use agent_registry::profiles::ProviderProfile;
use base64::Engine;
use bytes::Bytes;
use futures_util::TryStreamExt;
use http_body_util::{combinators::UnsyncBoxBody, BodyExt, Full, Limited, StreamBody};
use hyper::{
    body::{Frame, Incoming},
    header::{HeaderMap, HeaderName, HeaderValue},
    Method, Request, Response, StatusCode,
};
use hyper_util::rt::TokioIo;
use std::sync::atomic::{AtomicU16, AtomicU64, Ordering};
use std::{convert::Infallible, io, net::SocketAddr, sync::Arc, thread, time::Duration};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{oneshot, Semaphore},
    task::JoinSet,
};
use url::Url;
use zeroize::Zeroizing;

type Error = Box<dyn std::error::Error + Send + Sync>;
type Body = UnsyncBoxBody<Bytes, Error>;
const MAX_REQUEST_BYTES: usize = 32 * 1024 * 1024;
const MAX_CONNECTIONS: usize = 32;

/// Host-only, non-secret transport evidence. Never records URLs, bodies, or headers.
#[derive(Default)]
pub struct GatewayMetrics {
    attempted: AtomicU64,
    responses: AtomicU64,
    last_status: AtomicU16,
}
#[derive(Debug, Clone, Copy)]
pub struct GatewaySnapshot {
    pub attempted: u64,
    pub responses: u64,
    pub last_status: Option<u16>,
}
impl GatewayMetrics {
    pub fn snapshot(&self) -> GatewaySnapshot {
        let status = self.last_status.load(Ordering::Relaxed);
        GatewaySnapshot {
            attempted: self.attempted.load(Ordering::Relaxed),
            responses: self.responses.load(Ordering::Relaxed),
            last_status: (status != 0).then_some(status),
        }
    }
}

pub enum GatewayMode {
    Key {
        provider: ProviderProfile,
        key: Zeroizing<String>,
    },
    Account {
        allowed_hosts: Vec<String>,
    },
}

/// Deliberately no Debug or Serialize: even the session token is private to the launch.
#[derive(Clone)]
pub struct GatewayEndpoint {
    pub address: SocketAddr,
    pub token: String,
    pub provider_id: Option<String>,
}
impl GatewayEndpoint {
    pub fn base_url(&self) -> String {
        format!(
            "http://{}/{}",
            self.address,
            self.provider_id.as_deref().unwrap_or("")
        )
    }
    pub fn proxy_url(&self) -> String {
        format!("http://hilo:{}@{}", self.token, self.address)
    }
}

pub struct AgentGateway {
    metrics: Arc<GatewayMetrics>,
    endpoint: GatewayEndpoint,
    stop: Option<oneshot::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
}
impl AgentGateway {
    pub fn start(mode: GatewayMode) -> io::Result<Self> {
        validate_mode(&mode)?;
        let mut random = [0u8; 32];
        getrandom::fill(&mut random)
            .map_err(|_| io::Error::other("session token generation failed"))?;
        let token: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let endpoint = GatewayEndpoint {
            address: listener.local_addr()?,
            token,
            provider_id: match &mode {
                GatewayMode::Key { provider, .. } => Some(provider.id.clone()),
                _ => None,
            },
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let (stop, stopped) = oneshot::channel();
        let metrics = Arc::new(GatewayMetrics::default());
        let state = Arc::new(State {
            metrics: metrics.clone(),
            endpoint: endpoint.clone(),
            mode,
        });
        let thread = thread::Builder::new()
            .name("agent-egress".into())
            .spawn(move || {
                runtime.block_on(async move {
                    let Ok(listener) = TcpListener::from_std(listener) else {
                        return;
                    };
                    serve(listener, state, stopped).await;
                });
            })?;
        Ok(Self {
            metrics,
            endpoint,
            stop: Some(stop),
            thread: Some(thread),
        })
    }
    pub fn endpoint(&self) -> &GatewayEndpoint {
        &self.endpoint
    }
    pub fn metrics(&self) -> Arc<GatewayMetrics> {
        self.metrics.clone()
    }
}
impl Drop for AgentGateway {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct State {
    metrics: Arc<GatewayMetrics>,
    endpoint: GatewayEndpoint,
    mode: GatewayMode,
}

fn validate_mode(mode: &GatewayMode) -> io::Result<()> {
    match mode {
        GatewayMode::Key { provider, key } => {
            let url = Url::parse(&provider.upstream).map_err(io::Error::other)?;
            if url.scheme() != "https"
                || url.port_or_known_default() != Some(443)
                || !url.username().is_empty()
                || url.password().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
                || !url.host_str().is_some_and(network::valid_host)
                || !provider
                    .id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
                || provider.id.is_empty()
            {
                return Err(io::Error::other("invalid provider upstream"));
            }
            HeaderName::try_from(&provider.key_header).map_err(io::Error::other)?;
            let header = HeaderValue::try_from(format!("{}{}", provider.key_prefix, key.as_str()))
                .map_err(|_| io::Error::other("invalid provider key"))?;
            if key.is_empty() || header.is_empty() {
                return Err(io::Error::other("provider key is empty"));
            }
        }
        GatewayMode::Account { allowed_hosts } => {
            if allowed_hosts.is_empty()
                || allowed_hosts.iter().any(|host| !network::valid_host(host))
            {
                return Err(io::Error::other(
                    "account network requires explicit valid hosts",
                ));
            }
        }
    }
    Ok(())
}

async fn serve(listener: TcpListener, state: Arc<State>, mut stopped: oneshot::Receiver<()>) {
    let permits = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            _ = &mut stopped => break,
            Some(_) = connections.join_next(), if !connections.is_empty() => {},
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else { break; };
                let Ok(permit) = permits.clone().try_acquire_owned() else { continue; };
                let state = state.clone();
                connections.spawn(async move {
                    let _permit = permit;
                    let service = hyper::service::service_fn(move |request| handle(request, state.clone()));
                    let connection = hyper::server::conn::http1::Builder::new()
                        .max_buf_size(64 * 1024).max_headers(64).keep_alive(false)
                        .serve_connection(TokioIo::new(stream), service).with_upgrades();
                    let _ = tokio::time::timeout(Duration::from_secs(3600), connection).await;
                });
            }
        }
    }
    connections.abort_all();
    // Dropping the runtime also cancels CONNECT upgrades; no tunnel outlives the session.
    while connections.join_next().await.is_some() {}
}

fn response(status: StatusCode, message: &'static str) -> Response<Body> {
    let mut response = Response::new(
        Full::new(Bytes::from_static(message.as_bytes()))
            .map_err(|never: Infallible| match never {})
            .boxed_unsync(),
    );
    *response.status_mut() = status;
    response
}

fn session_token(headers: &HeaderMap, expected: &str) -> bool {
    let direct = headers.get("x-hilo-session").and_then(|v| v.to_str().ok());
    let bearer = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    [direct, bearer]
        .into_iter()
        .flatten()
        .any(|token| constant_time_eq::constant_time_eq(token.as_bytes(), expected.as_bytes()))
}

fn proxy_token(headers: &HeaderMap, expected: &str) -> bool {
    let Some(encoded) = headers
        .get("proxy-authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Basic "))
    else {
        return false;
    };
    let Ok(value) = base64::engine::general_purpose::STANDARD.decode(encoded) else {
        return false;
    };
    let expected = format!("hilo:{expected}");
    constant_time_eq::constant_time_eq(&value, expected.as_bytes())
}

async fn handle(
    mut request: Request<Incoming>,
    state: Arc<State>,
) -> Result<Response<Body>, Infallible> {
    let result = match &state.mode {
        GatewayMode::Key { provider, key } => {
            if !session_token(request.headers(), &state.endpoint.token) {
                response(StatusCode::UNAUTHORIZED, "Invalid session token")
            } else {
                forward(request, provider, key, &state.metrics)
                    .await
                    .unwrap_or_else(|_| {
                        response(StatusCode::BAD_GATEWAY, "Provider connection failed")
                    })
            }
        }
        GatewayMode::Account { allowed_hosts } => {
            if !proxy_token(request.headers(), &state.endpoint.token) {
                response(
                    StatusCode::PROXY_AUTHENTICATION_REQUIRED,
                    "Invalid proxy token",
                )
            } else if request.method() != Method::CONNECT {
                response(StatusCode::FORBIDDEN, "Only HTTPS CONNECT is permitted")
            } else {
                let host = request.uri().host().unwrap_or_default();
                if request.uri().port_u16() != Some(443)
                    || !allowed_hosts.iter().any(|allowed| allowed == host)
                {
                    response(StatusCode::FORBIDDEN, "Host is not allowed")
                } else {
                    match connect(host).await {
                        Err(_) => response(
                            StatusCode::FORBIDDEN,
                            "Destination is not public or unavailable",
                        ),
                        Ok(mut upstream) => {
                            let upgrade = hyper::upgrade::on(&mut request);
                            tokio::spawn(async move {
                                if let Ok(client) = upgrade.await {
                                    let _ = tokio::time::timeout(
                                        Duration::from_secs(3600),
                                        tokio::io::copy_bidirectional(
                                            &mut TokioIo::new(client),
                                            &mut upstream,
                                        ),
                                    )
                                    .await;
                                }
                            });
                            response(StatusCode::OK, "")
                        }
                    }
                }
            }
        }
    };
    Ok(result)
}

async fn connect(host: &str) -> io::Result<TcpStream> {
    let addresses = network::resolve(host).await?;
    tokio::time::timeout(
        Duration::from_secs(10),
        TcpStream::connect(addresses.as_slice()),
    )
    .await
    .map_err(|_| io::Error::other("connection timeout"))?
}

fn upstream_url(uri: &hyper::Uri, provider: &ProviderProfile) -> Result<Url, Error> {
    if uri.scheme().is_some() || uri.authority().is_some() {
        return Err("absolute proxy URLs are forbidden".into());
    }
    let prefix = format!("/{}/", provider.id);
    let suffix = uri
        .path_and_query()
        .ok_or("missing path")?
        .as_str()
        .strip_prefix(&prefix)
        .ok_or("wrong provider route")?;
    // Build by concatenation, then verify the origin. Never use URL join on agent input.
    let base = Url::parse(&provider.upstream)?;
    let url = Url::parse(&format!(
        "{}/{suffix}",
        provider.upstream.trim_end_matches('/')
    ))?;
    let base_path = base.path().trim_end_matches('/');
    if url.origin() != base.origin()
        || !(url.path() == base_path || url.path().starts_with(&format!("{base_path}/")))
    {
        return Err("invalid provider route".into());
    }
    Ok(url)
}

fn strip_headers(headers: &mut HeaderMap) {
    // RFC 9110: Connection may name additional hop-by-hop fields.
    let extra: Vec<_> = headers
        .get_all("connection")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(',').map(|s| s.trim().to_owned()))
        .collect();
    for name in extra {
        headers.remove(name);
    }
    for name in [
        "host",
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
        "authorization",
        "x-api-key",
        "x-goog-api-key",
        "x-hilo-session",
        "cookie",
        "set-cookie",
    ] {
        headers.remove(name);
    }
}

async fn forward(
    request: Request<Incoming>,
    provider: &ProviderProfile,
    key: &str,
    metrics: &GatewayMetrics,
) -> Result<Response<Body>, Error> {
    if request.method() == Method::CONNECT {
        return Ok(response(
            StatusCode::FORBIDDEN,
            "CONNECT is unavailable in key mode",
        ));
    }
    let url = match upstream_url(request.uri(), provider) {
        Ok(url) => url,
        Err(_) => return Ok(response(StatusCode::FORBIDDEN, "Invalid provider route")),
    };
    let host = url.host_str().ok_or("missing upstream host")?;
    let addresses = network::resolve(host).await?;
    let client = reqwest::Client::builder()
        .https_only(true)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .resolve_to_addrs(host, &addresses)
        .connect_timeout(Duration::from_secs(10))
        .read_timeout(Duration::from_secs(120))
        .build()?;
    metrics.attempted.fetch_add(1, Ordering::Relaxed);
    let response = relay(request, provider, key, url, client).await?;
    metrics.responses.fetch_add(1, Ordering::Relaxed);
    metrics
        .last_status
        .store(response.status().as_u16(), Ordering::Relaxed);
    Ok(response)
}

async fn relay(
    request: Request<Incoming>,
    provider: &ProviderProfile,
    key: &str,
    url: Url,
    client: reqwest::Client,
) -> Result<Response<Body>, Error> {
    let (parts, body) = request.into_parts();
    let mut headers = parts.headers;
    strip_headers(&mut headers);
    let mut credential = HeaderValue::try_from(format!("{}{key}", provider.key_prefix))?;
    credential.set_sensitive(true);
    headers.insert(HeaderName::try_from(&provider.key_header)?, credential);
    let body = Limited::new(body, MAX_REQUEST_BYTES).into_data_stream();
    let upstream = client
        .request(parts.method, url)
        .headers(headers)
        .body(reqwest::Body::wrap_stream(body))
        .send()
        .await?;
    let mut response = Response::builder().status(upstream.status());
    let mut headers = upstream.headers().clone();
    strip_headers(&mut headers);
    *response.headers_mut().ok_or("invalid response")? = headers;
    // Every provider chunk goes straight to the agent. No collect(), compression, or SSE parsing.
    let stream = upstream
        .bytes_stream()
        .map_ok(Frame::data)
        .map_err(|error| -> Error { Box::new(error) });
    Ok(response.body(StreamBody::new(stream).boxed_unsync())?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    use std::io::{Read, Write};

    fn provider() -> ProviderProfile {
        agent_registry::profiles::provider_profile("openai")
            .unwrap()
            .clone()
    }

    #[tokio::test(flavor = "current_thread")]
    async fn sse_is_delivered_before_the_provider_finishes_and_only_host_credentials_are_forwarded()
    {
        // Exercise the relay against a controlled HTTP fixture. Production reaches it only
        // after fixed-origin HTTPS and public-DNS validation in forward().
        let source = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = Url::parse(&format!(
            "http://{}/responses",
            source.local_addr().unwrap()
        ))
        .unwrap();
        let release = Arc::new(tokio::sync::Notify::new());
        let source_release = release.clone();
        let upstream = tokio::spawn(async move {
            let (socket, _) = source.accept().await.unwrap();
            let service = hyper::service::service_fn(move |request: Request<Incoming>| {
                let release = source_release.clone();
                async move {
                    assert_eq!(request.headers()["authorization"], "Bearer host-only-key");
                    assert!(request.headers().get("x-hilo-session").is_none());
                    assert!(request.headers().get("cookie").is_none());
                    let first = futures_util::stream::once(async {
                        Ok::<_, io::Error>(Frame::data(Bytes::from_static(b"data: first\n\n")))
                    });
                    let last = futures_util::stream::once(async move {
                        release.notified().await;
                        Ok::<_, io::Error>(Frame::data(Bytes::from_static(b"data: last\n\n")))
                    });
                    Ok::<_, Infallible>(
                        Response::builder()
                            .header("content-type", "text/event-stream")
                            .body(StreamBody::new(first.chain(last)))
                            .unwrap(),
                    )
                }
            });
            hyper::server::conn::http1::Builder::new()
                .keep_alive(false)
                .serve_connection(TokioIo::new(socket), service)
                .await
                .unwrap();
        });
        let proxy = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = proxy.local_addr().unwrap();
        let relay_task = tokio::spawn(async move {
            let (socket, _) = proxy.accept().await.unwrap();
            let client = reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap();
            let service = hyper::service::service_fn(move |request| {
                let client = client.clone();
                let url = url.clone();
                async move {
                    Ok::<_, Infallible>(
                        relay(request, &provider(), "host-only-key", url, client)
                            .await
                            .unwrap(),
                    )
                }
            });
            hyper::server::conn::http1::Builder::new()
                .keep_alive(false)
                .serve_connection(TokioIo::new(socket), service)
                .await
                .unwrap();
        });
        let agent = reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(format!("http://{address}/openai/responses"))
            .header("authorization", "Bearer session-only-token")
            .header("x-hilo-session", "session-only-token")
            .header("cookie", "must-not-forward")
            .send()
            .await
            .unwrap();
        assert_eq!(agent.headers()["content-type"], "text/event-stream");
        let mut chunks = agent.bytes_stream();
        let first = tokio::time::timeout(Duration::from_secs(2), chunks.try_next())
            .await
            .expect("first SSE chunk must arrive before upstream finishes")
            .unwrap()
            .unwrap();
        assert_eq!(first.as_ref(), b"data: first\n\n");
        release.notify_one();
        let last = chunks.try_next().await.unwrap().unwrap();
        assert_eq!(last.as_ref(), b"data: last\n\n");
        assert!(chunks.try_next().await.unwrap().is_none());
        upstream.await.unwrap();
        relay_task.await.unwrap();
    }
    fn exchange(endpoint: &GatewayEndpoint, request: &str) -> String {
        let mut stream = std::net::TcpStream::connect(endpoint.address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    }

    #[test]
    fn session_tokens_and_routes_are_checked_before_any_upstream_connection() {
        let gateway = AgentGateway::start(GatewayMode::Key {
            provider: provider(),
            key: Zeroizing::new("fake-provider-secret".into()),
        })
        .unwrap();
        let endpoint = gateway.endpoint();
        for header in [
            "",
            "Authorization: Bearer wrong\r\n",
            "x-hilo-session: fake-provider-secret\r\n",
        ] {
            let response = exchange(endpoint, &format!("GET /openai/models HTTP/1.1\r\nHost: localhost\r\n{header}Connection: close\r\n\r\n"));
            assert!(response.starts_with("HTTP/1.1 401"), "{response}");
            assert!(!response.contains("fake-provider-secret"));
        }
        let response = exchange(endpoint, &format!("GET /anthropic/messages HTTP/1.1\r\nHost: localhost\r\nx-hilo-session: {}\r\nConnection: close\r\n\r\n", endpoint.token));
        assert!(response.starts_with("HTTP/1.1 403"), "{response}");
        let address = endpoint.address;
        drop(gateway);
        assert!(std::net::TcpStream::connect(address).is_err());
    }

    #[test]
    fn account_proxy_refuses_other_hosts_ip_literals_ports_and_http() {
        let gateway = AgentGateway::start(GatewayMode::Account {
            allowed_hosts: vec!["auth.openai.com".into()],
        })
        .unwrap();
        let endpoint = gateway.endpoint();
        let auth =
            base64::engine::general_purpose::STANDARD.encode(format!("hilo:{}", endpoint.token));
        for target in [
            "evil.example.com:443",
            "127.0.0.1:443",
            "169.254.169.254:443",
            "auth.openai.com.evil.com:443",
            "auth.openai.com:80",
            "auth.openai.com:8443",
        ] {
            let response = exchange(endpoint, &format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\nProxy-Authorization: Basic {auth}\r\n\r\n"));
            assert!(response.starts_with("HTTP/1.1 403"), "{target}: {response}");
        }
        let response = exchange(endpoint, &format!("GET http://auth.openai.com/ HTTP/1.1\r\nHost: auth.openai.com\r\nProxy-Authorization: Basic {auth}\r\n\r\n"));
        assert!(response.starts_with("HTTP/1.1 403"), "{response}");
        let response = exchange(
            endpoint,
            "CONNECT auth.openai.com:443 HTTP/1.1\r\nHost: auth.openai.com\r\n\r\n",
        );
        assert!(response.starts_with("HTTP/1.1 407"), "{response}");
        assert!(AgentGateway::start(GatewayMode::Account {
            allowed_hosts: vec![]
        })
        .is_err());
    }

    #[test]
    fn routes_cannot_change_the_origin_and_credentials_are_stripped() {
        let provider = provider();
        assert_eq!(
            upstream_url(&"/openai/responses?stream=true".parse().unwrap(), &provider)
                .unwrap()
                .as_str(),
            "https://api.openai.com/v1/responses?stream=true"
        );
        for route in [
            "http://evil.example.com/openai/responses",
            "/other/responses",
            "/openai/../../secret",
            "/openai/%2e%2e/secret",
        ] {
            assert!(
                upstream_url(&route.parse().unwrap(), &provider).is_err(),
                "{route}"
            );
        }
        let mut headers = HeaderMap::new();
        for header in [
            "authorization",
            "x-api-key",
            "x-goog-api-key",
            "x-hilo-session",
            "proxy-authorization",
            "cookie",
            "leaked-hop-header",
        ] {
            headers.insert(
                HeaderName::from_static(header),
                HeaderValue::from_static("must-not-forward"),
            );
        }
        headers.insert("connection", HeaderValue::from_static("leaked-hop-header"));
        headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
        strip_headers(&mut headers);
        assert_eq!(headers.len(), 1);
        assert_eq!(headers["anthropic-version"], "2023-06-01");
    }
}

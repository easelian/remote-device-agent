//! One local connection policy for pairing HTTP and the Agent WebSocket.
use std::io::Read;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use hyper_util::client::proxy::matcher::{Intercept, Matcher};
use rustls::pki_types::{CertificateDer, ServerName, pem::PemObject};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{Uri, header};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};
use url::Url;

use crate::error::{Error, Result};

pub trait Socket: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Socket for T {}
pub type WebSocket = WebSocketStream<MaybeTlsStream<Box<dyn Socket>>>;

#[derive(Debug)]
pub enum WebSocketError {
    Unauthorized(u16),
    Network(String),
}

// Proxy URLs may contain credentials: neither this type nor Network implements Debug.
struct ProxyPolicy {
    http: Option<String>,
    https: Option<String>,
    matcher: Matcher,
}

impl ProxyPolicy {
    fn new(http: Option<String>, https: Option<String>, no: &str) -> Result<Self> {
        for value in [&http, &https].into_iter().flatten() {
            let url = Url::parse(value).map_err(|_| Error::message("Invalid local proxy URL"))?;
            if !matches!(url.scheme(), "http" | "https")
                || url.host().is_none()
                || url.path() != "/"
                || url.query().is_some()
                || url.fragment().is_some()
            {
                return Err(Error::message("Invalid local proxy URL"));
            }
        }
        let matcher = Matcher::builder()
            .http(http.as_deref().unwrap_or(""))
            .https(https.as_deref().unwrap_or(""))
            .no(no)
            .build();
        Ok(Self {
            http,
            https,
            matcher,
        })
    }

    fn raw_for(&self, target: &Url) -> Option<&String> {
        match target.scheme() {
            "http" => self.http.as_ref(),
            "https" => self.https.as_ref(),
            _ => None,
        }
    }

    fn uri(target: &Url) -> Result<Uri> {
        target
            .as_str()
            .parse()
            .map_err(|_| Error::message("Invalid network destination"))
    }

    fn intercept(&self, target: &Url) -> Result<Option<Intercept>> {
        Ok(self.matcher.intercept(&Self::uri(target)?))
    }

    fn proxy_for_http(&self, target: &Url) -> Option<String> {
        let configured = self.raw_for(target)?;
        match self.intercept(target) {
            Ok(Some(_)) => Some(configured.clone()),
            Ok(None) => None,
            // A malformed matcher input must not silently bypass an opted-in proxy.
            Err(_) => Some(configured.clone()),
        }
    }
}

pub struct Network {
    client: reqwest::Client,
    policy: Arc<ProxyPolicy>,
    tls: Arc<rustls::ClientConfig>,
}

fn validate_update_target(url: &Url, was_https: &mut bool) -> Result<()> {
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        return Err(Error::message("Invalid native update URL"));
    }
    let loopback = match url.host() {
        Some(url::Host::Domain(host)) => host == "localhost",
        Some(url::Host::Ipv4(host)) => host == std::net::Ipv4Addr::LOCALHOST,
        Some(url::Host::Ipv6(host)) => host == std::net::Ipv6Addr::LOCALHOST,
        None => false,
    };
    if url.scheme() != "https" && !(url.scheme() == "http" && loopback && !*was_https) {
        return Err(Error::message(
            "Native updates require HTTPS, except for loopback fixtures",
        ));
    }
    *was_https |= url.scheme() == "https";
    Ok(())
}

fn env_prefer_lower(lower: &str, upper: &str) -> Result<Option<String>> {
    for name in [lower, upper] {
        if let Some(value) = std::env::var_os(name) {
            let value = value
                .into_string()
                .map_err(|_| Error::message(format!("Invalid local {name} setting")))?;
            return Ok((!value.is_empty()).then_some(value));
        }
    }
    Ok(None)
}

fn tls_config(ca_file: Option<&Path>) -> Result<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    let certificates = rustls_native_certs::load_native_certs();
    let _ = roots.add_parsable_certificates(certificates.certs);
    if let Some(path) = ca_file {
        let file = std::fs::File::open(path)?;
        let mut bytes = Vec::new();
        file.take(4 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
        if bytes.len() > 4 * 1024 * 1024 {
            return Err(Error::message("Local CA bundle exceeds 4 MiB"));
        }
        let mut count = 0;
        for certificate in CertificateDer::pem_slice_iter(&bytes) {
            let certificate = certificate.map_err(|_| Error::message("Invalid local CA bundle"))?;
            roots
                .add(certificate)
                .map_err(|_| Error::message("Invalid local CA certificate"))?;
            count += 1;
        }
        if count == 0 {
            return Err(Error::message("Local CA bundle contains no certificates"));
        }
    }
    rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|_| Error::message("Unable to configure TLS protocol versions"))
        .map(|builder| builder.with_root_certificates(roots).with_no_client_auth())
}

impl Network {
    pub fn from_env() -> Result<Self> {
        let enabled = std::env::var_os("RDH_USE_ENV_PROXY").is_some_and(|value| value == "1");
        let (http, https, no) = if enabled {
            (
                env_prefer_lower("http_proxy", "HTTP_PROXY")?,
                env_prefer_lower("https_proxy", "HTTPS_PROXY")?,
                env_prefer_lower("no_proxy", "NO_PROXY")?.unwrap_or_default(),
            )
        } else {
            (None, None, String::new())
        };
        let ca = std::env::var_os("RDH_CA_BUNDLE").filter(|value| !value.is_empty());
        Self::new(http, https, &no, ca.as_deref().map(Path::new))
    }

    pub fn new(
        http: Option<String>,
        https: Option<String>,
        no: &str,
        ca_file: Option<&Path>,
    ) -> Result<Self> {
        let policy = Arc::new(ProxyPolicy::new(http, https, no)?);
        let tls = tls_config(ca_file)?;
        let proxy_policy = Arc::clone(&policy);
        let client = reqwest::Client::builder()
            .no_proxy()
            .proxy(reqwest::Proxy::custom(move |target| {
                proxy_policy.proxy_for_http(target)
            }))
            .use_preconfigured_tls(tls.clone())
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(20))
            .connect_timeout(Duration::from_secs(15))
            .build()
            .map_err(|error| {
                Error::message(format!(
                    "Unable to initialize network client: {}",
                    error.without_url()
                ))
            })?;
        Ok(Self {
            client,
            policy,
            tls: Arc::new(tls),
        })
    }

    pub async fn post_json(
        &self,
        origin: &str,
        path: &str,
        body: &serde_json::Value,
    ) -> Result<reqwest::Response> {
        let origin = Url::parse(origin).map_err(|_| Error::message("Invalid Hub URL"))?;
        let url = origin
            .join(path)
            .map_err(|_| Error::message("Invalid Hub endpoint"))?;
        self.client
            .post(url)
            .json(body)
            .send()
            .await
            .map_err(|error| Error::message(format!("Hub request failed: {}", error.without_url())))
    }

    /// Download update metadata or a binary without device credentials. Each
    /// redirect is checked independently; HTTPS cannot downgrade to HTTP.
    pub async fn get_update(&self, initial: &Url, limit: usize) -> Result<Vec<u8>> {
        let mut url = initial.clone();
        let mut was_https = false;
        for redirects in 0..=5 {
            validate_update_target(&url, &mut was_https)?;
            let mut response = self
                .client
                .get(url.clone())
                .timeout(Duration::from_secs(300))
                .send()
                .await
                .map_err(|error| {
                    Error::message(format!(
                        "Native update download failed: {}",
                        error.without_url()
                    ))
                })?;
            if response.status().is_redirection() {
                if redirects == 5 {
                    return Err(Error::message("Too many native update redirects"));
                }
                let location = response
                    .headers()
                    .get(header::LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .ok_or_else(|| Error::message("Invalid native update redirect"))?;
                url = url
                    .join(location)
                    .map_err(|_| Error::message("Invalid native update redirect"))?;
                continue;
            }
            if !response.status().is_success() {
                return Err(Error::message(format!(
                    "Native update request failed (HTTP {})",
                    response.status().as_u16()
                )));
            }
            if response
                .content_length()
                .is_some_and(|bytes| bytes > limit as u64)
            {
                return Err(Error::message(
                    "Native update download exceeds its size limit",
                ));
            }
            let mut bytes = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(|error| {
                Error::message(format!(
                    "Native update download failed: {}",
                    error.without_url()
                ))
            })? {
                if bytes.len().saturating_add(chunk.len()) > limit {
                    return Err(Error::message(
                        "Native update download exceeds its size limit",
                    ));
                }
                bytes.extend_from_slice(&chunk);
            }
            return Ok(bytes);
        }
        Err(Error::message("Too many native update redirects"))
    }

    pub async fn response_json(mut response: reqwest::Response) -> Result<serde_json::Value> {
        const MAX_RESPONSE: usize = 64 * 1024;
        if response
            .content_length()
            .is_some_and(|length| length > MAX_RESPONSE as u64)
        {
            return Err(Error::message("Hub response exceeds 64 KiB"));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|error| {
            Error::message(format!("Hub response failed: {}", error.without_url()))
        })? {
            if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE {
                return Err(Error::message("Hub response exceeds 64 KiB"));
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(serde_json::from_slice(&bytes)?)
    }

    pub async fn websocket(
        &self,
        origin: &str,
        device_id: &str,
        token: &str,
    ) -> std::result::Result<WebSocket, WebSocketError> {
        let mut destination =
            Url::parse(origin).map_err(|_| WebSocketError::Network("Invalid Hub URL".into()))?;
        destination.set_path(&format!("/agent/{device_id}"));
        let target = destination.clone();
        let scheme = if destination.scheme() == "https" {
            "wss"
        } else {
            "ws"
        };
        destination
            .set_scheme(scheme)
            .map_err(|_| WebSocketError::Network("Invalid WebSocket scheme".into()))?;
        let mut request = destination
            .as_str()
            .into_client_request()
            .map_err(|_| WebSocketError::Network("Invalid WebSocket request".into()))?;
        let mut authorization = format!("Bearer {token}")
            .parse::<header::HeaderValue>()
            .map_err(|_| WebSocketError::Network("Invalid device credential".into()))?;
        authorization.set_sensitive(true);
        request
            .headers_mut()
            .insert(header::AUTHORIZATION, authorization);
        let config = WebSocketConfig::default()
            .max_message_size(Some(2 * 1024 * 1024))
            .max_frame_size(Some(2 * 1024 * 1024))
            .max_write_buffer_size(4 * 1024 * 1024);
        let result = tokio::time::timeout(Duration::from_secs(15), async {
            let proxy = self
                .policy
                .intercept(&target)
                .map_err(|_| WebSocketError::Network("Invalid network destination".into()))?;
            let stream = if let Some(proxy) = proxy {
                self.proxy_tunnel(&target, &proxy).await?
            } else {
                let host = target
                    .host_str()
                    .ok_or_else(|| WebSocketError::Network("Invalid Hub host".into()))?;
                let port = target
                    .port_or_known_default()
                    .ok_or_else(|| WebSocketError::Network("Invalid Hub port".into()))?;
                let host = host.trim_start_matches('[').trim_end_matches(']');
                let stream = TcpStream::connect((host, port))
                    .await
                    .map_err(|_| WebSocketError::Network("Gateway connection failed".into()))?;
                Box::new(stream) as Box<dyn Socket>
            };
            let (socket, _) = tokio_tungstenite::client_async_tls_with_config(
                request,
                stream,
                Some(config),
                Some(Connector::Rustls(Arc::clone(&self.tls))),
            )
            .await
            .map_err(|error| match error {
                tokio_tungstenite::tungstenite::Error::Http(response)
                    if matches!(response.status().as_u16(), 401 | 403) =>
                {
                    WebSocketError::Unauthorized(response.status().as_u16())
                }
                tokio_tungstenite::tungstenite::Error::Http(response) => {
                    WebSocketError::Network(format!(
                        "Gateway handshake failed (HTTP {})",
                        response.status().as_u16()
                    ))
                }
                _ => WebSocketError::Network(
                    "Gateway connection failed (network or TLS error)".into(),
                ),
            })?;
            Ok(socket)
        })
        .await;
        result.unwrap_or_else(|_| {
            Err(WebSocketError::Network(
                "Gateway handshake timed out".into(),
            ))
        })
    }

    async fn proxy_tunnel(
        &self,
        target: &Url,
        proxy: &Intercept,
    ) -> std::result::Result<Box<dyn Socket>, WebSocketError> {
        let proxy_uri = proxy.uri();
        let proxy_host = proxy_uri
            .host()
            .ok_or_else(|| WebSocketError::Network("Invalid local proxy host".into()))?;
        let proxy_host = proxy_host.trim_start_matches('[').trim_end_matches(']');
        let proxy_scheme = proxy_uri.scheme_str().unwrap_or("http");
        let proxy_port =
            proxy_uri
                .port_u16()
                .unwrap_or(if proxy_scheme == "https" { 443 } else { 80 });
        let tcp = TcpStream::connect((proxy_host, proxy_port))
            .await
            .map_err(|_| WebSocketError::Network("Local proxy connection failed".into()))?;
        let mut stream: Box<dyn Socket> = if proxy_scheme == "https" {
            let name = ServerName::try_from(proxy_host.to_owned())
                .map_err(|_| WebSocketError::Network("Invalid local proxy host".into()))?;
            let connector = tokio_rustls::TlsConnector::from(Arc::clone(&self.tls));
            Box::new(connector.connect(name, tcp).await.map_err(|_| {
                WebSocketError::Network("Local proxy TLS verification failed".into())
            })?)
        } else {
            Box::new(tcp)
        };
        let target_host = target
            .host_str()
            .ok_or_else(|| WebSocketError::Network("Invalid Hub host".into()))?;
        let target_port = target
            .port_or_known_default()
            .ok_or_else(|| WebSocketError::Network("Invalid Hub port".into()))?;
        let authority = if target_host.contains(':') && !target_host.starts_with('[') {
            format!("[{target_host}]:{target_port}")
        } else {
            format!("{target_host}:{target_port}")
        };
        let mut request = format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n");
        if let Some(auth) = proxy.basic_auth() {
            request.push_str("Proxy-Authorization: ");
            request.push_str(
                auth.to_str().map_err(|_| {
                    WebSocketError::Network("Invalid local proxy credentials".into())
                })?,
            );
            request.push_str("\r\n");
        }
        request.push_str("\r\n");
        stream
            .write_all(request.as_bytes())
            .await
            .map_err(|_| WebSocketError::Network("Local proxy CONNECT failed".into()))?;
        stream
            .flush()
            .await
            .map_err(|_| WebSocketError::Network("Local proxy CONNECT failed".into()))?;
        // Read exactly the header. Buffering beyond CRLFCRLF could consume bytes
        // belonging to the tunneled protocol and corrupt the TLS handshake.
        let mut response = Vec::new();
        while !response.ends_with(b"\r\n\r\n") {
            if response.len() >= 16 * 1024 {
                return Err(WebSocketError::Network(
                    "Local proxy response is too large".into(),
                ));
            }
            let mut byte = [0];
            stream
                .read_exact(&mut byte)
                .await
                .map_err(|_| WebSocketError::Network("Local proxy CONNECT failed".into()))?;
            response.push(byte[0]);
        }
        let line = response
            .split(|byte| *byte == b'\n')
            .next()
            .unwrap_or_default();
        let line = std::str::from_utf8(line)
            .map_err(|_| WebSocketError::Network("Invalid local proxy response".into()))?;
        let mut fields = line.split_whitespace();
        let protocol = fields.next().unwrap_or("");
        let status = fields
            .next()
            .and_then(|value| value.parse::<u16>().ok())
            .unwrap_or(0);
        if !matches!(protocol, "HTTP/1.0" | "HTTP/1.1") || !(200..300).contains(&status) {
            return Err(WebSocketError::Network(format!(
                "Local proxy CONNECT rejected (HTTP {status})"
            )));
        }
        Ok(stream)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_rules_are_explicit_and_shared() {
        let policy = ProxyPolicy::new(
            Some("http://u:p@127.0.0.1:3128".into()),
            None,
            "localhost,127.0.0.1,.example.org",
        )
        .unwrap();
        assert!(
            policy
                .proxy_for_http(&Url::parse("http://127.0.0.1:8000").unwrap())
                .is_none()
        );
        assert!(
            policy
                .proxy_for_http(&Url::parse("http://host.example.org").unwrap())
                .is_none()
        );
        assert!(
            policy
                .proxy_for_http(&Url::parse("http://different.example").unwrap())
                .is_some()
        );
        assert!(
            policy
                .proxy_for_http(&Url::parse("https://different.example").unwrap())
                .is_none()
        );
        assert!(ProxyPolicy::new(Some("socks5://example.com".into()), None, "").is_err());
    }

    #[tokio::test]
    async fn update_downloads_are_bounded_credential_free_and_reject_unsafe_redirects() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for _ in 0..4 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                while !bytes.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    socket.read_exact(&mut byte).await.unwrap();
                    bytes.push(byte[0]);
                }
                let request = String::from_utf8(bytes).unwrap();
                let response = if request.starts_with("GET /redirect ") {
                    "HTTP/1.1 302 Found\r\nLocation: /payload\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                } else if request.starts_with("GET /payload ") {
                    "HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\nabc"
                } else if request.starts_with("GET /oversize ") {
                    "HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nabcde"
                } else {
                    "HTTP/1.1 302 Found\r\nLocation: http://example.invalid/file\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                };
                socket.write_all(response.as_bytes()).await.unwrap();
                requests.push(request);
            }
            requests
        });
        let network = Network::new(None, None, "", None).unwrap();
        let url = |path: &str| Url::parse(&format!("http://{address}/{path}")).unwrap();
        assert_eq!(
            network.get_update(&url("redirect"), 3).await.unwrap(),
            b"abc"
        );
        assert!(network.get_update(&url("oversize"), 4).await.is_err());
        assert!(network.get_update(&url("escape"), 16).await.is_err());
        for request in server.await.unwrap() {
            let request = request.to_ascii_lowercase();
            assert!(!request.contains("authorization:"));
        }
        let mut secure = false;
        validate_update_target(
            &Url::parse("https://example.test/index").unwrap(),
            &mut secure,
        )
        .unwrap();
        assert!(validate_update_target(&url("payload"), &mut secure).is_err());
        assert!(
            validate_update_target(
                &Url::parse("https://user:secret@example.test/").unwrap(),
                &mut false
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn connect_sends_only_proxy_credentials_before_the_tls_tunnel() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            while !bytes.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                socket.read_exact(&mut byte).await.unwrap();
                bytes.push(byte[0]);
            }
            socket
                .write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n")
                .await
                .unwrap();
            String::from_utf8(bytes).unwrap()
        });
        let network = Network::new(
            None,
            Some(format!("http://proxy-user:proxy-password@127.0.0.1:{port}")),
            "",
            None,
        )
        .unwrap();
        let result = network
            .websocket(
                "https://unresolvable.invalid",
                "device",
                "device-token-secret-value-000000000000",
            )
            .await;
        assert!(matches!(result, Err(WebSocketError::Network(_))));
        let request = server.await.unwrap();
        assert!(request.starts_with("CONNECT unresolvable.invalid:443 HTTP/1.1\r\n"));
        assert!(
            request.contains("Proxy-Authorization: Basic cHJveHktdXNlcjpwcm94eS1wYXNzd29yZA==\r\n")
        );
        assert!(!request.contains("device-token-secret"));
        assert!(!request.contains("\r\nAuthorization:"));
    }
}

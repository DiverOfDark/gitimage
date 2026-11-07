//! Forwards upload requests to the replica that owns the upload.
//!
//! The owner is reached at `https://<forward_host>:<forward_port>` with the host name pinned
//! to the owner's pod IP, so the serving certificate (issued for the Service name) verifies
//! against `forward_ca_pem`. Without a CA the request goes over plain HTTP (tests only).

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Mutex;
use std::time::Duration;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderMap, HeaderName, header};
use axum::response::Response;
use futures::TryStreamExt;

use super::error::ApiError;
use crate::OciError;

/// Marks a forwarded request so a confused receiver never forwards it again.
const FORWARDED: HeaderName = HeaderName::from_static("x-gitimage-forwarded");

pub(super) struct Forwarder {
    host: String,
    port: u16,
    ca: Option<reqwest::Certificate>,
    /// One client per owner IP, since the DNS override is per client.
    clients: Mutex<HashMap<IpAddr, reqwest::Client>>,
}

/// Headers that describe a single connection and must not be relayed.
fn hop_by_hop(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "host"
    )
}

fn relay_headers(from: &HeaderMap, to: &mut HeaderMap) {
    for (name, value) in from {
        if !hop_by_hop(name) {
            to.append(name.clone(), value.clone());
        }
    }
}

impl Forwarder {
    pub(super) fn new(host: String, port: u16, ca_pem: Option<&[u8]>) -> Result<Self, OciError> {
        let ca = ca_pem
            .map(reqwest::Certificate::from_pem)
            .transpose()
            .map_err(|e| OciError::Backend(format!("forward CA: {e}")))?;
        Ok(Self {
            host,
            port,
            ca,
            clients: Mutex::default(),
        })
    }

    fn client(&self, ip: IpAddr) -> Result<reqwest::Client, ApiError> {
        let mut clients = self.clients.lock().expect("clients lock");
        if let Some(c) = clients.get(&ip) {
            return Ok(c.clone());
        }
        let mut builder = reqwest::Client::builder()
            .resolve(&self.host, SocketAddr::new(ip, self.port))
            .connect_timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy();
        if let Some(ca) = &self.ca {
            builder = builder.tls_certs_only([ca.clone()]).https_only(true);
        }
        let client = builder
            .build()
            .map_err(|e| ApiError::from(OciError::Backend(format!("forward client: {e}"))))?;
        clients.insert(ip, client.clone());
        Ok(client)
    }

    /// Relays `req` to the replica at `owner` and streams its response back.
    pub(super) async fn forward(&self, owner: &str, req: Request) -> Result<Response, ApiError> {
        let unknown = || ApiError::from(OciError::BlobUploadUnknown);
        if req.headers().contains_key(&FORWARDED) {
            return Err(unknown());
        }
        let ip: IpAddr = owner.parse().map_err(|_| unknown())?;
        let scheme = if self.ca.is_some() { "https" } else { "http" };
        let path = req
            .uri()
            .path_and_query()
            .map(|p| p.as_str())
            .unwrap_or("/");
        let url = format!("{scheme}://{}:{}{path}", self.host, self.port);
        let mut headers = HeaderMap::new();
        relay_headers(req.headers(), &mut headers);
        headers.insert(FORWARDED, header::HeaderValue::from_static("1"));
        let method = req.method().clone();
        let body = reqwest::Body::wrap_stream(req.into_body().into_data_stream());

        tracing::debug!(%owner, %url, "forwarding upload request");
        let resp = self
            .client(ip)?
            .request(method, url)
            .headers(headers)
            .body(body)
            .send()
            .await
            .map_err(|e| {
                tracing::warn!(%owner, error = %e, "forwarding upload request failed");
                if e.is_connect() || e.is_timeout() {
                    unknown()
                } else {
                    ApiError::from(OciError::Backend(format!("forwarding to {owner}: {e}")))
                }
            })?;

        let mut out = Response::builder().status(resp.status());
        if let Some(h) = out.headers_mut() {
            relay_headers(resp.headers(), h);
        }
        let stream = resp.bytes_stream().map_err(std::io::Error::other);
        out.body(Body::from_stream(stream))
            .map_err(|e| ApiError::from(OciError::Backend(e.to_string())))
    }
}

//! ssh transport for gix on russh (design D3): no `ssh` binary. Connects, verifies the host key
//! against the Secret's `known_hosts`, authenticates with its `identity`, execs
//! `git-upload-pack '<path>'` with `GIT_PROTOCOL=version=2` and bridges the channel to the
//! blocking `Read`/`Write` gix's git-protocol connection needs. gix detects v1 vs v2 from the
//! server's first lines, so servers ignoring the env request fall back to v1 transparently.

mod known_hosts;

use std::borrow::Cow;
use std::io::{self, Read};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use gix::bstr::{BStr, BString};
use gix::protocol::transport::client::blocking_io::{RequestWriter, SetServiceResponse, Transport};
use gix::protocol::transport::client::git::ConnectMode;
use gix::protocol::transport::client::git::blocking_io::Connection;
use gix::protocol::transport::client::{
    self as tclient, MessageKind, TransportWithoutIO, WriteMode,
};
use gix::protocol::transport::{Protocol, Service};
use russh::client;
use russh::keys::{HashAlg, PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use russh::{ChannelMsg, ChannelReadHalf};
use tokio::io::AsyncWrite;
use tokio::runtime::Handle;
use tokio_util::io::SyncIoBridge;

pub(crate) use known_hosts::{HostKeyError, KnownHosts};

use crate::GitError;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// `ssh://[user@]host[:port]/path` or scp-like `[user@]host:path`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SshUrl {
    pub user: String,
    pub host: String,
    pub port: u16,
    /// What `git-upload-pack` gets as its argument.
    pub path: String,
}

pub(crate) fn is_ssh(url: &str) -> bool {
    if let Some((scheme, _)) = url.split_once("://") {
        return matches!(scheme, "ssh" | "git+ssh" | "ssh+git");
    }
    // scp-like: a ':' before any '/', and not a Windows drive letter
    match (url.find(':'), url.find('/')) {
        (Some(c), Some(s)) => c < s && c > 1,
        (Some(c), None) => c > 1,
        _ => false,
    }
}

impl SshUrl {
    pub(crate) fn parse(url: &str) -> Result<Self, GitError> {
        let bad = |why: &str| GitError::Remote(format!("invalid ssh url {url:?}: {why}"));
        let (user, host, port, path) = if let Some((scheme, rest)) = url.split_once("://") {
            if !matches!(scheme, "ssh" | "git+ssh" | "ssh+git") {
                return Err(bad("not an ssh url"));
            }
            let (authority, path) = rest
                .find('/')
                .map(|i| (&rest[..i], &rest[i..]))
                .ok_or_else(|| bad("missing path"))?;
            let (user, hostport) = match authority.rsplit_once('@') {
                Some((u, h)) => (Some(u), h),
                None => (None, authority),
            };
            let (host, port) = if let Some(v6) = hostport.strip_prefix('[') {
                let (h, rest) = v6.split_once(']').ok_or_else(|| bad("unclosed '['"))?;
                let port = match rest.strip_prefix(':') {
                    Some(p) => Some(p),
                    None if rest.is_empty() => None,
                    None => return Err(bad("garbage after ']'")),
                };
                (h, port)
            } else {
                match hostport.split_once(':') {
                    Some((h, p)) => (h, Some(p)),
                    None => (hostport, None),
                }
            };
            let port = match port {
                Some("") | None => 22,
                Some(p) => p.parse().map_err(|_| bad("invalid port"))?,
            };
            // git: ssh://host/~user/repo sends "~user/repo"
            let path = path
                .strip_prefix('/')
                .filter(|p| p.starts_with('~'))
                .unwrap_or(path);
            (user, host, port, path)
        } else {
            let (user, rest) = match url.split_once('@') {
                Some((u, r)) if !u.contains(':') && !u.contains('/') => (Some(u), r),
                _ => (None, url),
            };
            let (host, path) = match rest.strip_prefix('[') {
                Some(r) => {
                    let (h, after) = r.split_once(']').ok_or_else(|| bad("unclosed '['"))?;
                    (
                        h,
                        after.strip_prefix(':').ok_or_else(|| bad("missing ':'"))?,
                    )
                }
                None => rest.split_once(':').ok_or_else(|| bad("missing ':'"))?,
            };
            (user, host, 22, path)
        };
        if host.is_empty() {
            return Err(bad("missing host"));
        }
        if path.is_empty() || path == "/" {
            return Err(bad("missing path"));
        }
        if host.starts_with('-') || user.is_some_and(|u| u.starts_with('-')) {
            return Err(bad("host or user starts with '-'"));
        }
        Ok(Self {
            user: user.filter(|u| !u.is_empty()).unwrap_or("git").to_owned(),
            host: host.to_owned(),
            port,
            path: path.to_owned(),
        })
    }

    fn canonical(&self) -> String {
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        let path = if self.path.starts_with('/') {
            self.path.clone()
        } else {
            format!("/{}", self.path)
        };
        format!("ssh://{}@{host}:{}{path}", self.user, self.port)
    }
}

/// POSIX single-quoting, as git does for the remote command.
pub(crate) fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

struct HostKeyCheck {
    host: String,
    port: u16,
    known: Arc<KnownHosts>,
    verdict: Arc<Mutex<Option<String>>>,
}

impl client::Handler for HostKeyCheck {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let result = match key {
            PublicKeyOrCertificate::PublicKey { key, .. } => self
                .known
                .verify(&self.host, self.port, key)
                .map_err(|e| e.to_string()),
            PublicKeyOrCertificate::Certificate(_) => Err(
                "server presented a host certificate; only plain host keys are supported".into(),
            ),
        };
        match result {
            Ok(()) => Ok(true),
            Err(why) => {
                tracing::warn!(host = %self.host, port = self.port, "{why}");
                *self.verdict.lock().unwrap_or_else(|e| e.into_inner()) = Some(why);
                Ok(false)
            }
        }
    }
}

type Session = client::Handle<HostKeyCheck>;

async fn connect(url: &SshUrl, identity: &str, known_hosts: &str) -> Result<Session, GitError> {
    let known = Arc::new(KnownHosts::parse(known_hosts));
    let algorithms = known.algorithms_for(&url.host, url.port);
    if algorithms.is_empty() {
        return Err(GitError::Remote(
            HostKeyError::Unknown {
                host: known_hosts::host_port_name(&url.host, url.port),
            }
            .to_string(),
        ));
    }
    // Never format the identity or the decoding error's input into errors.
    let key = russh::keys::decode_secret_key(identity, None).map_err(|e| {
        GitError::Local(format!(
            "ssh identity is not a usable unencrypted OpenSSH private key: {e}"
        ))
    })?;

    let mut config = client::Config {
        keepalive_interval: Some(Duration::from_secs(30)),
        nodelay: true,
        ..Default::default()
    };
    config.preferred.key = Cow::Owned(algorithms);
    let verdict = Arc::new(Mutex::new(None));
    let handler = HostKeyCheck {
        host: url.host.clone(),
        port: url.port,
        known,
        verdict: verdict.clone(),
    };
    let target = format!("{}:{}", url.host, url.port);
    let mut session = match tokio::time::timeout(
        CONNECT_TIMEOUT,
        client::connect(Arc::new(config), (url.host.as_str(), url.port), handler),
    )
    .await
    {
        Err(_) => return Err(GitError::Remote(format!("ssh {target}: connect timed out"))),
        Ok(Err(e)) => {
            let why = verdict.lock().unwrap_or_else(|e| e.into_inner()).take();
            return Err(GitError::Remote(match why {
                Some(why) => format!("ssh {target}: {why}"),
                None => format!("ssh {target}: {e}"),
            }));
        }
        Ok(Ok(s)) => s,
    };

    let hash = if key.algorithm().is_rsa() {
        // rsa-sha2-512/256 only: never the SHA-1 `ssh-rsa` signature.
        match session.best_supported_rsa_hash().await {
            Ok(Some(Some(h))) => Some(h),
            _ => Some(HashAlg::Sha256),
        }
    } else {
        None
    };
    let auth = session
        .authenticate_publickey(
            url.user.clone(),
            PrivateKeyWithHashAlg::new(Arc::new(key), hash),
        )
        .await
        .map_err(|e| GitError::Remote(format!("ssh {target}: authentication: {e}")))?;
    if !auth.success() {
        return Err(GitError::Remote(format!(
            "ssh {target}: server rejected the identity for user {:?}",
            url.user
        )));
    }
    Ok(session)
}

/// What the server said besides stdout.
#[derive(Default)]
struct Exit {
    status: Option<u32>,
    stderr: String,
    signal: Option<String>,
}

/// Blocking reader over the channel's stdout, fed by a pump task. Bounded, so russh's window
/// flow control still applies.
struct ChannelReader {
    rx: tokio::sync::mpsc::Receiver<Bytes>,
    buf: Bytes,
    exit: Arc<Mutex<Exit>>,
    read_any: bool,
}

impl Read for ChannelReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        while self.buf.is_empty() {
            match self.rx.blocking_recv() {
                Some(b) => self.buf = b,
                None => {
                    let exit = self.exit.lock().unwrap_or_else(|e| e.into_inner());
                    let stderr = exit.stderr.trim();
                    return match (exit.status, &exit.signal) {
                        (Some(0), None) | (None, None) if self.read_any && stderr.is_empty() => {
                            Ok(0)
                        }
                        (Some(0), None) => Ok(0),
                        (status, signal) => Err(io::Error::other(format!(
                            "remote git-upload-pack failed (exit {}): {}",
                            match (status, signal) {
                                (_, Some(sig)) => format!("signal {sig}"),
                                (Some(s), _) => s.to_string(),
                                (None, None) => "unknown".into(),
                            },
                            if stderr.is_empty() {
                                "channel closed without output"
                            } else {
                                stderr
                            }
                        ))),
                    };
                }
            }
        }
        self.read_any = true;
        let n = out.len().min(self.buf.len());
        out[..n].copy_from_slice(&self.buf[..n]);
        let _ = self.buf.split_to(n);
        Ok(n)
    }
}

async fn pump(
    mut read: ChannelReadHalf,
    tx: tokio::sync::mpsc::Sender<Bytes>,
    exit: Arc<Mutex<Exit>>,
) {
    const MAX_STDERR: usize = 8 * 1024;
    let mut stdout_open = true;
    while let Some(msg) = read.wait().await {
        match msg {
            ChannelMsg::Data { data } => {
                if stdout_open && tx.send(data).await.is_err() {
                    // reader gone: keep draining so the session stays healthy
                    stdout_open = false;
                }
            }
            ChannelMsg::ExtendedData { data, ext: 1 } => {
                let mut e = exit.lock().unwrap_or_else(|e| e.into_inner());
                if e.stderr.len() < MAX_STDERR {
                    e.stderr.push_str(&String::from_utf8_lossy(&data));
                }
            }
            ChannelMsg::ExitStatus { exit_status } => {
                exit.lock().unwrap_or_else(|e| e.into_inner()).status = Some(exit_status);
            }
            ChannelMsg::ExitSignal { signal_name, .. } => {
                exit.lock().unwrap_or_else(|e| e.into_inner()).signal =
                    Some(format!("{signal_name:?}"));
            }
            ChannelMsg::Close => break,
            _ => {}
        }
    }
    // dropping `tx` ends the reader
}

type ChannelWriter = SyncIoBridge<Pin<Box<dyn AsyncWrite + Send>>>;

/// gix blocking `Transport` over one russh session. The channel (and `git-upload-pack`) is
/// started on `handshake`, when the service is known, like gix's own process transport.
pub(crate) struct SshTransport {
    rt: Handle,
    session: Arc<Session>,
    url: SshUrl,
    canonical: BString,
    connection: Option<Connection<ChannelReader, ChannelWriter>>,
}

impl SshTransport {
    /// Connects and authenticates. Must run on a thread outside the runtime (e.g. inside
    /// `spawn_blocking`), with `rt` the handle of the runtime driving the session.
    pub(crate) fn connect(
        rt: Handle,
        url: &str,
        identity: &str,
        known_hosts: &str,
    ) -> Result<Self, GitError> {
        let url = SshUrl::parse(url)?;
        let session = rt.block_on(connect(&url, identity, known_hosts))?;
        Ok(Self {
            rt,
            session: Arc::new(session),
            canonical: url.canonical().into(),
            url,
            connection: None,
        })
    }

    fn open(&self, service: Service) -> io::Result<(ChannelReader, ChannelWriter)> {
        let session = self.session.clone();
        let command = format!("{} {}", service.as_str(), shell_quote(&self.url.path));
        let (read, writer) = self.rt.block_on(async move {
            let channel = session
                .channel_open_session()
                .await
                .map_err(|e| io::Error::other(format!("ssh: open session channel: {e}")))?;
            // Servers not accepting the env request ignore it; gix then sees a v1 advertisement.
            channel
                .set_env(false, "GIT_PROTOCOL", "version=2")
                .await
                .map_err(|e| io::Error::other(format!("ssh: env request: {e}")))?;
            channel
                .exec(false, command)
                .await
                .map_err(|e| io::Error::other(format!("ssh: exec: {e}")))?;
            let (read, write) = channel.split();
            let writer: Pin<Box<dyn AsyncWrite + Send>> = Box::pin(write.make_writer());
            Ok::<_, io::Error>((read, writer))
        })?;
        let (tx, rx) = tokio::sync::mpsc::channel(32);
        let exit = Arc::new(Mutex::new(Exit::default()));
        self.rt.spawn(pump(read, tx, exit.clone()));
        Ok((
            ChannelReader {
                rx,
                buf: Bytes::new(),
                exit,
                read_any: false,
            },
            SyncIoBridge::new_with_handle(writer, self.rt.clone()),
        ))
    }
}

impl Drop for SshTransport {
    fn drop(&mut self) {
        // Drop the connection (closing stdin) first, then end the session without blocking.
        self.connection = None;
        let session = self.session.clone();
        self.rt.spawn(async move {
            let _ = session
                .disconnect(russh::Disconnect::ByApplication, "", "en")
                .await;
        });
    }
}

impl TransportWithoutIO for SshTransport {
    fn to_url(&self) -> Cow<'_, BStr> {
        Cow::Borrowed(self.canonical.as_ref())
    }

    fn connection_persists_across_multiple_requests(&self) -> bool {
        true
    }

    fn configure(&mut self, _config: &dyn std::any::Any) -> gix::error::Result {
        Ok(())
    }
}

impl Transport for SshTransport {
    fn handshake<'a>(
        &mut self,
        service: Service,
        extra_parameters: &'a [(&'a str, Option<&'a str>)],
    ) -> Result<SetServiceResponse<'_>, tclient::Error> {
        let (reader, writer) = self.open(service).map_err(tclient::Error::Io)?;
        let connection = Connection::new(
            reader,
            writer,
            Protocol::V2,
            self.url.path.clone(),
            None::<(&str, Option<u16>)>,
            ConnectMode::Process,
            false,
        )
        .custom_url(Some(self.canonical.clone()));
        self.connection
            .insert(connection)
            .handshake(service, extra_parameters)
    }

    fn request(
        &mut self,
        write_mode: WriteMode,
        on_into_read: MessageKind,
        trace: bool,
    ) -> Result<RequestWriter<'_>, tclient::Error> {
        self.connection
            .as_mut()
            .ok_or(tclient::Error::MissingHandshake)?
            .request(write_mode, on_into_read, trace)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(url: &str) -> SshUrl {
        SshUrl::parse(url).unwrap()
    }

    #[test]
    fn detects_ssh_urls() {
        assert!(is_ssh("git@github.com:you/app.git"));
        assert!(is_ssh("github.com:you/app.git"));
        assert!(is_ssh("ssh://git@host:22/you/app.git"));
        assert!(is_ssh("git+ssh://host/you/app.git"));
        assert!(!is_ssh("https://github.com/you/app.git"));
        assert!(!is_ssh("https://user@github.com/you/app.git"));
        assert!(!is_ssh("/srv/git/app.git"));
        assert!(!is_ssh("./a:b"));
        assert!(!is_ssh("C:/repo"));
    }

    #[test]
    fn parses_scp_like() {
        assert_eq!(
            p("git@github.com:you/app.git"),
            SshUrl {
                user: "git".into(),
                host: "github.com".into(),
                port: 22,
                path: "you/app.git".into()
            }
        );
        let u = p("deploy@host:/srv/app.git");
        assert_eq!(
            (u.user.as_str(), u.path.as_str()),
            ("deploy", "/srv/app.git")
        );
        assert_eq!(p("host:app").user, "git");
        assert_eq!(p("[::1]:app").host, "::1");
    }

    #[test]
    fn parses_ssh_scheme() {
        assert_eq!(
            p("ssh://me@git.example.com:2222/team/app.git"),
            SshUrl {
                user: "me".into(),
                host: "git.example.com".into(),
                port: 2222,
                path: "/team/app.git".into()
            }
        );
        let u = p("ssh://git.example.com/app.git");
        assert_eq!((u.user.as_str(), u.port), ("git", 22));
        assert_eq!(p("ssh://host/~me/app.git").path, "~me/app.git");
        let v6 = p("ssh://git@[::1]:2200/app.git");
        assert_eq!((v6.host.as_str(), v6.port), ("::1", 2200));
        assert_eq!(v6.canonical(), "ssh://git@[::1]:2200/app.git");
        assert_eq!(p("git@h:a/b").canonical(), "ssh://git@h:22/a/b");
    }

    #[test]
    fn rejects_bad_urls() {
        for bad in [
            "ssh://host",
            "ssh://host/",
            "ssh://:22/x",
            "ssh://host:abc/x",
            "ssh://-oProxyCommand=x/y",
            "https://host/x",
            "git@host:",
        ] {
            assert!(SshUrl::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn quotes_paths() {
        assert_eq!(shell_quote("you/app.git"), "'you/app.git'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
    }
}

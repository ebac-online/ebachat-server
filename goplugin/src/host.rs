//! A plugin process and its connection, from the host side: launch and handshake, yamux, the stream broker, and
//! the hooks the host calls.
//!
//! Based on HashiCorp go-plugin (MPL-2.0) and Mattermost's `server/public/plugin` (Apache-2.0), Copyright (c)
//! 2015-present Mattermost, Inc.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use futures_util::StreamExt;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::oneshot;
use tokio_yamux::{Config, Control, Session, StreamHandle};

use crate::gob::{Marshal, Ty, Value};
use crate::rpc::{self, EMPTY, empty};

/// Mattermost's handshake: plugins refuse to start without this cookie.
const MAGIC_COOKIE_KEY: &str = "MATTERMOST_PLUGIN";
const MAGIC_COOKIE_VALUE: &str = "Securely message teams, anywhere.";
/// How long a plugin has to print its handshake line.
const START_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the broker waits for the other side to open a stream it announced.
const BROKER_TIMEOUT: Duration = Duration::from_secs(5);
/// The host's environment variables a plugin gets; everything else (database URLs, keys) stays with the host.
const PASSED_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "TZ",
    "LANG",
    "LC_ALL",
    "TMPDIR",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "no_proxy",
];

trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

/// go-plugin's `MuxBroker`: extra streams over the plugin's yamux session, each opened by one side with its id
/// (4 bytes, little endian) and acknowledged by the other with the same id.
pub struct Broker {
    control: Control,
    next: AtomicU32,
    slots: Mutex<HashMap<u32, Slot>>,
}

enum Slot {
    Waiting(oneshot::Sender<StreamHandle>),
    Ready(StreamHandle),
}

impl Broker {
    fn inbound(self: &Arc<Self>, mut s: StreamHandle) {
        let b = self.clone();
        tokio::spawn(async move {
            let Ok(id) = s.read_u32_le().await else { return };
            crate::trace(|| format!("inbound stream {id}"));
            let parked = {
                let mut slots = b.slots.lock().unwrap_or_else(|e| e.into_inner());
                match slots.remove(&id) {
                    Some(Slot::Waiting(tx)) => {
                        let _ = tx.send(s);
                        false
                    }
                    _ => {
                        slots.insert(id, Slot::Ready(s));
                        true
                    }
                }
            };
            if parked {
                // A stream nobody accepts is dropped, as go-plugin's timeoutWait does.
                tokio::time::sleep(BROKER_TIMEOUT).await;
                let mut slots = b.slots.lock().unwrap_or_else(|e| e.into_inner());
                if matches!(slots.get(&id), Some(Slot::Ready(_))) {
                    slots.remove(&id);
                }
            }
        });
    }

    pub fn next_id(&self) -> u32 {
        self.next.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// The stream the plugin opens with `id`.
    pub async fn accept(&self, id: u32) -> Result<StreamHandle> {
        let ready = {
            let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
            match slots.remove(&id) {
                Some(Slot::Ready(s)) => Ok(s),
                _ => {
                    let (tx, rx) = oneshot::channel();
                    slots.insert(id, Slot::Waiting(tx));
                    Err(rx)
                }
            }
        };
        let rx = match ready {
            Ok(s) => return ack(s, id).await,
            Err(rx) => rx,
        };
        let s = match tokio::time::timeout(BROKER_TIMEOUT, rx).await {
            Ok(Ok(s)) => s,
            _ => {
                self.slots.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
                bail!("broker: timeout waiting for stream {id}");
            }
        };
        ack(s, id).await
    }

    /// Opens the stream the plugin accepts as `id`.
    pub async fn dial(&self, id: u32) -> Result<StreamHandle> {
        crate::trace(|| format!("dial {id}"));
        let mut s = self.control.clone().open_stream().await.map_err(|e| anyhow!("broker: open stream: {e:?}"))?;
        s.write_u32_le(id).await?;
        s.flush().await?;
        crate::trace(|| format!("dial {id}: sent, waiting for ack"));
        let got = s.read_u32_le().await?;
        crate::trace(|| format!("dial {id}: ack {got}"));
        if got != id {
            bail!("broker: bad ack {got} (expected {id})");
        }
        Ok(s)
    }
}

async fn ack(mut s: StreamHandle, id: u32) -> Result<StreamHandle> {
    s.write_u32_le(id).await?;
    s.flush().await?;
    Ok(s)
}

/// Handles the plugin's API calls: the method (`Plugin.CreatePost`) and its `Z_..Args`, giving the `Z_..Returns`.
pub type ApiHandler = Arc<dyn Fn(String, Value) -> rpc::Reply + Send + Sync>;

/// A running plugin.
pub struct Plugin {
    child: tokio::sync::Mutex<Child>,
    pub broker: Arc<Broker>,
    control: Arc<rpc::Client<StreamHandle>>,
    pub hooks: Arc<rpc::Client<StreamHandle>>,
    /// The hooks it implements, by name.
    pub implemented: Vec<String>,
}

/// One line of what the plugin writes to its stdout or stderr.
pub type LogLine = Arc<dyn Fn(&str) + Send + Sync>;

impl Plugin {
    /// Starts the plugin's executable in its bundle directory and connects to it: handshake, yamux, the control
    /// stream, then the `hooks` plugin and the hooks it implements.
    pub async fn start(executable: &Path, dir: &Path, log: LogLine) -> Result<Plugin> {
        let mut cmd = Command::new(executable);
        cmd.current_dir(dir)
            .env_clear()
            .envs(PASSED_ENV.iter().filter_map(|k| std::env::var(k).ok().map(|v| (*k, v))))
            .env(MAGIC_COOKIE_KEY, MAGIC_COOKIE_VALUE)
            .env("PLUGIN_MIN_PORT", "10000")
            .env("PLUGIN_MAX_PORT", "25000")
            .env("PLUGIN_PROTOCOL_VERSIONS", "1")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd.spawn().with_context(|| format!("starting {}", executable.display()))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
        let stderr = child.stderr.take().ok_or_else(|| anyhow!("no stderr"))?;
        let mut lines = BufReader::new(stdout).lines();
        let handshake = tokio::time::timeout(START_TIMEOUT, lines.next_line())
            .await
            .map_err(|_| anyhow!("plugin did not start within {START_TIMEOUT:?}"))??
            .ok_or_else(|| anyhow!("plugin exited before its handshake"))?;
        let log2 = log.clone();
        tokio::spawn(async move {
            while let Ok(Some(l)) = lines.next_line().await {
                log2(&l);
            }
        });
        let log2 = log.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(l)) = lines.next_line().await {
                log2(&l);
            }
        });

        // CORE-PROTOCOL-VERSION|APP-PROTOCOL-VERSION|NETWORK|ADDRESS|PROTOCOL
        let parts: Vec<&str> = handshake.trim().split('|').collect();
        let (Some(core), Some(app), Some(network), Some(address)) =
            (parts.first(), parts.get(1), parts.get(2), parts.get(3))
        else {
            bail!("bad handshake line: {handshake:?}");
        };
        if *core != "1" || *app != "1" {
            bail!("unsupported plugin protocol {core}|{app}");
        }
        if parts.get(4).is_some_and(|p| *p != "netrpc" && !p.is_empty()) {
            bail!("unsupported plugin protocol {:?}", parts[4]);
        }
        let io: Box<dyn Io> = match *network {
            #[cfg(unix)]
            "unix" => Box::new(tokio::net::UnixStream::connect(address).await?),
            "tcp" => Box::new(tokio::net::TcpStream::connect(address).await?),
            n => bail!("unsupported network {n}"),
        };

        let mut session = Session::new_client(io, Config::default());
        let control = session.control();
        let broker = Arc::new(Broker { control: control.clone(), next: AtomicU32::new(0), slots: Mutex::default() });
        let b = broker.clone();
        tokio::spawn(async move {
            while let Some(Ok(s)) = session.next().await {
                b.inbound(s);
            }
        });
        // go-plugin's server takes the control stream, then stdout and stderr, in this order.
        let open = |mut c: Control| async move { c.open_stream().await.map_err(|e| anyhow!("open stream: {e:?}")) };
        let control_stream = open(control.clone()).await?;
        for _ in 0..2 {
            let mut s = open(control.clone()).await?;
            let log = log.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                while let Ok(n) = s.read(&mut buf).await {
                    if n == 0 {
                        break;
                    }
                    log(String::from_utf8_lossy(&buf[..n]).trim_end());
                }
            });
        }
        let control = rpc::Client::new(control_stream);
        let id = control.call("Dispenser.Dispense", &Ty::String, &Value::String("hooks".into())).await?;
        let id = id.as_i64().and_then(|i| u32::try_from(i).ok()).ok_or_else(|| anyhow!("bad dispense id"))?;
        let hooks = rpc::Client::new(broker.dial(id).await?);
        let implemented = match hooks.call("Plugin.Implemented", &EMPTY, &empty()).await? {
            Value::List(l) => l.iter().filter_map(|v| v.as_str().map(str::to_string)).collect(),
            v => bail!("bad Implemented reply: {v:?}"),
        };
        Ok(Plugin { child: tokio::sync::Mutex::new(child), broker, control, hooks, implemented })
    }

    pub fn implements(&self, hook: &str) -> bool {
        self.implemented.iter().any(|h| h == hook)
    }

    /// `OnActivate`: the plugin connects to the API stream served by `api` (and to a database stream, refused
    /// here), then runs its activation, calling the API.
    pub async fn activate(&self, api: ApiHandler) -> Result<()> {
        let api_id = self.broker.next_id();
        let b = self.broker.clone();
        tokio::spawn(async move {
            if let Ok(s) = b.accept(api_id).await {
                let _ = rpc::serve(s, move |m, a| api(m, a)).await;
            }
        });
        let db_id = self.broker.next_id();
        let b = self.broker.clone();
        tokio::spawn(async move {
            if let Ok(s) = b.accept(db_id).await {
                let _ = rpc::serve(s, |m, _| {
                    Box::pin(async move { Err(anyhow!("{m}: plugins have no database access here")) })
                })
                .await;
            }
        });
        let args =
            Value::strukt(vec![("APIMuxId", Value::Uint(api_id as u64)), ("DriverMuxId", Value::Uint(db_id as u64))]);
        let r = self.hooks.call("Plugin.OnActivate", &ON_ACTIVATE_ARGS, &args).await?;
        error_of(r.get("A")).map_or(Ok(()), |e| Err(anyhow!("OnActivate: {e}")))
    }

    pub async fn deactivate(&self) -> Result<()> {
        if !self.implements("OnDeactivate") {
            return Ok(());
        }
        let r = self.hooks.call("Plugin.OnDeactivate", &EMPTY, &empty()).await?;
        error_of(r.get("A")).map_or(Ok(()), |e| Err(anyhow!("OnDeactivate: {e}")))
    }

    pub async fn configuration_changed(&self) -> Result<()> {
        if !self.implements("OnConfigurationChange") {
            return Ok(());
        }
        let r = self.hooks.call("Plugin.OnConfigurationChange", &EMPTY, &empty()).await?;
        error_of(r.get("A")).map_or(Ok(()), |e| Err(anyhow!("OnConfigurationChange: {e}")))
    }

    /// `ExecuteCommand`: the `CommandResponse` and `AppError` as the plugin returned them.
    pub async fn execute_command(&self, context: Value, args: Value) -> Result<(Option<Value>, Option<Value>)> {
        let a = Value::strukt(vec![("A", context), ("B", args)]);
        let r = self.hooks.call("Plugin.ExecuteCommand", &EXECUTE_COMMAND_ARGS, &a).await?;
        Ok((r.get("A").cloned(), r.get("B").cloned()))
    }

    /// `ServeHTTP`: one request (`request` is an `HTTPRequestSubset` without the body), answered through the
    /// response writer the plugin calls back on a stream of its own; the body is streamed on request.
    pub async fn serve_http(&self, context: Value, request: Value, body: Vec<u8>) -> Result<HttpResponse> {
        let response_id = self.broker.next_id();
        let body_id = if body.is_empty() { 0 } else { self.broker.next_id() };
        let response = Arc::new(Mutex::new(HttpResponse { status: 0, headers: vec![], body: vec![] }));
        let (b, resp) = (self.broker.clone(), response.clone());
        let writer = tokio::spawn(async move {
            let s = b.accept(response_id).await?;
            rpc::serve(s, move |m, a| response_writer(resp.clone(), m, a)).await
        });
        if body_id != 0 {
            let b = self.broker.clone();
            tokio::spawn(async move {
                if let Ok(s) = b.accept(body_id).await {
                    let _ = serve_reader(s, body).await;
                }
            });
        }
        let args = Value::strukt(vec![
            ("ResponseWriterStream", Value::Uint(response_id as u64)),
            ("Request", request),
            ("Context", context),
            ("RequestBodyStream", Value::Uint(body_id as u64)),
        ]);
        self.hooks.call("Plugin.ServeHTTP", &SERVE_HTTP_ARGS, &args).await?;
        // The plugin closes the writer stream when its handler returns.
        let _ = tokio::time::timeout(Duration::from_secs(5), writer).await;
        let mut r = response.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if r.status == 0 {
            r.status = 200;
        }
        Ok(r)
    }

    /// go-plugin's `Control.Ping`: the plugin process answers.
    pub async fn ping(&self) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(5), self.control.call("Control.Ping", &Ty::Bool, &Value::Bool(true)))
            .await
            .map_err(|_| anyhow!("ping timed out"))??;
        Ok(())
    }

    /// Whether the plugin process has exited.
    pub async fn exited(&self) -> bool {
        self.child.lock().await.try_wait().is_ok_and(|s| s.is_some())
    }

    /// `OnDeactivate`, then go-plugin's `Control.Quit`, then makes sure the process is gone.
    pub async fn stop(&self) {
        let _ = tokio::time::timeout(Duration::from_secs(5), self.deactivate()).await;
        let _ = tokio::time::timeout(
            Duration::from_secs(2),
            self.control.call("Control.Quit", &Ty::Bool, &Value::Bool(true)),
        )
        .await;
        let mut child = self.child.lock().await;
        if tokio::time::timeout(Duration::from_secs(5), child.wait()).await.is_err() {
            let _ = child.kill().await;
        }
    }
}

/// What a plugin answered to an HTTP request.
#[derive(Clone, Debug)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, Vec<String>)>,
    pub body: Vec<u8>,
}

/// `httpResponseWriterRPCServer`.
fn response_writer(resp: Arc<Mutex<HttpResponse>>, method: String, arg: Value) -> rpc::Reply {
    Box::pin(async move {
        let mut r = resp.lock().unwrap_or_else(|e| e.into_inner());
        match method.as_str() {
            "Plugin.Header" => Ok((HEADER.clone(), header_value(&r.headers))),
            "Plugin.SyncHeader" => {
                r.headers = header_list(&arg);
                Ok((EMPTY.clone(), empty()))
            }
            "Plugin.WriteHeader" => {
                let code = arg
                    .as_i64()
                    .filter(|c| (100..=999).contains(c))
                    .ok_or_else(|| anyhow!("invalid http status code"))?;
                if r.status == 0 {
                    r.status = code as u16;
                }
                Ok((EMPTY.clone(), empty()))
            }
            "Plugin.Write" => {
                if r.status == 0 {
                    r.status = 200;
                }
                r.body.extend_from_slice(arg.as_bytes().unwrap_or_default());
                Ok((EMPTY.clone(), empty()))
            }
            "Plugin.Flush" => Ok((EMPTY.clone(), empty())),
            m => bail!("rpc: can't find method {m}"),
        }
    })
}

fn header_list(v: &Value) -> Vec<(String, Vec<String>)> {
    match v {
        Value::Map(m) => m
            .iter()
            .filter_map(|(k, v)| {
                let vals = match v {
                    Value::List(l) => l.iter().filter_map(|x| x.as_str().map(str::to_string)).collect(),
                    _ => vec![],
                };
                k.as_str().map(|k| (k.to_string(), vals))
            })
            .collect(),
        _ => vec![],
    }
}

pub fn header_value(h: &[(String, Vec<String>)]) -> Value {
    Value::Map(
        h.iter()
            .map(|(k, v)| (Value::String(k.clone()), Value::List(v.iter().map(|s| Value::String(s.clone())).collect())))
            .collect(),
    )
}

/// `serveIOReader`: the plugin asks for up to n bytes (a signed varint) and gets what is left of the body, up to n;
/// fewer than asked means the end.
async fn serve_reader(mut s: StreamHandle, body: Vec<u8>) -> Result<()> {
    let mut pos = 0usize;
    loop {
        let mut u: u64 = 0;
        let mut shift = 0;
        loop {
            let b = s.read_u8().await?;
            u |= ((b & 0x7f) as u64) << shift;
            if b < 0x80 {
                break;
            }
            shift += 7;
            if shift > 63 {
                bail!("bad varint");
            }
        }
        let n = if u & 1 != 0 { !(u >> 1) as i64 } else { (u >> 1) as i64 };
        let n = usize::try_from(n).unwrap_or(0);
        let end = pos.saturating_add(n).min(body.len());
        s.write_all(&body[pos..end]).await?;
        s.flush().await?;
        let sent = end - pos;
        pos = end;
        if sent < n {
            let _ = s.shutdown().await;
            return Ok(());
        }
    }
}

/// The message of an `error` interface the plugin returned (`*plugin.ErrorString` and the like); `None` for nil.
pub fn error_of(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::Interface(Some(i)) => Some(
            i.value
                .get("Err")
                .or_else(|| i.value.get("Message"))
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| format!("{:?}", i.value)),
        ),
        _ => None,
    }
}

/// An `error` to return to the plugin: `*plugin.ErrorString`.
pub fn error_value(msg: &str) -> Value {
    Value::iface("*plugin.ErrorString", ERROR_STRING.clone(), Value::strukt(vec![("Err", Value::String(msg.into()))]))
}

pub static ERROR_STRING: LazyLock<Ty> =
    LazyLock::new(|| Ty::strukt("ErrorString", &[("Code", Ty::Int), ("Err", Ty::String)]));
pub static HEADER: LazyLock<Ty> = LazyLock::new(|| Ty::map(Ty::String, Ty::slice(Ty::String)));
pub static CONTEXT: LazyLock<Ty> = LazyLock::new(|| {
    Ty::strukt(
        "Context",
        &[
            ("SessionId", Ty::String),
            ("ConnectionId", Ty::String),
            ("RequestId", Ty::String),
            ("IPAddress", Ty::String),
            ("AcceptLanguage", Ty::String),
            ("UserAgent", Ty::String),
        ],
    )
});
pub static COMMAND_ARGS: LazyLock<Ty> = LazyLock::new(|| {
    Ty::strukt(
        "CommandArgs",
        &[
            ("UserId", Ty::String),
            ("ChannelId", Ty::String),
            ("TeamId", Ty::String),
            ("RootId", Ty::String),
            ("ParentId", Ty::String),
            ("TriggerId", Ty::String),
            ("ConnectionId", Ty::String),
            ("Command", Ty::String),
            ("SiteURL", Ty::String),
            ("UserMentions", Ty::map(Ty::String, Ty::String)),
            ("ChannelMentions", Ty::map(Ty::String, Ty::String)),
        ],
    )
});
pub static HTTP_REQUEST: LazyLock<Ty> = LazyLock::new(|| {
    Ty::strukt(
        "HTTPRequestSubset",
        &[
            ("Method", Ty::String),
            ("URL", Ty::Marshaled(Marshal::Binary, "url.URL".into())),
            ("Proto", Ty::String),
            ("ProtoMajor", Ty::Int),
            ("ProtoMinor", Ty::Int),
            ("Header", HEADER.clone()),
            ("Host", Ty::String),
            ("RemoteAddr", Ty::String),
            ("RequestURI", Ty::String),
        ],
    )
});
static ON_ACTIVATE_ARGS: LazyLock<Ty> = LazyLock::new(|| {
    Ty::strukt("Z_OnActivateArgs", &[("APIMuxId", Ty::Uint), ("DriverMuxId", Ty::Uint), ("ContextHTTP", Ty::Bool)])
});
static EXECUTE_COMMAND_ARGS: LazyLock<Ty> =
    LazyLock::new(|| Ty::strukt("Z_ExecuteCommandArgs", &[("A", CONTEXT.clone()), ("B", COMMAND_ARGS.clone())]));
static SERVE_HTTP_ARGS: LazyLock<Ty> = LazyLock::new(|| {
    Ty::strukt(
        "Z_ServeHTTPArgs",
        &[
            ("ResponseWriterStream", Ty::Uint),
            ("Request", HTTP_REQUEST.clone()),
            ("Context", CONTEXT.clone()),
            ("RequestBodyStream", Ty::Uint),
            ("RequestContextStream", Ty::Uint),
        ],
    )
});

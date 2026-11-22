//! Go's `net/rpc` over gob, both ends: a client whose calls share one connection (matched by sequence number), and
//! a server that answers each request as soon as its handler finishes.
//!
//! On the wire every call is a `Request` header value then the argument value, every answer a `Response` header
//! then the reply (an empty struct when the header carries an error). One gob encoder and one decoder per direction
//! of the connection, so a type is defined once per connection.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use anyhow::{Result, anyhow, bail};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, WriteHalf};
use tokio::sync::oneshot;

use crate::gob::{self, Decoder, Encoder, Ty, Value};

pub static REQUEST: LazyLock<Ty> =
    LazyLock::new(|| Ty::strukt("Request", &[("ServiceMethod", Ty::String), ("Seq", Ty::Uint)]));
pub static RESPONSE: LazyLock<Ty> = LazyLock::new(|| {
    Ty::strukt("Response", &[("ServiceMethod", Ty::String), ("Seq", Ty::Uint), ("Error", Ty::String)])
});
/// `struct{}`.
pub static EMPTY: LazyLock<Ty> = LazyLock::new(|| Ty::strukt("", &[]));

pub fn empty() -> Value {
    Value::Struct(vec![])
}

/// Reads gob values from one direction of a connection.
pub struct GobReader<R> {
    r: R,
    dec: Decoder,
}

impl<R: AsyncRead + Unpin> GobReader<R> {
    pub fn new(r: R) -> Self {
        GobReader { r, dec: Decoder::default() }
    }

    pub async fn next(&mut self) -> Result<Value> {
        loop {
            if let Some(v) = self.dec.next_value()? {
                return Ok(v);
            }
            let msg = read_message(&mut self.r).await?;
            self.dec.push(msg);
        }
    }
}

async fn read_message<R: AsyncRead + Unpin>(r: &mut R) -> Result<Vec<u8>> {
    let first = r.read_u8().await?;
    let n = gob::uint_len(first).ok_or_else(|| anyhow!("gob: invalid message length"))?;
    let len = if n == 0 {
        first as u64
    } else {
        let mut b = [0u8; 8];
        r.read_exact(&mut b[8 - n..]).await?;
        u64::from_be_bytes(b)
    };
    if len > gob::MAX_MESSAGE {
        bail!("gob: message of {len} bytes");
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf).await?;
    crate::trace(|| format!("read message {len}: {:02x?}", &buf[..buf.len().min(64)]));
    Ok(buf)
}

/// Writes gob values to one direction of a connection.
pub struct GobWriter<W> {
    w: W,
    enc: Encoder,
}

impl<W: AsyncWrite + Unpin> GobWriter<W> {
    pub fn new(w: W) -> Self {
        GobWriter { w, enc: Encoder::default() }
    }

    /// Writes values back to back (a header and its body), then flushes.
    pub async fn send(&mut self, values: &[(&Ty, &Value)]) -> Result<()> {
        let mut out = vec![];
        for (ty, v) in values {
            out.extend(self.enc.encode(ty, v)?);
        }
        crate::trace(|| format!("write {}: {:02x?}", out.len(), &out[..out.len().min(96)]));
        self.w.write_all(&out).await?;
        self.w.flush().await?;
        Ok(())
    }
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>>;

/// The client end: `Call(method, args, &reply)`.
pub struct Client<S> {
    w: tokio::sync::Mutex<GobWriter<WriteHalf<S>>>,
    pending: Pending,
    seq: AtomicU64,
}

impl<S: AsyncRead + AsyncWrite + Send + 'static> Client<S> {
    pub fn new(stream: S) -> Arc<Self> {
        let (r, w) = tokio::io::split(stream);
        let pending: Pending = Arc::default();
        tokio::spawn(read_replies(GobReader::new(r), pending.clone()));
        Arc::new(Client { w: tokio::sync::Mutex::new(GobWriter::new(w)), pending, seq: AtomicU64::new(0) })
    }

    /// One call; its reply as decoded, or the error string the server answered with.
    pub async fn call(&self, method: &str, arg_ty: &Ty, arg: &Value) -> Result<Value> {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).insert(seq, tx);
        let header = Value::strukt(vec![("ServiceMethod", Value::String(method.into())), ("Seq", Value::Uint(seq))]);
        let sent = self.w.lock().await.send(&[(&REQUEST, &header), (arg_ty, arg)]).await;
        if let Err(e) = sent {
            self.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&seq);
            return Err(e.context(format!("rpc: sending {method}")));
        }
        rx.await.map_err(|_| anyhow!("rpc: connection closed during {method}"))?
    }
}

async fn read_replies<R: AsyncRead + Unpin>(mut r: GobReader<R>, pending: Pending) {
    let err = loop {
        let header = match r.next().await {
            Ok(h) => h,
            Err(e) => break e,
        };
        let body = match r.next().await {
            Ok(b) => b,
            Err(e) => break e,
        };
        // gob leaves out zero fields: no Seq is sequence 0.
        let seq = header.get("Seq").and_then(Value::as_i64).unwrap_or(0);
        let error = header.get("Error").and_then(Value::as_str).unwrap_or("");
        let reply = if error.is_empty() { Ok(body) } else { Err(anyhow!("{error}")) };
        let tx = u64::try_from(seq).ok().and_then(|s| pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&s));
        if let Some(tx) = tx {
            let _ = tx.send(reply);
        }
    };
    for (_, tx) in pending.lock().unwrap_or_else(|e| e.into_inner()).drain() {
        let _ = tx.send(Err(anyhow!("rpc: {err}")));
    }
}

/// A future returned by a request handler: the reply's type and value, or the error to answer with.
pub type Reply = Pin<Box<dyn Future<Output = Result<(Ty, Value)>> + Send>>;

/// The server end: each request is handed to `handler` (method, argument) as it arrives; replies are written as
/// handlers finish, in any order. Returns when the other side closes the connection.
pub async fn serve<S, H>(stream: S, handler: H) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Send + 'static,
    H: Fn(String, Value) -> Reply + Send + Sync + 'static,
{
    let (r, w) = tokio::io::split(stream);
    let mut r = GobReader::new(r);
    let w = Arc::new(tokio::sync::Mutex::new(GobWriter::new(w)));
    let handler = Arc::new(handler);
    loop {
        let header = match r.next().await {
            Ok(h) => h,
            Err(e) if is_eof(&e) => return Ok(()),
            Err(e) => return Err(e),
        };
        let body = r.next().await?;
        let method = header.get("ServiceMethod").and_then(Value::as_str).unwrap_or("").to_string();
        let seq = header.get("Seq").and_then(Value::as_i64).unwrap_or(0);
        let (w, handler) = (w.clone(), handler.clone());
        tokio::spawn(async move {
            let reply = handler(method.clone(), body).await;
            let (error, ty, value) = match reply {
                Ok((ty, value)) => (String::new(), ty, value),
                Err(e) => (format!("{e:#}"), EMPTY.clone(), empty()),
            };
            let header = Value::strukt(vec![
                ("ServiceMethod", Value::String(method)),
                ("Seq", Value::Uint(seq as u64)),
                ("Error", Value::String(error)),
            ]);
            let _ = w.lock().await.send(&[(&RESPONSE, &header), (&ty, &value)]).await;
        });
    }
}

fn is_eof(e: &anyhow::Error) -> bool {
    e.downcast_ref::<std::io::Error>().is_some_and(|e| e.kind() == std::io::ErrorKind::UnexpectedEof)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn call_and_serve_over_a_pipe() {
        let (a, b) = tokio::io::duplex(1 << 16);
        tokio::spawn(serve(b, |method, arg| {
            Box::pin(async move {
                match method.as_str() {
                    "Plugin.Echo" => Ok((Ty::String, arg)),
                    _ => Err(anyhow!("rpc: can't find method {method}")),
                }
            })
        }));
        let c = Client::new(a);
        let (one, two) = (Value::String("one".into()), Value::String("two".into()));
        let (x, y) = tokio::join!(c.call("Plugin.Echo", &Ty::String, &one), c.call("Plugin.Echo", &Ty::String, &two));
        assert_eq!(x.unwrap(), Value::String("one".into()));
        assert_eq!(y.unwrap(), Value::String("two".into()));
        let e = c.call("Plugin.Nope", &EMPTY, &empty()).await.unwrap_err();
        assert!(e.to_string().contains("can't find method"), "{e}");
    }
}

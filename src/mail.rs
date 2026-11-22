//! Sending one email over SMTP, as Go's mail package does: the SMTP session of `net/smtp` (STARTTLS or TLS,
//! PLAIN or LOGIN auth), and the message `go-mail` writes (headers in its order, a text/plain part made from the
//! HTML by `html2text`, the HTML part, inline images).

// Based on Mattermost server/platform/shared/mail/mail.go, Copyright (c) 2015-present Mattermost, Inc. Licensed
// under AGPL-3.0; the message layout of github.com/wneessen/go-mail (MIT) and the text of
// github.com/jaytaylor/html2text (MIT) as Mattermost uses them.

use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

/// `mail.SMTPConfig`.
#[derive(Clone, Debug, Default)]
pub struct SmtpConfig {
    pub connection_security: String,
    pub skip_cert_verification: bool,
    pub hostname: String,
    pub server_name: String,
    pub server: String,
    pub port: String,
    pub timeout_secs: u64,
    pub username: String,
    pub password: String,
    pub auth: bool,
    pub feedback_name: String,
    pub feedback_email: String,
    pub reply_to: String,
}

impl SmtpConfig {
    /// `MailServiceConfig` / `mailServiceConfig(replyTo)` from `EmailSettings`.
    pub fn from_config(e: &serde_json::Value, site_url: &str, reply_to: &str) -> SmtpConfig {
        let s = |k: &str| e[k].as_str().unwrap_or("").to_string();
        SmtpConfig {
            connection_security: s("ConnectionSecurity"),
            skip_cert_verification: e["SkipServerCertificateVerification"].as_bool().unwrap_or(false),
            hostname: hostname_of(site_url),
            server_name: s("SMTPServer"),
            server: s("SMTPServer"),
            port: s("SMTPPort"),
            timeout_secs: e["SMTPServerTimeout"].as_u64().unwrap_or(10),
            username: s("SMTPUsername"),
            password: s("SMTPPassword"),
            auth: e["EnableSMTPAuth"].as_bool().unwrap_or(false),
            feedback_name: s("FeedbackName"),
            feedback_email: s("FeedbackEmail"),
            reply_to: if reply_to.is_empty() { s("ReplyToAddress") } else { reply_to.to_string() },
        }
    }
}

/// `utils.GetHostnameFromSiteURL`.
pub fn hostname_of(site_url: &str) -> String {
    let rest = site_url.split_once("://").map(|(_, r)| r).unwrap_or("");
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = host.rsplit_once('@').map(|(_, h)| h).unwrap_or(host);
    if let Some(v6) = host.strip_prefix('[') {
        return v6.split(']').next().unwrap_or("").to_string();
    }
    host.split(':').next().unwrap_or("").to_string()
}

/// One message: `mailData`.
#[derive(Default)]
pub struct Mail {
    pub to: String,
    pub subject: String,
    pub html: String,
    pub cc: String,
    /// Inline files by name (`user-avatar.png`).
    pub embedded: Vec<(String, Vec<u8>)>,
    pub message_id: String,
    pub in_reply_to: String,
    pub references: String,
    pub category: String,
}

trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}

struct Conn {
    io: BufReader<Box<dyn Stream>>,
    /// The stream really is TLS (`Client.tls` of `net/smtp`): a TLS connection, or a STARTTLS that went through.
    /// What the auth mechanisms check, never the configured security.
    tls: bool,
}

/// `net/smtp`'s `validateLine`: a command may not carry CR or LF, so no value can smuggle in another command.
const BAD_LINE: &str = "smtp: A line must not contain CR or LF";

impl Conn {
    /// A reply: its code and its lines (`textproto.ReadResponse`).
    async fn reply(&mut self) -> Result<(u16, Vec<String>), String> {
        let mut lines = vec![];
        loop {
            let mut l = String::new();
            let n = self.io.read_line(&mut l).await.map_err(|e| e.to_string())?;
            if n == 0 {
                return Err("EOF".into());
            }
            let l = l.trim_end_matches(['\r', '\n']).to_string();
            if l.len() < 3 {
                return Err(format!("short response: {l}"));
            }
            let code: u16 = l[..3].parse().map_err(|_| format!("invalid response code: {l}"))?;
            let more = l.as_bytes().get(3) == Some(&b'-');
            lines.push(l.get(4..).unwrap_or("").to_string());
            if !more {
                return Ok((code, lines));
            }
        }
    }
    async fn cmd(&mut self, expect: u16, line: &str) -> Result<(u16, Vec<String>), String> {
        if line.contains(['\r', '\n']) {
            return Err(BAD_LINE.into());
        }
        self.io.get_mut().write_all(format!("{line}\r\n").as_bytes()).await.map_err(|e| e.to_string())?;
        let (code, msg) = self.reply().await?;
        if code / 100 != expect / 100 && code != expect {
            return Err(format!("{code} {}", msg.join("\n")));
        }
        Ok((code, msg))
    }
}

/// A TLS client config: the platform's roots, or any certificate (`InsecureSkipVerify`).
fn tls_config(skip: bool) -> Result<Arc<tokio_rustls::rustls::ClientConfig>, String> {
    use rustls_platform_verifier::ConfigVerifierExt;
    use tokio_rustls::rustls;
    if !skip {
        return rustls::ClientConfig::with_platform_verifier().map(Arc::new).map_err(|e| e.to_string());
    }
    #[derive(Debug)]
    struct Any(Arc<rustls::crypto::CryptoProvider>);
    impl rustls::client::danger::ServerCertVerifier for Any {
        fn verify_server_cert(
            &self,
            _: &rustls::pki_types::CertificateDer<'_>,
            _: &[rustls::pki_types::CertificateDer<'_>],
            _: &rustls::pki_types::ServerName<'_>,
            _: &[u8],
            _: rustls::pki_types::UnixTime,
        ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        }
        fn verify_tls12_signature(
            &self,
            m: &[u8],
            c: &rustls::pki_types::CertificateDer<'_>,
            d: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls12_signature(m, c, d, &self.0.signature_verification_algorithms)
        }
        fn verify_tls13_signature(
            &self,
            m: &[u8],
            c: &rustls::pki_types::CertificateDer<'_>,
            d: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls13_signature(m, c, d, &self.0.signature_verification_algorithms)
        }
        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            self.0.signature_verification_algorithms.supported_schemes()
        }
    }
    let provider = rustls::crypto::CryptoProvider::get_default()
        .cloned()
        .unwrap_or_else(|| Arc::new(rustls::crypto::aws_lc_rs::default_provider()));
    let cfg = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(Any(provider)))
        .with_no_client_auth();
    Ok(Arc::new(cfg))
}

async fn tls_wrap(tcp: Box<dyn Stream>, cfg: &SmtpConfig) -> Result<Box<dyn Stream>, String> {
    let connector = tokio_rustls::TlsConnector::from(tls_config(cfg.skip_cert_verification)?);
    let name =
        tokio_rustls::rustls::pki_types::ServerName::try_from(cfg.server_name.clone()).map_err(|e| e.to_string())?;
    let s = connector.connect(name, tcp).await.map_err(|e| e.to_string())?;
    Ok(Box::new(s))
}

/// An SMTP session: `ConnectToSMTPServer` and `NewSMTPClient`, then what `net/smtp` does lazily (`hello`). Returns
/// the session and the server's extensions.
async fn connect(cfg: &SmtpConfig) -> Result<(Conn, Vec<String>), String> {
    let addr = format!("{}:{}", cfg.server, cfg.port);
    let timeout = Duration::from_secs(cfg.timeout_secs.max(1));
    let tcp = tokio::time::timeout(timeout, TcpStream::connect(&addr))
        .await
        .map_err(|_| format!("unable to connect to the SMTP server: dial tcp {addr}: i/o timeout"))?
        .map_err(|e| format!("unable to connect to the SMTP server: dial tcp {addr}: {}", go_net_error(&e)))?;
    let mut stream: Box<dyn Stream> = Box::new(tcp);
    let tls = cfg.connection_security == "TLS";
    if tls {
        stream = tls_wrap(stream, cfg)
            .await
            .map_err(|e| format!("unable to connect to the SMTP server through TLS: {e}"))?;
    }
    let mut c = Conn { io: BufReader::new(stream), tls };
    let (code, msg) = c.reply().await.map_err(|e| format!("unable to connect to the SMTP server: {e}"))?;
    if code != 220 {
        return Err(format!("unable to connect to the SMTP server: {code} {}", msg.join("\n")));
    }
    let local = if cfg.hostname.is_empty() { "localhost".to_string() } else { cfg.hostname.clone() };
    let mut ext = ehlo(&mut c, &local).await?;
    if cfg.connection_security == "STARTTLS" {
        // `Client.StartTLS`, sent whether the server offers it or not. Go ignores its error, so a server that
        // refuses STARTTLS (or a man in the middle that strips it) leaves the session in plain text: `tls` stays
        // false, and the auth below refuses to send the credentials.
        if c.cmd(220, "STARTTLS").await.is_ok() {
            let plain = c.io.into_inner();
            let tls = tls_wrap(plain, cfg).await?;
            c = Conn { io: BufReader::new(tls), tls: true };
            ext = ehlo(&mut c, &local).await?;
        }
    }
    if cfg.auth {
        auth(&mut c, &ext, cfg).await.map_err(|e| format!("authentication failed: {e}"))?;
    }
    Ok((c, ext))
}

/// `dial tcp` errors in Go's words.
fn go_net_error(e: &std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::ConnectionRefused => "connect: connection refused".into(),
        _ => e.to_string(),
    }
}

async fn ehlo(c: &mut Conn, local: &str) -> Result<Vec<String>, String> {
    match c.cmd(250, &format!("EHLO {local}")).await {
        Ok((_, lines)) => Ok(lines.into_iter().skip(1).collect()),
        Err(_) => {
            c.cmd(250, &format!("HELO {local}")).await?;
            Ok(vec![])
        }
    }
}

/// `authChooser`: PLAIN when offered, LOGIN otherwise. Both refuse a session that is not really TLS
/// (`plainAuth.Start`, Mattermost's `loginAuth.Start`) with "unencrypted connection", whatever the configured
/// security says. `net/smtp` spares PLAIN on localhost, but Go names the server `host:port` there, which is never
/// "localhost": no server is spared.
async fn auth(c: &mut Conn, ext: &[String], cfg: &SmtpConfig) -> Result<(), String> {
    let mechs: Vec<String> = ext
        .iter()
        .find_map(|e| e.strip_prefix("AUTH ").or_else(|| e.strip_prefix("auth ")))
        .map(|m| m.split_whitespace().map(str::to_uppercase).collect())
        .unwrap_or_default();
    let b64 = |s: &str| base64::engine::general_purpose::STANDARD.encode(s);
    if !c.tls {
        // `Client.Auth` says QUIT when the mechanism refuses to start.
        let _ = c.cmd(221, "QUIT").await;
        return Err("unencrypted connection".into());
    }
    if mechs.iter().any(|m| m == "PLAIN") {
        c.cmd(235, &format!("AUTH PLAIN {}", b64(&format!("\0{}\0{}", cfg.username, cfg.password)))).await?;
        return Ok(());
    }
    c.cmd(334, "AUTH LOGIN").await?;
    c.cmd(334, &b64(&cfg.username)).await?;
    c.cmd(235, &b64(&cfg.password)).await?;
    Ok(())
}

/// `TestConnection`.
pub async fn test_connection(cfg: &SmtpConfig) -> Result<(), String> {
    connect(cfg).await.map(|_| ()).map_err(|e| format!("unable to connect: {e}"))
}

/// `SendMailUsingConfig` / `SendMailWithEmbeddedFilesUsingConfig`. No server: nothing is sent, no error.
pub async fn send(m: &Mail, cfg: &SmtpConfig) -> Result<(), String> {
    if cfg.server.is_empty() {
        return Ok(());
    }
    let timeout = Duration::from_secs(cfg.timeout_secs.max(1));
    let (mut c, ext) = tokio::time::timeout(timeout, connect(cfg))
        .await
        .map_err(|_| "unable to connect to the SMTP server: context deadline exceeded".to_string())??;
    eprintln!("sending mail to={}", m.to);
    let to = single_address(&m.to).map_err(|e| format!("invalid To header: {e}"))?;
    if !m.cc.is_empty() {
        single_address(&m.cc).map_err(|e| format!("invalid Cc header: {e}"))?;
    }
    let msg = build(m, cfg, chrono::Utc::now());
    let has = |x: &str| ext.iter().any(|e| e.split_whitespace().next().is_some_and(|k| k.eq_ignore_ascii_case(x)));
    let mut mail_from = format!("MAIL FROM:<{}>", cfg.feedback_email);
    if has("8BITMIME") {
        mail_from.push_str(" BODY=8BITMIME");
    }
    if has("SMTPUTF8") {
        mail_from.push_str(" SMTPUTF8");
    }
    c.cmd(250, &mail_from).await.map_err(|e| format!("failed to set the from address: {e}"))?;
    // Go gives `Rcpt` the value as it came, so `validateLine` refuses CR or LF anywhere in it (a parenthesised
    // comment may hold them and still parse); the command carries the parsed address.
    if m.to.contains(['\r', '\n']) {
        return Err(format!("failed to set the to address: {BAD_LINE}"));
    }
    c.cmd(250, &format!("RCPT TO:<{to}>")).await.map_err(|e| format!("failed to set the to address: {e}"))?;
    c.cmd(354, "DATA").await.map_err(|e| format!("failed to add email message data: {e}"))?;
    let mut data = String::with_capacity(msg.len() + 64);
    for line in msg.split_inclusive('\n') {
        let line = line.strip_suffix('\n').map(|l| l.strip_suffix('\r').unwrap_or(l));
        let (text, nl) = match line {
            Some(l) => (l, true),
            None => (msg.rsplit('\n').next().unwrap_or(""), false),
        };
        if text.starts_with('.') {
            data.push('.');
        }
        data.push_str(text);
        if nl {
            data.push_str("\r\n");
        }
    }
    if !data.ends_with("\r\n") {
        data.push_str("\r\n");
    }
    data.push_str(".\r\n");
    let w = c.io.get_mut();
    w.write_all(data.as_bytes()).await.map_err(|e| format!("failed to write the email message: {e}"))?;
    let (code, msg) = c.reply().await.map_err(|e| format!("failed to close connection to the SMTP server: {e}"))?;
    if code != 250 {
        return Err(format!("failed to close connection to the SMTP server: {code} {}", msg.join("\n")));
    }
    // Go closes the connection without QUIT (`defer c.Quit(); defer c.Close()`).
    Ok(())
}

/// `validateSingleAddress`: exactly one address by `mail.ParseAddressList`, its addr-spec.
fn single_address(v: &str) -> Result<String, String> {
    let list = parse_address_list(v).map_err(|e| format!("failed to parse address: {e}"))?;
    match <[_; 1]>::try_from(list) {
        Ok([(_, addr)]) => Ok(addr),
        Err(list) => Err(format!("must contain exactly one address, got {}", list.len())),
    }
}

// ---------- net/mail address lists ----------

// Based on Go's net/mail (message.go, addrParser), Copyright 2011 The Go Authors, BSD-style license.

/// `mail.ParseAddressList`: (display name, addr-spec) pairs, or Go's error.
pub fn parse_address_list(list: &str) -> Result<Vec<(String, String)>, String> {
    AddrParser { s: list.as_bytes() }.parse_address_list()
}

struct AddrParser<'a> {
    s: &'a [u8],
}

/// `utf8.DecodeRune`: the first rune and its size; `None` for invalid bytes (size 1) or nothing (size 0).
fn decode_rune(s: &[u8]) -> (Option<char>, usize) {
    let n = match s.first() {
        None => return (None, 0),
        Some(0..=0x7f) => 1,
        Some(0xc0..=0xdf) => 2,
        Some(0xe0..=0xef) => 3,
        Some(0xf0..=0xf7) => 4,
        Some(_) => return (None, 1),
    };
    match s.get(..n).map(std::str::from_utf8) {
        Some(Ok(t)) => (t.chars().next(), n),
        _ => (None, 1),
    }
}

fn is_vchar(r: char) -> bool {
    ('!'..='~').contains(&r) || r as u32 >= 0x80
}
fn is_wsp(r: char) -> bool {
    r == ' ' || r == '\t'
}
fn is_atext(r: char, dot: bool) -> bool {
    match r {
        '.' => dot,
        '(' | ')' | '<' | '>' | '[' | ']' | ':' | ';' | '@' | '\\' | ',' | '"' => false,
        _ => is_vchar(r),
    }
}
fn is_qtext(r: char) -> bool {
    r != '\\' && r != '"' && is_vchar(r)
}
fn is_dtext(r: char) -> bool {
    r != '[' && r != ']' && r != '\\' && is_vchar(r)
}

/// Go's `%q` of a string, close enough for error texts.
fn go_quote(s: &[u8]) -> String {
    format!("{:?}", String::from_utf8_lossy(s))
}

impl AddrParser<'_> {
    fn empty(&self) -> bool {
        self.s.is_empty()
    }
    fn peek(&self) -> u8 {
        self.s[0]
    }
    fn consume(&mut self, c: u8) -> bool {
        if self.empty() || self.peek() != c {
            return false;
        }
        self.s = &self.s[1..];
        true
    }
    fn skip_space(&mut self) {
        while let Some(b' ' | b'\t') = self.s.first() {
            self.s = &self.s[1..];
        }
    }

    fn parse_address_list(&mut self) -> Result<Vec<(String, String)>, String> {
        let mut list = vec![];
        loop {
            self.skip_space();
            if self.consume(b',') {
                continue;
            }
            list.extend(self.parse_address(true)?);
            if !self.skip_cfws() {
                return Err("mail: misformatted parenthetical comment".into());
            }
            if self.empty() {
                break;
            }
            if self.peek() != b',' {
                return Err("mail: expected comma".into());
            }
            while self.consume(b',') {
                self.skip_space();
            }
            if self.empty() {
                break;
            }
        }
        Ok(list)
    }

    fn parse_address(&mut self, handle_group: bool) -> Result<Vec<(String, String)>, String> {
        self.skip_space();
        if self.empty() {
            return Err("mail: no address".into());
        }
        // addr-spec, then an optional display name in a comment.
        if let Ok(spec) = self.consume_addr_spec() {
            self.skip_space();
            let mut name = String::new();
            if !self.empty() && self.peek() == b'(' {
                name = self.consume_display_name_comment()?;
            }
            return Ok(vec![(name, spec)]);
        }
        let mut name = String::new();
        if self.peek() != b'<' {
            name = self.consume_phrase()?;
        }
        self.skip_space();
        if handle_group && self.consume(b':') {
            return self.consume_group_list();
        }
        if !self.consume(b'<') {
            if name.chars().all(|r| is_atext(r, true)) {
                return Err("mail: missing '@' or angle-addr".into());
            }
            return Err("mail: no angle-addr".into());
        }
        let spec = self.consume_addr_spec()?;
        if !self.consume(b'>') {
            return Err("mail: unclosed angle-addr".into());
        }
        Ok(vec![(name, spec)])
    }

    fn consume_group_list(&mut self) -> Result<Vec<(String, String)>, String> {
        let mut group = vec![];
        self.skip_space();
        if self.consume(b';') {
            if !self.skip_cfws() {
                return Err("mail: misformatted parenthetical comment".into());
            }
            return Ok(group);
        }
        loop {
            self.skip_space();
            group.extend(self.parse_address(false)?);
            if !self.skip_cfws() {
                return Err("mail: misformatted parenthetical comment".into());
            }
            if self.consume(b';') {
                if !self.skip_cfws() {
                    return Err("mail: misformatted parenthetical comment".into());
                }
                break;
            }
            if !self.consume(b',') {
                return Err("mail: expected comma".into());
            }
        }
        Ok(group)
    }

    /// An addr-spec; on error the parser is where it was.
    fn consume_addr_spec(&mut self) -> Result<String, String> {
        let orig = self.s;
        let r = self.addr_spec();
        if r.is_err() {
            self.s = orig;
        }
        r
    }
    fn addr_spec(&mut self) -> Result<String, String> {
        self.skip_space();
        if self.empty() {
            return Err("mail: no addr-spec".into());
        }
        let local = if self.peek() == b'"' {
            // Go replaces any quoted-string error by this one.
            match self.consume_quoted_string() {
                Ok(q) if !q.is_empty() => q,
                _ => return Err("mail: empty quoted string in addr-spec".into()),
            }
        } else {
            self.consume_atom(true, false)?
        };
        if !self.consume(b'@') {
            return Err("mail: missing @ in addr-spec".into());
        }
        self.skip_space();
        if self.empty() {
            return Err("mail: no domain in addr-spec".into());
        }
        let domain = if self.peek() == b'[' { self.consume_domain_literal()? } else { self.consume_atom(true, false)? };
        Ok(format!("{local}@{domain}"))
    }

    fn consume_phrase(&mut self) -> Result<String, String> {
        let mut words: Vec<String> = vec![];
        let mut encoded = String::new();
        let mut err = None;
        loop {
            if !words.is_empty() && !self.skip_cfws() {
                return Err("mail: misformatted parenthetical comment".into());
            }
            self.skip_space();
            if self.empty() {
                break;
            }
            let r = if self.peek() == b'"' {
                self.consume_quoted_string().map(|w| (w, false))
            } else {
                self.consume_atom(true, true).and_then(|w| decode_rfc2047_word(&w))
            };
            match r {
                Err(e) => {
                    err = Some(e);
                    break;
                }
                Ok((w, true)) => encoded.push_str(&w),
                Ok((w, false)) => {
                    if !encoded.is_empty() {
                        words.push(std::mem::take(&mut encoded));
                    }
                    words.push(w);
                }
            }
        }
        if !encoded.is_empty() {
            words.push(encoded);
        }
        if let Some(e) = err
            && words.is_empty()
        {
            return Err(format!("mail: missing word in phrase: {e}"));
        }
        Ok(words.join(" "))
    }

    fn consume_quoted_string(&mut self) -> Result<String, String> {
        let mut i = 1;
        let mut out = String::new();
        let mut escaped = false;
        loop {
            let (r, size) = decode_rune(&self.s[i..]);
            let Some(r) = r else {
                if size == 0 {
                    return Err("mail: unclosed quoted-string".into());
                }
                return Err(format!("mail: invalid utf-8 in quoted-string: {}", go_quote(self.s)));
            };
            if escaped {
                if !is_vchar(r) && !is_wsp(r) {
                    return Err(format!("mail: bad character in quoted-string: {r:?}"));
                }
                out.push(r);
                escaped = false;
            } else if is_qtext(r) || is_wsp(r) {
                out.push(r);
            } else if r == '"' {
                break;
            } else if r == '\\' {
                escaped = true;
            } else {
                return Err(format!("mail: bad character in quoted-string: {r:?}"));
            }
            i += size;
        }
        self.s = &self.s[i + 1..];
        Ok(out)
    }

    fn consume_atom(&mut self, dot: bool, permissive: bool) -> Result<String, String> {
        let mut i = 0;
        loop {
            match decode_rune(&self.s[i..]) {
                (None, 1) => return Err(format!("mail: invalid utf-8 in address: {}", go_quote(self.s))),
                (Some(r), size) if is_atext(r, dot) => i += size,
                _ => break,
            }
        }
        if i == 0 {
            return Err("mail: invalid string".into());
        }
        let atom = String::from_utf8_lossy(&self.s[..i]).into_owned();
        self.s = &self.s[i..];
        if !permissive {
            if atom.starts_with('.') {
                return Err("mail: leading dot in atom".into());
            }
            if atom.contains("..") {
                return Err("mail: double dot in atom".into());
            }
            if atom.ends_with('.') {
                return Err("mail: trailing dot in atom".into());
            }
        }
        Ok(atom)
    }

    fn consume_domain_literal(&mut self) -> Result<String, String> {
        if !self.consume(b'[') {
            return Err("mail: missing \"[\" in domain-literal".into());
        }
        let start = self.s;
        let mut len = 0;
        loop {
            if self.empty() {
                return Err("mail: unclosed domain-literal".into());
            }
            if self.peek() == b']' {
                break;
            }
            let (r, size) = decode_rune(self.s);
            let Some(r) = r else {
                return Err(format!("mail: invalid utf-8 in domain-literal: {}", go_quote(self.s)));
            };
            if !is_dtext(r) {
                return Err(format!("mail: bad character in domain-literal: {r:?}"));
            }
            len += size;
            self.s = &self.s[size..];
        }
        let dtext = String::from_utf8_lossy(&start[..len]).into_owned();
        if !self.consume(b']') {
            return Err("mail: unclosed domain-literal".into());
        }
        if dtext.parse::<std::net::IpAddr>().is_err() {
            return Err(format!("mail: invalid IP address in domain-literal: {dtext:?}"));
        }
        Ok(format!("[{dtext}]"))
    }

    fn consume_display_name_comment(&mut self) -> Result<String, String> {
        if !self.consume(b'(') {
            return Err("mail: comment does not start with (".into());
        }
        let Some(comment) = self.consume_comment() else {
            return Err("mail: misformatted parenthetical comment".into());
        };
        let mut words = vec![];
        for w in comment.split([' ', '\t']).filter(|w| !w.is_empty()) {
            words.push(decode_rfc2047_word(w)?.0);
        }
        Ok(words.join(" "))
    }

    fn skip_cfws(&mut self) -> bool {
        self.skip_space();
        while self.consume(b'(') {
            if self.consume_comment().is_none() {
                return false;
            }
            self.skip_space();
        }
        true
    }

    /// A comment after its `(`, nested ones included; `None` when it does not close.
    fn consume_comment(&mut self) -> Option<String> {
        let mut depth = 1;
        let mut out = vec![];
        while !self.empty() && depth != 0 {
            if self.peek() == b'\\' && self.s.len() > 1 {
                self.s = &self.s[1..];
            } else if self.peek() == b'(' {
                depth += 1;
            } else if self.peek() == b')' {
                depth -= 1;
            }
            if depth > 0 {
                out.push(self.s[0]);
            }
            self.s = &self.s[1..];
        }
        (depth == 0).then(|| String::from_utf8_lossy(&out).into_owned())
    }
}

/// `addrParser.decodeRFC2047Word`: an encoded-word decoded (`true`), anything else as it is (`false`); an error
/// only for a charset Go cannot read (it knows UTF-8, ISO-8859-1 and US-ASCII).
fn decode_rfc2047_word(s: &str) -> Result<(String, bool), String> {
    let plain = || Ok((s.to_string(), false));
    if s.len() < 8 || !s.starts_with("=?") || !s.ends_with("?=") || s.matches('?').count() != 4 {
        return plain();
    }
    let (charset, rest) = s[2..s.len() - 2].split_once('?').unwrap_or_default();
    let (enc, text) = rest.split_once('?').unwrap_or_default();
    if charset.is_empty() || enc.len() != 1 {
        return plain();
    }
    let content = match enc {
        "b" | "B" => match base64::engine::general_purpose::STANDARD.decode(text) {
            Ok(c) => c,
            Err(_) => return plain(),
        },
        "q" | "Q" => {
            let b = text.as_bytes();
            let mut out = vec![];
            let mut i = 0;
            while i < b.len() {
                match b[i] {
                    b'_' => out.push(b' '),
                    b'=' => {
                        let hex = b.get(i + 1..i + 3).filter(|h| h.iter().all(u8::is_ascii_hexdigit));
                        let Some(v) = hex.and_then(|h| u8::from_str_radix(std::str::from_utf8(h).ok()?, 16).ok())
                        else {
                            return plain();
                        };
                        out.push(v);
                        i += 2;
                    }
                    c if (b' '..=b'~').contains(&c) || c == b'\n' || c == b'\r' || c == b'\t' => out.push(c),
                    _ => return plain(),
                }
                i += 1;
            }
            out
        }
        _ => return plain(),
    };
    let word = match charset.to_ascii_lowercase().as_str() {
        "utf-8" => String::from_utf8_lossy(&content).into_owned(),
        "iso-8859-1" => content.iter().map(|&b| b as char).collect(),
        "us-ascii" => content.iter().map(|&b| if b < 0x80 { b as char } else { '\u{FFFD}' }).collect(),
        _ => return Err(format!("charset not supported: {charset:?}")),
    };
    Ok((word, true))
}

/// `mail.Address.String()` of a name and address.
fn address(name: &str, addr: &str) -> String {
    if name.is_empty() {
        return format!("<{addr}>");
    }
    if name.is_ascii() {
        let plain = name.bytes().all(|b| b.is_ascii_alphanumeric() || b" !#$%&'*+-/=?^_`{|}~".contains(&b));
        if plain {
            return format!("{name} <{addr}>");
        }
        let quoted = name.replace('\\', "\\\\").replace('"', "\\\"");
        return format!("\"{quoted}\" <{addr}>");
    }
    format!("{} <{addr}>", word_encode(name))
}

/// `mime.QEncoding.Encode("UTF-8", s)`.
pub fn word_encode(s: &str) -> String {
    let needs = s.bytes().any(|b| !(b' '..=b'~').contains(&b) && b != b'\t');
    if !needs {
        return s.to_string();
    }
    const MAX: usize = 75 - "=?UTF-8?q?".len() - "?=".len();
    let mut words: Vec<String> = vec![];
    let mut cur = String::new();
    for ch in s.chars() {
        let mut enc = String::new();
        let mut buf = [0u8; 4];
        for &b in ch.encode_utf8(&mut buf).as_bytes() {
            if b == b' ' {
                enc.push('_');
            } else if (b'!'..=b'~').contains(&b) && b != b'=' && b != b'?' && b != b'_' {
                enc.push(b as char);
            } else {
                enc.push_str(&format!("={b:02X}"));
            }
        }
        if cur.len() + enc.len() > MAX {
            words.push(std::mem::take(&mut cur));
        }
        cur.push_str(&enc);
    }
    words.push(cur);
    words.iter().map(|w| format!("=?UTF-8?q?{w}?=")).collect::<Vec<_>>().join(" ")
}

/// `msgWriter.writeHeader`: folded at spaces before 76 characters.
fn header(out: &mut String, key: &str, value: &str) {
    let mut b = String::new();
    let mut left: i64 = 76 - 2 - key.len() as i64 - 2;
    b.push_str(key);
    b.push_str(": ");
    let words: Vec<&str> = value.split(' ').collect();
    for (i, w) in words.iter().enumerate() {
        if left - (w.len() as i64) <= 1 {
            b.push_str("\r\n ");
            left = 76 - 3;
        }
        b.push_str(w);
        if i < words.len() - 1 {
            b.push(' ');
            left -= 1;
        }
        left -= w.len() as i64;
    }
    out.push_str(&b.replace(" \r\n", "\r\n"));
    out.push_str("\r\n");
}

/// `mime/quotedprintable.Writer` (text mode: line ends become CRLF).
pub fn quoted_printable(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + s.len() / 8);
    let mut line = String::new();
    let flush_soft = |out: &mut String, line: &mut String| {
        out.push_str(line);
        out.push_str("=\r\n");
        line.clear();
    };
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\r' && bytes.get(i + 1) == Some(&b'\n') {
            i += 1;
            continue;
        }
        if b == b'\n' || b == b'\r' {
            // Trailing whitespace before a line end is encoded.
            if let Some(last) = line.pop() {
                if last == ' ' || last == '\t' {
                    let enc = format!("={:02X}", last as u8);
                    if line.len() + enc.len() > 75 {
                        flush_soft(&mut out, &mut line);
                    }
                    line.push_str(&enc);
                } else {
                    line.push(last);
                }
            }
            out.push_str(&line);
            out.push_str("\r\n");
            line.clear();
            i += 1;
            continue;
        }
        let enc = if (b'!'..=b'~').contains(&b) && b != b'=' || b == b' ' || b == b'\t' {
            (b as char).to_string()
        } else {
            format!("={b:02X}")
        };
        if line.len() + enc.len() > 75 {
            flush_soft(&mut out, &mut line);
        }
        line.push_str(&enc);
        i += 1;
    }
    if let Some(last) = line.pop() {
        if last == ' ' || last == '\t' {
            let enc = format!("={:02X}", last as u8);
            if line.len() + enc.len() > 75 {
                flush_soft(&mut out, &mut line);
            }
            line.push_str(&enc);
        } else {
            line.push(last);
        }
    }
    out.push_str(&line);
    out
}

fn boundary() -> String {
    let b: [u8; 30] = rand::random();
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// The message as go-mail writes it.
pub fn build(m: &Mail, cfg: &SmtpConfig, date: chrono::DateTime<chrono::Utc>) -> String {
    let text = html_to_text(&m.html);
    let mut gen_h: Vec<(&str, String)> = vec![
        ("Subject", word_encode(&m.subject)),
        ("Content-Transfer-Encoding", "8bit".into()),
        ("Auto-Submitted", "auto-generated".into()),
        ("Precedence", "bulk".into()),
    ];
    if !m.category.is_empty() {
        gen_h.push(("X-SMTPAPI", format!("{{\"category\": {}}}", serde_json::to_string(&m.category).unwrap())));
    }
    let msg_id = if m.message_id.is_empty() {
        format!("<{}-{}@{}>", &crate::app::new_id()[..16], date.timestamp(), cfg.hostname)
    } else {
        m.message_id.clone()
    };
    gen_h.push(("Message-ID", msg_id));
    gen_h.push(("Date", date.with_timezone(&chrono::Local).format("%a, %d %b %Y %H:%M:%S %z").to_string()));
    gen_h.push(("MIME-Version", "1.0".into()));
    gen_h.push(("User-Agent", "go-mail v0.8.1 // https://github.com/wneessen/go-mail".into()));
    gen_h.push(("X-Mailer", "go-mail v0.8.1 // https://github.com/wneessen/go-mail".into()));
    gen_h.sort_by(|a, b| a.0.cmp(b.0));
    let mut out = String::new();
    for (k, v) in &gen_h {
        header(&mut out, k, v);
    }
    if !m.in_reply_to.is_empty() {
        out.push_str(&format!("In-Reply-To: {}\r\n", m.in_reply_to));
    }
    if !m.references.is_empty() {
        out.push_str(&format!("References: {}\r\n", m.references));
    }
    header(&mut out, "From", &address(&cfg.feedback_name, &cfg.feedback_email));
    header(&mut out, "To", &address("", &single_address(&m.to).unwrap_or_default()));
    if !m.cc.is_empty() {
        header(&mut out, "Cc", &address("", &single_address(&m.cc).unwrap_or_default()));
    }
    if !cfg.reply_to.is_empty() {
        header(&mut out, "Reply-To", &address(&cfg.feedback_name, &cfg.reply_to));
    }
    let alt = boundary();
    let parts = format!(
        "--{alt}\r\nContent-Transfer-Encoding: quoted-printable\r\nContent-Type: text/plain; charset=UTF-8\r\n\r\n{}\r\n--{alt}\r\nContent-Transfer-Encoding: quoted-printable\r\nContent-Type: text/html; charset=UTF-8\r\n\r\n{}\r\n--{alt}--\r\n",
        quoted_printable(&text),
        quoted_printable(&m.html)
    );
    if m.embedded.is_empty() {
        out.push_str(&format!("Content-Type: multipart/alternative;\r\n boundary={alt}\r\n\r\n"));
        out.push_str(&parts);
        return out;
    }
    let rel = boundary();
    out.push_str(&format!("Content-Type: multipart/related;\r\n boundary={rel}\r\n\r\n"));
    out.push_str(&format!("--{rel}\r\nContent-Type: multipart/alternative;\r\n boundary={alt}\r\n\r\n{parts}"));
    for (name, data) in &m.embedded {
        let mime = mime_guess::from_path(name).first_raw().unwrap_or("application/octet-stream");
        let b64 = base64::engine::general_purpose::STANDARD.encode(data);
        let mut lines = String::new();
        for chunk in b64.as_bytes().chunks(76) {
            lines.push_str(std::str::from_utf8(chunk).unwrap_or(""));
            lines.push_str("\r\n");
        }
        out.push_str(&format!(
            "\r\n--{rel}\r\nContent-Disposition: inline; filename=\"{name}\"\r\nContent-Id: <{name}>\r\nContent-Transfer-Encoding: base64\r\nContent-Type: {mime}; name=\"{name}\"\r\n\r\n{}",
            lines.trim_end_matches("\r\n")
        ));
    }
    out.push_str(&format!("\r\n--{rel}--\r\n"));
    out
}

// ---------- html2text ----------

use markup5ever_rcdom::{Handle, NodeData, RcDom};

struct Textify {
    buf: String,
    prefix: String,
    ends_with_space: bool,
    just_closed_div: bool,
    blockquote: usize,
    line_len: usize,
    pre: bool,
}

impl Textify {
    fn new() -> Textify {
        Textify {
            buf: String::new(),
            prefix: String::new(),
            ends_with_space: false,
            just_closed_div: false,
            blockquote: 0,
            line_len: 0,
            pre: false,
        }
    }

    fn emit(&mut self, data: &str) {
        if data.is_empty() {
            return;
        }
        for line in self.break_long_lines(data) {
            let first = line.chars().next().unwrap_or(' ');
            if !first.is_whitespace() && !self.ends_with_space && !data.starts_with('.') {
                self.buf.push(' ');
                self.line_len += 1;
            }
            self.ends_with_space = line.chars().last().is_some_and(char::is_whitespace);
            for c in line.chars() {
                self.buf.push(c);
                self.line_len += 1;
                if c == '\n' {
                    self.line_len = 0;
                    if !self.prefix.is_empty() {
                        let p = self.prefix.clone();
                        self.buf.push_str(&p);
                    }
                }
            }
        }
    }

    fn break_long_lines(&self, data: &str) -> Vec<String> {
        const MAX: usize = 74;
        if self.blockquote == 0 {
            return vec![data.to_string()];
        }
        let mut ret = vec![];
        let mut runes: Vec<char> = data.chars().collect();
        let mut existing = self.line_len;
        if existing >= MAX {
            ret.push("\n".to_string());
            existing = 0;
        }
        while runes.len() + existing > MAX {
            let mut i = MAX as isize - existing as isize;
            while i >= 0 && !runes.get(i as usize).is_some_and(|c| c.is_whitespace()) {
                i -= 1;
            }
            let mut i = if i == -1 {
                let mut j = MAX - existing;
                while j < runes.len() && !runes[j].is_whitespace() {
                    j += 1;
                }
                j
            } else {
                i as usize
            };
            ret.push(runes[..i.min(runes.len())].iter().collect::<String>() + "\n");
            while i < runes.len() && runes[i].is_whitespace() {
                i += 1;
            }
            runes = runes[i.min(runes.len())..].to_vec();
            existing = 0;
        }
        if !runes.is_empty() {
            ret.push(runes.iter().collect());
        }
        ret
    }

    fn children(&mut self, n: &Handle) {
        for c in n.children.borrow().iter() {
            self.traverse(c);
        }
    }

    fn traverse(&mut self, n: &Handle) {
        match &n.data {
            NodeData::Text { contents } => {
                let t = contents.borrow().to_string();
                let data = if self.pre {
                    t
                } else {
                    let mut s = String::with_capacity(t.len());
                    let mut space = false;
                    for c in t.chars() {
                        if matches!(c, ' ' | '\r' | '\n' | '\t') {
                            if !space {
                                s.push(' ');
                            }
                            space = true;
                        } else {
                            s.push(c);
                            space = false;
                        }
                    }
                    s.trim_matches(char::is_whitespace).to_string()
                };
                self.emit(&data);
            }
            NodeData::Element { name, attrs, .. } => {
                let tag = name.local.to_string();
                let attr = |k: &str| {
                    attrs.borrow().iter().find(|a| &*a.name.local == k).map(|a| a.value.to_string()).unwrap_or_default()
                };
                self.element(n, &tag, &attr);
            }
            NodeData::Document => self.children(n),
            _ => {}
        }
    }

    fn element(&mut self, n: &Handle, tag: &str, attr: &dyn Fn(&str) -> String) {
        self.just_closed_div = false;
        match tag {
            "br" => self.emit("\n"),
            "h1" | "h2" | "h3" => {
                let mut sub = Textify::new();
                sub.children(n);
                let s = sub.buf;
                let len = s.split('\n').map(|l| l.chars().count() as isize - 1).max().unwrap_or(0).max(0) as usize;
                let div = if tag == "h1" { "*" } else { "-" }.repeat(len);
                if tag == "h3" {
                    self.emit(&format!("\n\n{s}\n{div}\n\n"));
                } else {
                    self.emit(&format!("\n\n{div}\n{s}\n{div}\n\n"));
                }
            }
            "blockquote" => {
                self.blockquote += 1;
                self.prefix = ">".repeat(self.blockquote) + " ";
                self.emit("\n");
                if self.blockquote == 1 {
                    self.emit("\n");
                }
                self.children(n);
                self.blockquote -= 1;
                self.prefix = ">".repeat(self.blockquote);
                if self.blockquote > 0 {
                    self.prefix.push(' ');
                }
                self.emit("\n\n");
            }
            "div" => {
                if self.line_len > 0 {
                    self.emit("\n");
                }
                self.children(n);
                if !self.just_closed_div {
                    self.emit("\n");
                }
                self.just_closed_div = true;
            }
            "li" => {
                self.emit("* ");
                self.children(n);
                self.emit("\n");
            }
            "b" | "strong" => {
                let mut sub = Textify::new();
                sub.ends_with_space = true;
                sub.children(n);
                self.emit(&format!("*{}*", sub.buf));
            }
            "a" => {
                let kids = n.children.borrow();
                let mut link_text = String::new();
                if kids.len() == 1
                    && let NodeData::Text { contents } = &kids[0].data
                {
                    link_text = contents.borrow().to_string();
                }
                let img_alt = match kids.as_slice() {
                    [only] => match &only.data {
                        NodeData::Element { name, attrs, .. } if &*name.local == "img" => Some(
                            attrs
                                .borrow()
                                .iter()
                                .find(|a| &*a.name.local == "alt")
                                .map(|a| a.value.to_string())
                                .unwrap_or_default(),
                        ),
                        _ => None,
                    },
                    _ => None,
                };
                drop(kids);
                match img_alt {
                    Some(alt) => self.emit(&alt),
                    None => self.children(n),
                }
                let href = attr("href");
                let mut link = String::new();
                if !href.is_empty() {
                    let h = href.trim();
                    let h = h.strip_prefix("mailto:").unwrap_or(h);
                    if !h.is_empty() && link_text != h {
                        link = format!("( {h} )");
                    }
                }
                self.emit(&link);
            }
            "p" | "ul" | "table" => {
                self.emit("\n\n");
                self.children(n);
                self.emit("\n\n");
            }
            "pre" => {
                self.pre = true;
                self.children(n);
                self.pre = false;
            }
            "style" | "script" | "head" => {}
            _ => self.children(n),
        }
    }
}

/// `html2text.FromString(html)` with Mattermost's (default) options.
pub fn html_to_text(html: &str) -> String {
    use html5ever::tendril::TendrilSink;
    let dom = html5ever::parse_document(RcDom::default(), Default::default()).one(html);
    let mut t = Textify::new();
    t.traverse(&dom.document);
    let s = t.buf.replace("\n ", "\n");
    let mut out = String::with_capacity(s.len());
    let mut newlines = 0;
    for c in s.chars() {
        if c == '\n' {
            newlines += 1;
            continue;
        }
        if newlines > 0 {
            out.push_str(if newlines >= 2 { "\n\n" } else { "\n" });
            newlines = 0;
        }
        out.push(c);
    }
    if newlines > 0 {
        out.push_str(if newlines >= 2 { "\n\n" } else { "\n" });
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_of_html() {
        let t = html_to_text(
            "<html><head><style>x{}</style></head><body><h2>Hi there</h2><p>See <a href='http://a'>this</a> and <b>bold</b><br>next</p></body></html>",
        );
        assert_eq!(t, "--------\nHi there\n--------\n\nSee this ( http://a ) and *bold*\nnext");
    }

    /// Expectations printed by Go 1.26's `mail.ParseAddressList` for the same inputs.
    #[test]
    fn parses_addresses_like_go() {
        type Want = Result<&'static [(&'static str, &'static str)], &'static str>;
        let cases: &[(&str, Want)] = &[
            ("a@b.com", Ok(&[("", "a@b.com")])),
            ("", Err("mail: no address")),
            ("  ", Err("mail: no address")),
            ("a@b.com, c@d.com", Ok(&[("", "a@b.com"), ("", "c@d.com")])),
            ("a@b.com\r\nRCPT TO:<x@y.z>", Err("mail: expected comma")),
            ("a@b.com\nX", Err("mail: expected comma")),
            ("a@b.com (x\r\ny)", Ok(&[("x\r\ny", "a@b.com")])),
            ("Bob <bob@x.com>", Ok(&[("Bob", "bob@x.com")])),
            ("bob", Err("mail: missing '@' or angle-addr")),
            ("bob smith", Err("mail: no angle-addr")),
            ("\"bob\"@x.com", Ok(&[("", "bob@x.com")])),
            ("\"\"@x.com", Err("mail: missing '@' or angle-addr")),
            ("a..b@x.com", Err("mail: missing '@' or angle-addr")),
            (".a@x.com", Err("mail: missing '@' or angle-addr")),
            ("a.@x.com", Err("mail: missing '@' or angle-addr")),
            ("a@[127.0.0.1]", Ok(&[("", "a@[127.0.0.1]")])),
            ("a@[foo]", Err("mail: missing '@' or angle-addr")),
            ("a@[1.2", Err("mail: missing '@' or angle-addr")),
            ("<a@b.com", Err("mail: unclosed angle-addr")),
            ("group: a@b.com, c@d.com;", Ok(&[("", "a@b.com"), ("", "c@d.com")])),
            ("undisclosed:;", Ok(&[])),
            ("a@b.com,", Ok(&[("", "a@b.com")])),
            (",,a@b.com", Ok(&[("", "a@b.com")])),
            ("a@", Err("mail: missing '@' or angle-addr")),
            ("@b.com", Err("mail: missing word in phrase: mail: invalid string")),
            ("=?utf-8?q?J=C3=B6rg?= <j@x.com>", Ok(&[("Jörg", "j@x.com")])),
            ("=?koi8-r?q?x?= <j@x.com>", Err("mail: missing word in phrase: charset not supported: \"koi8-r\"")),
            ("a@b.com (=?koi8-r?q?x?=)", Err("charset not supported: \"koi8-r\"")),
            ("a@b.com x", Err("mail: expected comma")),
            ("\"a\rb\"@x.com", Err("mail: missing word in phrase: mail: bad character in quoted-string: '\\r'")),
            ("a@b.com (unclosed", Err("mail: misformatted parenthetical comment")),
            ("José <j@x.com>", Ok(&[("José", "j@x.com")])),
            ("a@b.com\t", Ok(&[("", "a@b.com")])),
            ("(c) a@b.com", Err("mail: missing word in phrase: mail: invalid string")),
        ];
        for (input, want) in cases {
            let got = parse_address_list(input);
            let want = want.map(|l| l.iter().map(|(n, a)| (n.to_string(), a.to_string())).collect::<Vec<_>>());
            assert_eq!(got, want.map_err(str::to_string), "{input:?}");
        }
        assert_eq!(single_address("a@b.com, c@d.com"), Err("must contain exactly one address, got 2".into()));
        assert_eq!(
            single_address("x@y.z\r\nRCPT TO:<y@z>").unwrap_err(),
            "failed to parse address: mail: expected comma"
        );
        assert_eq!(single_address("Bob <bob@x.com>"), Ok("bob@x.com".into()));
    }

    /// A fake SMTP server: answers each line from `replies` in turn and records what it got.
    async fn fake_smtp(replies: Vec<&'static str>) -> (u16, tokio::task::JoinHandle<Vec<String>>) {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        let h = tokio::spawn(async move {
            let (s, _) = l.accept().await.unwrap();
            let mut s = BufReader::new(s);
            s.get_mut().write_all(b"220 fake ESMTP\r\n").await.unwrap();
            let mut got = vec![];
            for r in replies {
                let mut line = String::new();
                if s.read_line(&mut line).await.unwrap_or(0) == 0 {
                    break;
                }
                got.push(line.trim_end().to_string());
                s.get_mut().write_all(r.as_bytes()).await.unwrap();
            }
            got
        });
        (port, h)
    }

    fn smtp_cfg(port: u16, security: &str) -> SmtpConfig {
        SmtpConfig {
            connection_security: security.into(),
            server_name: "localhost".into(),
            server: "127.0.0.1".into(),
            port: port.to_string(),
            timeout_secs: 5,
            username: "user".into(),
            password: "secret".into(),
            auth: true,
            feedback_email: "noreply@x.com".into(),
            ..Default::default()
        }
    }

    /// A server that refuses STARTTLS (or a downgrading man in the middle) never gets the credentials, by PLAIN
    /// or LOGIN, even on localhost: Go's `unencrypted connection`.
    #[tokio::test]
    async fn no_credentials_without_tls() {
        for (security, mechs) in [
            ("STARTTLS", "250-fake\r\n250 AUTH PLAIN LOGIN\r\n"),
            ("STARTTLS", "250-fake\r\n250 AUTH LOGIN\r\n"),
            ("", "250-fake\r\n250 AUTH PLAIN\r\n"),
        ] {
            let (port, h) = fake_smtp(vec![mechs, "454 TLS not available\r\n", "221 bye\r\n"]).await;
            let err = test_connection(&smtp_cfg(port, security)).await.unwrap_err();
            assert_eq!(err, "unable to connect: authentication failed: unencrypted connection");
            let got = h.await.unwrap();
            assert!(got.iter().all(|l| !l.starts_with("AUTH")), "{got:?}");
            assert_eq!(got.last().map(String::as_str), Some("QUIT"), "{got:?}");
            if security == "STARTTLS" {
                assert_eq!(got[1], "STARTTLS");
            }
        }
    }

    /// No value reaches the SMTP session with a line break in it.
    #[tokio::test]
    async fn no_command_injection() {
        let (port, h) = fake_smtp(vec!["250 OK\r\n", "250 OK\r\n", "250 OK\r\n", "250 OK\r\n"]).await;
        let mut cfg = smtp_cfg(port, "");
        cfg.auth = false;
        let m = Mail {
            to: "a@b.com (x\r\nRCPT TO:<evil@x.com>)".into(),
            subject: "s".into(),
            html: "h".into(),
            ..Default::default()
        };
        let err = send(&m, &cfg).await.unwrap_err();
        assert_eq!(err, "failed to set the to address: smtp: A line must not contain CR or LF");
        let got = h.await.unwrap();
        assert!(got.iter().all(|l| !l.contains("evil")), "{got:?}");
        let m = Mail { to: "a@b.com\r\nRCPT TO:<evil@x.com>".into(), ..Default::default() };
        let (port, _h) = fake_smtp(vec!["250 OK\r\n", "250 OK\r\n"]).await;
        cfg.port = port.to_string();
        assert_eq!(
            send(&m, &cfg).await.unwrap_err(),
            "invalid To header: failed to parse address: mail: expected comma"
        );
        cfg.feedback_email = "x@y.com>\r\nRCPT TO:<evil@x.com".into();
        let (port, h) = fake_smtp(vec!["250 OK\r\n", "250 OK\r\n"]).await;
        cfg.port = port.to_string();
        let m = Mail { to: "a@b.com".into(), ..Default::default() };
        assert_eq!(
            send(&m, &cfg).await.unwrap_err(),
            "failed to set the from address: smtp: A line must not contain CR or LF"
        );
        assert!(h.await.unwrap().iter().all(|l| !l.contains("evil")));
    }

    #[test]
    fn encodes_like_go() {
        assert_eq!(word_encode("plain"), "plain");
        assert_eq!(word_encode("héllo you"), "=?UTF-8?q?h=C3=A9llo_you?=");
        assert_eq!(quoted_printable("a=b\nc "), "a=3Db\r\nc=20");
        assert_eq!(hostname_of("http://localhost:8065/x"), "localhost");
        assert_eq!(address("", "a@b.c"), "<a@b.c>");
    }
}

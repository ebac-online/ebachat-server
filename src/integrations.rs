//! Interactive messages: message attachments and their actions, `mm_blocks_actions`, the post action endpoint,
//! interactive dialogs (open, submit), trigger ids and action cookies.

// Based on Mattermost server/public/model/message_attachment.go, integration_action.go, mm_blocks_actions.go,
// server/channels/app/integration_action.go, integration_action_setup.go and api4/integration_action.go,
// Copyright (c) 2015-present Mattermost, Inc. Licensed under AGPL-3.0.

use std::collections::BTreeMap;
use std::sync::OnceLock;
use std::time::Duration;

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use axum::extract::{Path, State as St};
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value, json};

use crate::app::{ApiResult, App, AppError, Auth, MaybeAuth, State, is_valid_id, new_id, now_ms};
use crate::hub::Broadcast;
use crate::model::{Post, Session, StringInterface, null_default};
use crate::perm;

pub fn routes() -> Router<State> {
    Router::new()
        .route("/api/v4/posts/{post_id}/actions/{action_id}", post(do_post_action))
        .route("/api/v4/actions/dialogs/open", post(open_dialog))
        .route("/api/v4/actions/dialogs/submit", post(submit_dialog))
        .route("/api/v4/actions/dialogs/lookup", post(lookup_dialog))
        .route("/api/v4/actions/dialogs/execute", post(execute_dialog_action))
}

/// `MaxIntegrationResponseSize`, `MaxDialogResponseSize`.
const MAX_RESPONSE: usize = 1024 * 1024;

/// Props kept or dropped across an action's update (`PostActionRetainPropKeys`).
const RETAIN_KEYS: [&str; 7] = [
    "from_webhook",
    "from_bot",
    "from_plugin",
    "override_username",
    "override_icon_url",
    "override_icon_emoji",
    "webhook_display_name",
];

// ---------- message attachments ----------

/// `MessageAttachment`, in Go's field order.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct MessageAttachment {
    #[serde(deserialize_with = "null_default")]
    pub id: i64,
    #[serde(deserialize_with = "null_default")]
    pub fallback: String,
    #[serde(deserialize_with = "null_default")]
    pub color: String,
    #[serde(deserialize_with = "null_default")]
    pub pretext: String,
    #[serde(deserialize_with = "null_default")]
    pub author_name: String,
    #[serde(deserialize_with = "null_default")]
    pub author_link: String,
    #[serde(deserialize_with = "null_default")]
    pub author_icon: String,
    #[serde(deserialize_with = "null_default")]
    pub title: String,
    #[serde(deserialize_with = "null_default")]
    pub title_link: String,
    #[serde(deserialize_with = "null_default")]
    pub text: String,
    #[serde(deserialize_with = "nil_dropped")]
    pub fields: Option<Vec<AttachmentField>>,
    #[serde(deserialize_with = "null_default")]
    pub image_url: String,
    #[serde(deserialize_with = "null_default")]
    pub thumb_url: String,
    #[serde(deserialize_with = "null_default")]
    pub footer: String,
    #[serde(deserialize_with = "null_default")]
    pub footer_icon: String,
    /// A string or an integer.
    #[serde(rename = "ts")]
    pub timestamp: Value,
    #[serde(skip_serializing_if = "Vec::is_empty", deserialize_with = "nil_dropped_vec")]
    pub actions: Vec<PostAction>,
}

/// `MessageAttachmentField`.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct AttachmentField {
    #[serde(deserialize_with = "null_default")]
    pub title: String,
    pub value: Value,
    #[serde(deserialize_with = "slack_bool")]
    pub short: bool,
}

/// `PostAction`.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct PostAction {
    #[serde(skip_serializing_if = "String::is_empty", deserialize_with = "null_default")]
    pub id: String,
    #[serde(rename = "type", skip_serializing_if = "String::is_empty", deserialize_with = "null_default")]
    pub kind: String,
    #[serde(skip_serializing_if = "String::is_empty", deserialize_with = "null_default")]
    pub name: String,
    #[serde(skip_serializing_if = "String::is_empty", deserialize_with = "null_default")]
    pub tooltip: String,
    #[serde(skip_serializing_if = "std::ops::Not::not", deserialize_with = "null_default")]
    pub disabled: bool,
    #[serde(skip_serializing_if = "String::is_empty", deserialize_with = "null_default")]
    pub style: String,
    #[serde(skip_serializing_if = "String::is_empty", deserialize_with = "null_default")]
    pub data_source: String,
    #[serde(skip_serializing_if = "Vec::is_empty", deserialize_with = "null_default")]
    pub options: Vec<Option<ActionOption>>,
    #[serde(skip_serializing_if = "String::is_empty", deserialize_with = "null_default")]
    pub default_option: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub integration: Option<ActionIntegration>,
    #[serde(skip_serializing_if = "String::is_empty", deserialize_with = "null_default")]
    pub cookie: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct ActionOption {
    #[serde(deserialize_with = "null_default")]
    pub text: String,
    #[serde(deserialize_with = "null_default")]
    pub value: String,
}

/// `PostActionIntegration`.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct ActionIntegration {
    #[serde(skip_serializing_if = "String::is_empty", deserialize_with = "null_default")]
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<Map<String, Value>>,
}

/// `[]*T` where `null` elements are dropped, as `Attachments()` does with fields.
fn nil_dropped<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<Vec<T>>, D::Error> {
    let v: Option<Vec<Option<T>>> = Option::deserialize(d)?;
    Ok(v.map(|v| v.into_iter().flatten().collect::<Vec<T>>()).filter(|v| !v.is_empty()))
}

fn nil_dropped_vec<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Vec<T>, D::Error> {
    Ok(nil_dropped(d)?.unwrap_or_default())
}

/// `SlackCompatibleBool`: `true`, `false`, `"true"`, `"false"` in any case.
fn slack_bool<'de, D: Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    match Value::deserialize(d)? {
        Value::Bool(b) => Ok(b),
        Value::String(s) if s.eq_ignore_ascii_case("true") => Ok(true),
        Value::String(s) if s.eq_ignore_ascii_case("false") => Ok(false),
        v => Err(serde::de::Error::custom(format!("unmarshal: unable to convert {v} to bool"))),
    }
}

/// `Post.Attachments()`: each attachment that decodes, the others skipped.
pub fn attachments(props: &Map<String, Value>) -> Vec<MessageAttachment> {
    let Some(Value::Array(items)) = props.get("attachments") else { return vec![] };
    items.iter().filter_map(|a| serde_json::from_value::<MessageAttachment>(a.clone()).ok()).collect()
}

fn attachments_value(list: &[MessageAttachment]) -> Value {
    if list.is_empty() { Value::Null } else { serde_json::to_value(list).unwrap_or(Value::Null) }
}

/// `GenerateActionIds` (`Post.PreSave`/`PreCommit`): attachments as Go's typed list, every action with an id.
pub fn generate_action_ids(props: &mut Map<String, Value>) {
    if !props.contains_key("attachments") {
        return;
    }
    let mut list = attachments(props);
    for a in &mut list {
        for act in &mut a.actions {
            if act.id.is_empty() {
                act.id = new_id();
            }
        }
    }
    props.insert("attachments".into(), attachments_value(&list));
}

/// `Post.ToJSON`'s `StripActionIntegrations`: what a client may see of `props` — attachments (in Go's field
/// order) without their actions' integrations, and `mm_blocks_actions` only as its encrypted cookie.
pub fn serialize_props<S: Serializer>(props: &StringInterface, s: S) -> Result<S::Ok, S::Error> {
    let m = &props.0;
    let strip_blocks = m.get("mm_blocks_actions").is_some_and(|v| !v.is_string() && !v.is_null());
    if !m.contains_key("attachments") && !strip_blocks {
        return m.serialize(s);
    }
    let mut out = s.serialize_map(Some(m.len()))?;
    for (k, v) in m {
        match k.as_str() {
            "attachments" => {
                let mut list = attachments(m);
                for a in &mut list {
                    for act in &mut a.actions {
                        act.integration = None;
                    }
                }
                if list.is_empty() {
                    out.serialize_entry(k, &Value::Null)?;
                } else {
                    out.serialize_entry(k, &list)?;
                }
            }
            "mm_blocks_actions" if strip_blocks => {}
            _ => out.serialize_entry(k, v)?,
        }
    }
    out.end()
}

/// `ParseSlackLinksToMarkdown`: `<url|text>` → `[text](url)`.
pub fn slack_links_to_markdown(text: &str) -> String {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"<([^<|]+)\|([^>]+)>").unwrap()).replace_all(text, "[$2]($1)").into_owned()
}

// ---------- keys ----------

/// The server's `PostActionCookieSecret` (AES-256) and `AsymmetricSigningKey` (P-256), from `Systems`; made and
/// saved as Go does when missing.
pub struct Keys {
    pub cookie: [u8; 32],
    pub signing: p256::ecdsa::SigningKey,
}

impl App {
    pub async fn integration_keys(&self) -> ApiResult<&'static Keys> {
        static KEYS: tokio::sync::OnceCell<Keys> = tokio::sync::OnceCell::const_new();
        KEYS.get_or_try_init(|| async {
            let cookie = self.cookie_secret().await?;
            let signing = self.signing_key().await?;
            Ok(Keys { cookie, signing })
        })
        .await
    }

    async fn system_value(&self, name: &str) -> ApiResult<Option<String>> {
        Ok(sqlx::query_scalar("SELECT Value FROM Systems WHERE Name = $1").bind(name).fetch_optional(&self.db).await?)
    }

    /// Saves `value` unless another server did first; the stored value either way.
    async fn system_insert(&self, name: &str, value: &str) -> ApiResult<String> {
        sqlx::query("INSERT INTO Systems (Name, Value) VALUES ($1, $2) ON CONFLICT (Name) DO NOTHING")
            .bind(name)
            .bind(value)
            .execute(&self.db)
            .await?;
        self.system_value(name).await?.ok_or_else(|| AppError::internal("app.system.get_by_name.app_error", name))
    }

    async fn cookie_secret(&self) -> ApiResult<[u8; 32]> {
        let v = match self.system_value("PostActionCookieSecret").await? {
            Some(v) => v,
            None => {
                let k: [u8; 32] = rand::random();
                self.system_insert("PostActionCookieSecret", &json!({ "key": B64.encode(k) }).to_string()).await?
            }
        };
        let parsed: Value = serde_json::from_str(&v).unwrap_or_default();
        let key = parsed["key"].as_str().and_then(|k| B64.decode(k).ok()).filter(|k| k.len() == 32);
        let key =
            key.ok_or_else(|| AppError::internal("app.post_action_cookie_secret", "bad PostActionCookieSecret"))?;
        Ok(key.try_into().unwrap())
    }

    pub(crate) async fn signing_key(&self) -> ApiResult<p256::ecdsa::SigningKey> {
        let v = match self.system_value("AsymmetricSigningKey").await? {
            Some(v) => v,
            None => {
                let sk = loop {
                    let d: [u8; 32] = rand::random();
                    if let Ok(k) = p256::ecdsa::SigningKey::from_slice(&d) {
                        break k;
                    }
                };
                let point = sk.verifying_key().to_sec1_point(false);
                let (x, y) = (point.x().unwrap(), point.y().unwrap());
                let d = sk.to_bytes();
                let v = format!(
                    r#"{{"ecdsa_key":{{"curve":"P-256","x":{},"y":{},"d":{}}}}}"#,
                    to_decimal(x),
                    to_decimal(y),
                    to_decimal(&d)
                );
                self.system_insert("AsymmetricSigningKey", &v).await?
            }
        };
        let d =
            big_int_field(&v, "d").ok_or_else(|| AppError::internal("app.signing_key", "bad AsymmetricSigningKey"))?;
        p256::ecdsa::SigningKey::from_slice(&d).map_err(|e| AppError::internal("app.signing_key", e.to_string()))
    }
}

/// A big integer field of Go's `SystemECDSAKey` JSON (a decimal number), as 32 big-endian bytes.
fn big_int_field(json: &str, k: &str) -> Option<[u8; 32]> {
    let i = json.find(&format!("\"{k}\":"))? + k.len() + 3;
    let digits: String = json[i..].trim_start().chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    let mut out = [0u8; 32];
    for d in digits.bytes() {
        let mut carry = (d - b'0') as u32;
        for b in out.iter_mut().rev() {
            let x = *b as u32 * 10 + carry;
            *b = x as u8;
            carry = x >> 8;
        }
        if carry != 0 {
            return None;
        }
    }
    Some(out)
}

/// Big-endian bytes as a decimal number.
fn to_decimal(bytes: &[u8]) -> String {
    let mut n: Vec<u8> = bytes.to_vec();
    let mut digits = vec![];
    while n.iter().any(|b| *b != 0) {
        let mut rem = 0u32;
        for b in n.iter_mut() {
            let x = (rem << 8) | *b as u32;
            *b = (x / 10) as u8;
            rem = x % 10;
        }
        digits.push(b'0' + rem as u8);
    }
    if digits.is_empty() {
        return "0".into();
    }
    digits.reverse();
    String::from_utf8(digits).unwrap()
}

/// `EncryptPostActionCookie`: AES-256-GCM, nonce first, base64.
pub fn encrypt_cookie(plain: &str, key: &[u8; 32]) -> String {
    let c = Aes256Gcm::new(key.into());
    let nonce: [u8; 12] = rand::random();
    let sealed = c.encrypt(&Nonce::try_from(&nonce[..]).unwrap(), plain.as_bytes()).unwrap_or_default();
    let mut all = nonce.to_vec();
    all.extend(sealed);
    B64.encode(all)
}

/// `DecryptPostActionCookie`.
pub fn decrypt_cookie(encoded: &str, key: &[u8; 32]) -> Result<String, String> {
    let raw = B64.decode(encoded).map_err(|e| e.to_string())?;
    if raw.len() < 12 {
        return Err("cookie too short".into());
    }
    let c = Aes256Gcm::new(key.into());
    let plain = c
        .decrypt(&Nonce::try_from(&raw[..12]).map_err(|_| "bad nonce".to_string())?, &raw[12..])
        .map_err(|e| e.to_string())?;
    String::from_utf8(plain).map_err(|e| e.to_string())
}

/// `PostActionCookie` / `MmBlocksActionCookie`.
#[derive(Serialize, Deserialize, Default, Debug, Clone)]
#[serde(default)]
struct Cookie {
    #[serde(skip_serializing_if = "String::is_empty")]
    kind: String,
    #[serde(rename = "type", skip_serializing_if = "String::is_empty")]
    kind_of_action: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    post_id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    root_post_id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    channel_id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    data_source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    integration: Option<ActionIntegration>,
    #[serde(skip_serializing_if = "Map::is_empty")]
    retain_props: Map<String, Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    remove_props: Vec<String>,
    /// mm_blocks only (always written for them, `null` when absent).
    #[serde(skip_serializing_if = "Option::is_none")]
    actions: Option<Map<String, Value>>,
}

const MM_BLOCKS_KIND: &str = "mm_blocks_actions";

fn retain_and_remove(props: &Map<String, Value>) -> (Map<String, Value>, Vec<String>) {
    let mut retain = Map::new();
    let mut remove = vec![];
    for k in RETAIN_KEYS {
        match props.get(k) {
            Some(v) => {
                retain.insert(k.into(), v.clone());
            }
            None => remove.push(k.to_string()),
        }
    }
    (retain, remove)
}

/// `AddPostActionCookies`: each attachment action gets its encrypted cookie, and `mm_blocks_actions` becomes one.
pub fn add_action_cookies(p: &mut Post, key: &[u8; 32]) {
    let (retain, remove) = retain_and_remove(&p.props.0);
    let root = if p.root_id.is_empty() { p.id.clone() } else { p.root_id.clone() };
    if p.props.0.contains_key("attachments") {
        let mut list = attachments(&p.props.0);
        for a in &mut list {
            for act in &mut a.actions {
                let c = Cookie {
                    kind_of_action: act.kind.clone(),
                    post_id: p.id.clone(),
                    root_post_id: root.clone(),
                    channel_id: p.channel_id.clone(),
                    data_source: act.data_source.clone(),
                    integration: act.integration.clone(),
                    retain_props: retain.clone(),
                    remove_props: remove.clone(),
                    ..Default::default()
                };
                act.cookie = encrypt_cookie(&serde_json::to_string(&c).unwrap(), key);
            }
        }
        p.props.0.insert("attachments".into(), attachments_value(&list));
    }
    if let Some(Value::Object(actions)) = p.props.0.get("mm_blocks_actions").cloned()
        && !actions.is_empty()
    {
        let enc: Map<String, Value> = actions.into_iter().filter(|(_, v)| v.is_object()).collect();
        let c = Cookie {
            kind: MM_BLOCKS_KIND.into(),
            post_id: p.id.clone(),
            root_post_id: root,
            channel_id: p.channel_id.clone(),
            retain_props: retain,
            remove_props: remove,
            actions: Some(enc),
            ..Default::default()
        };
        p.props.0.insert("mm_blocks_actions".into(), encrypt_cookie(&serde_json::to_string(&c).unwrap(), key).into());
    }
}

// ---------- trigger ids ----------

/// `GenerateTriggerId`: (the client's trigger id, the signed trigger id the integration gets).
pub fn generate_trigger_id(user_id: &str, channel_id: &str, key: &p256::ecdsa::SigningKey) -> (String, String) {
    use p256::ecdsa::signature::Signer;
    let client = new_id();
    let data = format!("{client}:{user_id}:{}:{channel_id}:", now_ms());
    let sig: p256::ecdsa::Signature = key.sign(data.as_bytes());
    let sig = B64.encode(sig.to_der().as_bytes());
    (client, B64.encode(format!("{data}{sig}")))
}

/// `DecodeAndVerifyTriggerId`: (client trigger id, user id, channel id).
pub fn verify_trigger_id(
    trigger: &str,
    key: &p256::ecdsa::SigningKey,
    timeout: Duration,
) -> ApiResult<(String, String, String)> {
    use p256::ecdsa::signature::Verifier;
    let bad = |id: &str| AppError::bad_request(id, "");
    let raw = B64.decode(trigger).map_err(|_| bad("interactive_message.decode_trigger_id.base64_decode_failed"))?;
    let s = String::from_utf8_lossy(&raw).into_owned();
    let split: Vec<&str> = s.split(':').collect();
    if split.len() < 5 {
        return Err(bad("interactive_message.decode_trigger_id.missing_data"));
    }
    let ts: i64 = split[2].parse().unwrap_or(0);
    if now_ms() - ts > timeout.as_millis() as i64 {
        return Err(AppError::bad_request("interactive_message.decode_trigger_id.expired", "")
            .with_param("Duration", &format!("{}s", timeout.as_secs())));
    }
    let sig = B64
        .decode(split[split.len() - 1])
        .map_err(|_| bad("interactive_message.decode_trigger_id.base64_decode_failed_signature"))?;
    let sig = p256::ecdsa::Signature::from_der(&sig)
        .map_err(|_| bad("interactive_message.decode_trigger_id.signature_decode_failed"))?;
    let data = split[..split.len() - 1].join(":") + ":";
    key.verifying_key()
        .verify(data.as_bytes(), &sig)
        .map_err(|_| bad("interactive_message.decode_trigger_id.verify_signature_failed"))?;
    Ok((split[0].to_string(), split[1].to_string(), split[3].to_string()))
}

// ---------- the action request ----------

/// `PostActionIntegrationRequest`, in Go's field order.
#[derive(Serialize, Default, Clone, Debug)]
struct ActionRequest {
    user_id: String,
    user_name: String,
    channel_id: String,
    channel_name: String,
    team_id: String,
    team_domain: String,
    post_id: String,
    trigger_id: String,
    #[serde(rename = "type")]
    kind: String,
    data_source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    context: Option<Map<String, Value>>,
}

/// `PostActionIntegrationResponse`.
#[derive(Deserialize, Default, Debug)]
#[serde(default)]
struct ActionResponse {
    update: Option<Value>,
    ephemeral_text: String,
    skip_slack_parsing: bool,
    goto_location: String,
}

/// What `resolvePostActionSetup` gathers.
#[derive(Default)]
struct Setup {
    url: String,
    req: ActionRequest,
    data_source: String,
    retain: Map<String, Value>,
    remove: Vec<String>,
    original_props: Option<Map<String, Value>>,
    original_pinned: bool,
    original_reactions: bool,
    root_post_id: String,
    ephemeral: bool,
    ephemeral_channel: String,
    ephemeral_root: String,
    ephemeral_set_user: bool,
}

fn action_error(detail: impl Into<String>) -> AppError {
    AppError::bad_request("api.post.do_action.action_integration.app_error", "").detail(detail)
}

fn action_not_found(detail: impl Into<String>) -> AppError {
    AppError::not_found("api.post.do_action.action_id.app_error", "").detail(detail)
}

/// `MergeQueryIntoURL`.
fn merge_query(raw: &str, q: &BTreeMap<String, String>) -> Result<String, String> {
    if q.is_empty() {
        return Ok(raw.to_string());
    }
    // Go's url.Values: the merged query is re-encoded sorted by key.
    let (base, frag) = match raw.split_once('#') {
        Some((b, f)) => (b, Some(f)),
        None => (raw, None),
    };
    let (path, query) = base.split_once('?').unwrap_or((base, ""));
    let mut values: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for kv in query.split('&').filter(|s| !s.is_empty()) {
        let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
        values.entry(form_decode(k)).or_default().push(form_decode(v));
    }
    for (k, v) in q {
        values.insert(k.clone(), vec![v.clone()]);
    }
    let enc: Vec<String> = values
        .iter()
        .flat_map(|(k, vs)| vs.iter().map(move |v| format!("{}={}", query_escape(k), query_escape(v))))
        .collect();
    let mut out = format!("{path}?{}", enc.join("&"));
    if let Some(f) = frag {
        out.push('#');
        out.push_str(f);
    }
    Ok(out)
}

/// `url.QueryUnescape` as forms use it (`+` is a space).
pub fn form_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => match u8::from_str_radix(&s[i + 1..i + 3], 16) {
                Ok(x) => {
                    out.push(x);
                    i += 2;
                }
                Err(_) => out.push(b'%'),
            },
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Go's `url.QueryEscape`.
fn query_escape(s: &str) -> String {
    crate::opengraph::query_escape(s)
}

/// An `mm_blocks_actions` entry (`mmBlocksEntryMapToSpec`): (type, url, static query, context).
struct BlockSpec {
    kind: String,
    url: String,
    query: BTreeMap<String, String>,
    context: Option<Map<String, Value>>,
}

fn block_spec(actions: Option<&Value>, id: &str) -> Option<BlockSpec> {
    let entry = actions?.as_object()?.get(id)?.as_object()?;
    let kind = entry.get("type")?.as_str()?.to_string();
    if kind != "external" && kind != "openURL" {
        return None;
    }
    let query: BTreeMap<String, String> = entry
        .get("query")
        .and_then(Value::as_object)
        .map(|m| m.iter().filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string()))).collect())
        .unwrap_or_default();
    let context = if kind == "external" {
        match entry.get("context") {
            Some(Value::String(s)) if !s.is_empty() => match serde_json::from_str::<Map<String, Value>>(s) {
                Ok(m) => Some(m),
                Err(_) => Some([("context".to_string(), Value::String(s.clone()))].into_iter().collect()),
            },
            Some(Value::Object(m)) => Some(m.clone()),
            _ => None,
        }
    } else {
        None
    };
    Some(BlockSpec { kind, url: entry.get("url").and_then(Value::as_str).unwrap_or("").to_string(), query, context })
}

/// `ResolveMmBlocksAction`: Ok(goto url) or Ok(external url, context).
enum Resolved {
    Goto(String),
    External(String, Option<Map<String, Value>>),
}

fn resolve_block(spec: Option<BlockSpec>, id: &str, client: &BTreeMap<String, String>) -> ApiResult<Resolved> {
    let Some(spec) = spec.filter(|s| !s.url.is_empty()) else {
        return Err(action_not_found(format!("mm_blocks action_id={id}")));
    };
    let url = merge_query(&spec.url, &spec.query).map_err(action_error)?;
    if spec.kind == "openURL" {
        return Ok(Resolved::Goto(url));
    }
    let url = merge_query(&url, client).map_err(action_error)?;
    Ok(Resolved::External(url, spec.context))
}

/// `PostActionPreserveState`.
fn preserve(p: &Post, setup: &mut Setup) {
    let (retain, remove) = retain_and_remove(&p.props.0);
    setup.retain = retain;
    setup.remove = remove;
    setup.original_props = Some(p.props.0.clone());
    setup.original_pinned = p.is_pinned;
    setup.original_reactions = p.has_reactions;
    setup.root_post_id = if p.root_id.is_empty() { p.id.clone() } else { p.root_id.clone() };
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct DoActionBody {
    selected_option: String,
    cookie: String,
    query: Option<BTreeMap<String, String>>,
    integration_format: String,
}

/// `doPostAction`.
async fn do_post_action(
    St(app): St<State>,
    Auth(s): Auth,
    Path((post_id, action_id)): Path<(String, String)>,
    body: axum::body::Bytes,
) -> ApiResult<Json<ActionApiResponse>> {
    if !is_valid_id(&post_id) {
        return Err(AppError::invalid_url_param("post_id"));
    }
    if action_id.is_empty() || !action_id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return Err(AppError::invalid_url_param("action_id"));
    }
    let b: DoActionBody = if body.iter().all(u8::is_ascii_whitespace) {
        DoActionBody::default()
    } else {
        serde_json::from_slice(&body).map_err(|e| AppError::invalid_param("action_request").detail(e.to_string()))?
    };
    let keys = app.integration_keys().await?;
    let mut cookie: Option<Cookie> = None;
    if !b.cookie.is_empty() {
        let plain = decrypt_cookie(&b.cookie, &keys.cookie).map_err(action_error)?;
        let c: Cookie = serde_json::from_str(&plain).map_err(|e| action_error(e.to_string()))?;
        if c.post_id != post_id {
            return Err(AppError::forbidden_perm(perm::READ_CHANNEL_CONTENT));
        }
        let channel = app.channel_or_404(&c.channel_id).await?;
        if !app.can_read_channel(&s, &channel).await?.0 {
            return Err(AppError::forbidden_perm(perm::READ_CHANNEL_CONTENT));
        }
        cookie = Some(c);
    } else if !app.read_post_perm(&s, &post_id).await?.0 {
        return Err(AppError::forbidden_perm(perm::READ_CHANNEL_CONTENT));
    }
    let (trigger_id, goto) = app
        .do_post_action(
            &s,
            &post_id,
            &action_id,
            &b.selected_option,
            cookie,
            b.query.unwrap_or_default(),
            &b.integration_format,
        )
        .await?;
    Ok(Json(ActionApiResponse { status: "OK", trigger_id, goto_location: goto }))
}

/// `PostActionAPIResponse`.
#[derive(Serialize)]
struct ActionApiResponse {
    status: &'static str,
    trigger_id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    goto_location: String,
}

impl App {
    /// `DoPostActionWithCookie`: (client trigger id, goto location).
    #[allow(clippy::too_many_arguments)]
    async fn do_post_action(
        &self,
        s: &Session,
        post_id: &str,
        action_id: &str,
        selected: &str,
        cookie: Option<Cookie>,
        query: BTreeMap<String, String>,
        format: &str,
    ) -> ApiResult<(String, String)> {
        // ValidateActionQuery
        if query.len() > 50 || query.iter().any(|(k, v)| k.len() > 128 || v.len() > 2048) {
            return Err(AppError::bad_request("api.post.do_action.query.app_error", ""));
        }
        let mut setup = Setup {
            req: ActionRequest { user_id: s.user_id.clone(), post_id: post_id.into(), ..Default::default() },
            ..Default::default()
        };
        let post = self.get_single_post(post_id, false).await?;
        match post {
            None => {
                let Some(c) = cookie else {
                    return Err(AppError::not_found("app.post.get.app_error", ""));
                };
                if c.post_id != post_id {
                    return Err(action_error("postId doesn't match"));
                }
                let channel = self.channel_or_404(&c.channel_id).await?;
                setup.req.channel_id = c.channel_id.clone();
                setup.req.channel_name = channel.name.clone();
                setup.req.team_id = channel.team_id.clone();
                setup.retain = c.retain_props.clone();
                setup.remove = c.remove_props.clone();
                setup.root_post_id = c.root_post_id.clone();
                setup.ephemeral = true;
                setup.ephemeral_channel = c.channel_id.clone();
                setup.ephemeral_root = c.root_post_id.clone();
                if c.kind == MM_BLOCKS_KIND {
                    let actions = c.actions.clone().map(Value::Object);
                    match resolve_block(block_spec(actions.as_ref(), action_id), action_id, &query)? {
                        Resolved::Goto(u) => return Ok((String::new(), u)),
                        Resolved::External(u, ctx) => {
                            setup.url = u;
                            setup.req.kind = "button".into();
                            setup.req.context = ctx;
                        }
                    }
                } else {
                    let Some(integration) = c.integration.clone() else {
                        return Err(action_error("no Integration in action cookie"));
                    };
                    setup.url = integration.url;
                    setup.req.kind = c.kind_of_action.clone();
                    setup.req.context = integration.context;
                    setup.data_source = c.data_source.clone();
                    setup.ephemeral_set_user = true;
                }
            }
            Some(p) => {
                let channel = self.channel_or_404(&p.channel_id).await?;
                setup.req.channel_id = p.channel_id.clone();
                setup.req.channel_name = channel.name.clone();
                setup.req.team_id = channel.team_id.clone();
                match format.trim().to_lowercase().as_str() {
                    "mm_block" | "block" | "card" => {
                        match resolve_block(
                            block_spec(p.props.0.get("mm_blocks_actions"), action_id),
                            action_id,
                            &query,
                        )? {
                            Resolved::Goto(u) => return Ok((String::new(), u)),
                            Resolved::External(u, ctx) => {
                                preserve(&p, &mut setup);
                                setup.url = u;
                                setup.req.kind = "button".into();
                                setup.req.context = ctx;
                            }
                        }
                    }
                    "apps_binding" => {
                        return Err(action_not_found(format!("integration_context={format}")));
                    }
                    _ => {
                        let action = find_action(&p, action_id).ok_or_else(|| action_not_found("action=<nil>"))?;
                        let integration = action.integration.clone().unwrap_or_default();
                        preserve(&p, &mut setup);
                        setup.url = integration.url;
                        setup.req.kind = action.kind.clone();
                        setup.req.context = integration.context;
                        setup.data_source = action.data_source.clone();
                    }
                }
            }
        }
        // populateUpstreamRequestIdentity
        let user = self.user(&s.user_id).await?.ok_or_else(crate::users::missing_account)?;
        setup.req.user_name = user.username.clone();
        if !setup.req.team_id.is_empty() {
            let team = self
                .team(&setup.req.team_id)
                .await
                .map_err(|_| AppError::not_found("app.team.get.find.app_error", ""))?;
            setup.req.team_domain = team.name.clone();
        }
        if !selected.is_empty() {
            setup.req.context.get_or_insert_with(Map::new).insert("selected_option".into(), selected.into());
            setup.req.data_source = setup.data_source.clone();
        }
        let keys = self.integration_keys().await?;
        let (client_trigger, trigger) = generate_trigger_id(&setup.req.user_id, &setup.req.channel_id, &keys.signing);
        setup.req.trigger_id = trigger;
        let body = serde_json::to_vec(&setup.req).unwrap();
        let resp = self.do_action_request(s, &setup.url, body).await?;
        let response: ActionResponse = if resp.is_empty() {
            ActionResponse::default()
        } else {
            serde_json::from_slice(&resp).map_err(|e| action_error(e.to_string()))?
        };
        if let Some(update) = response.update.filter(|u| !u.is_null()) {
            self.apply_action_update(s, &setup, post_id, update).await?;
        }
        if !response.ephemeral_text.is_empty() {
            let message = if response.skip_slack_parsing {
                response.ephemeral_text.clone()
            } else {
                slack_links_to_markdown(&response.ephemeral_text)
            };
            let mut p = Post {
                message,
                channel_id: setup.req.channel_id.clone(),
                root_id: setup.root_post_id.clone(),
                user_id: s.user_id.clone(),
                ..Default::default()
            };
            for (k, v) in &setup.retain {
                p.props.0.insert(k.clone(), v.clone());
            }
            self.send_ephemeral(&s.user_id, p).await;
        }
        Ok((client_trigger, response.goto_location))
    }

    /// `DoActionRequest`: POST to the integration; a plugin's path is a local request to the plugin, whose
    /// answer is read whatever its status.
    async fn do_action_request(&self, s: &Session, raw: &str, body: Vec<u8>) -> ApiResult<Vec<u8>> {
        let clean = raw.trim_start_matches("./");
        if clean.starts_with("/plugins/") || clean.starts_with("plugins/") {
            let mut r = crate::plugins::http::plugin_request(self, s, "POST", clean, body).await?;
            r.body.truncate(MAX_RESPONSE);
            return Ok(r.body);
        }
        let u = reqwest::Url::parse(raw).map_err(|e| action_error(e.to_string()))?;
        let site = reqwest::Url::parse(self.cfg.str("ServiceSettings.SiteURL")).ok();
        let to_plugins = site.as_ref().and_then(|s| s.host_str()) == u.host_str()
            && u.host_str().is_some()
            && u.path().starts_with("/plugins");
        let timeout =
            Duration::from_secs(self.cfg.i64("ServiceSettings.OutgoingIntegrationRequestsTimeout").max(1) as u64);
        let mut req = if to_plugins {
            self.trusted_client().post(u.clone()).bearer_auth(&s.token)
        } else {
            if !self.untrusted_url_allowed(&u) {
                return Err(action_error(
                    "address forbidden, you may need to set AllowedUntrustedInternalConnections to allow an integration access to your internal network",
                ));
            }
            self.integration_client().post(u.clone())
        };
        req = req
            .timeout(timeout)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .body(body);
        let resp = req.send().await.map_err(|e| action_error(e.to_string()))?;
        let status = resp.status().as_u16();
        if status != 200 {
            let code = match status {
                429 | 503 => status,
                s if s >= 500 => 502,
                _ => 400,
            };
            return Err(AppError::new(
                StatusCode::from_u16(code).unwrap(),
                "api.post.do_action.action_integration.app_error",
                "",
            )
            .detail(format!("status={status}")));
        }
        read_limited(resp, MAX_RESPONSE).await.map_err(action_error)
    }

    /// `applyPostActionUpdate`.
    async fn apply_action_update(&self, s: &Session, setup: &Setup, post_id: &str, update: Value) -> ApiResult<()> {
        // Go's `update.GetProps() == nil`: no `props` at all in the integration's update.
        let had_props = update.get("props").is_some_and(|p| !p.is_null());
        let mut update: Post = serde_json::from_value(update).map_err(|e| action_error(e.to_string()))?;
        update.id = post_id.into();
        if !had_props {
            if let Some(orig) = &setup.original_props {
                update.props.0 = orig.clone();
            } else if setup.ephemeral && !setup.retain.is_empty() {
                update.props.0 = setup.retain.clone();
            }
        } else {
            for (k, v) in &setup.retain {
                update.props.0.insert(k.clone(), v.clone());
            }
            for k in &setup.remove {
                update.props.0.remove(k);
            }
        }
        update.is_pinned = setup.original_pinned;
        update.has_reactions = setup.original_reactions;
        if update.props.0.contains_key("mm_blocks_actions") {
            let original = setup.original_props.as_ref().and_then(|o| o.get("mm_blocks_actions")).cloned();
            match original {
                None => {
                    update.props.0.remove("mm_blocks_actions");
                }
                Some(o) => {
                    if !update.props.0.get("mm_blocks_actions").is_some_and(Value::is_object) {
                        update.props.0.insert("mm_blocks_actions".into(), o);
                    }
                }
            }
        }
        if setup.ephemeral {
            update.channel_id = setup.ephemeral_channel.clone();
            if update.root_id.is_empty() && !setup.ephemeral_root.is_empty() && setup.ephemeral_root != post_id {
                update.root_id = setup.ephemeral_root.clone();
            }
            if setup.ephemeral_set_user && update.user_id.is_empty() {
                update.user_id = s.user_id.clone();
            }
            self.update_ephemeral(&s.user_id, update).await;
            return Ok(());
        }
        self.update_post_opts(s, update, false, true).await?;
        Ok(())
    }

    /// `UpdateEphemeralPost`: `post_edited` to the user only.
    pub async fn update_ephemeral(&self, user_id: &str, mut p: Post) {
        p.r#type = crate::model::post::kind::EPHEMERAL.into();
        p.update_at = now_ms();
        generate_action_ids(&mut p.props.0);
        let prep = crate::posts::Prep { is_new: true, include_priority: true, ..Default::default() };
        let mut p = self.prepare_post_full(&p, prep).await;
        if let Ok(k) = self.integration_keys().await {
            add_action_cookies(&mut p, &k.cookie);
        }
        let p = self.sanitize_for_user(p, user_id).await;
        self.hub.publish(
            "post_edited",
            json!({"post": serde_json::to_string(&p).unwrap()}),
            Broadcast { user_id: user_id.into(), channel_id: p.channel_id.clone(), ..Default::default() },
        );
    }
}

fn find_action(p: &Post, id: &str) -> Option<PostAction> {
    for a in attachments(&p.props.0) {
        if let Some(act) = a.actions.into_iter().find(|x| x.id == id) {
            return Some(act);
        }
    }
    let spec = block_spec(p.props.0.get("mm_blocks_actions"), id)?;
    if spec.kind != "external" || spec.url.is_empty() {
        return None;
    }
    let url = merge_query(&spec.url, &spec.query).ok()?;
    Some(PostAction {
        id: id.into(),
        kind: "button".into(),
        integration: Some(ActionIntegration { url, context: spec.context }),
        ..Default::default()
    })
}

async fn read_limited(resp: reqwest::Response, max: usize) -> Result<Vec<u8>, String> {
    use futures_util::StreamExt;
    let mut out = vec![];
    let mut s = resp.bytes_stream();
    while let Some(chunk) = s.next().await {
        let chunk = chunk.map_err(|e| e.to_string())?;
        let room = max - out.len();
        out.extend_from_slice(&chunk[..chunk.len().min(room)]);
        if out.len() >= max {
            break;
        }
    }
    Ok(out)
}

// ---------- dialogs ----------

#[derive(Deserialize, Serialize, Default, Debug)]
#[serde(default)]
pub(crate) struct OpenDialogRequest {
    trigger_id: String,
    url: String,
    dialog: Value,
    #[serde(skip_serializing_if = "String::is_empty")]
    channel_id: String,
}

/// `openDialog` (no session needed: the trigger id proves it).
async fn open_dialog(St(app): St<State>, MaybeAuth(_): MaybeAuth, body: axum::body::Bytes) -> ApiResult<Json<Value>> {
    let d: OpenDialogRequest =
        serde_json::from_slice(&body).map_err(|e| AppError::invalid_param("dialog").detail(e.to_string()))?;
    if d.url.is_empty() {
        return Err(AppError::invalid_param("url"));
    }
    app.open_interactive_dialog(d).await?;
    Ok(crate::users::ok())
}

impl App {
    /// The dialog, to the user its trigger id was made for.
    pub(crate) async fn open_interactive_dialog(&self, mut d: OpenDialogRequest) -> ApiResult<()> {
        let app = self;
        let keys = app.integration_keys().await?;
        let timeout =
            Duration::from_secs(app.cfg.i64("ServiceSettings.OutgoingIntegrationRequestsTimeout").max(1) as u64);
        let (client, user_id, channel_id) = verify_trigger_id(&d.trigger_id, &keys.signing, timeout)?;
        d.trigger_id = client;
        d.channel_id = channel_id;
        // OpenDialogRequest.IsValid only logs in 12.1; the dialog goes out as given.
        let dialog = dialog_json(&d);
        app.hub.publish("open_dialog", json!({ "dialog": dialog }), Broadcast { user_id, ..Default::default() });
        Ok(())
    }
}

/// The `OpenDialogRequest` JSON the client gets, in Go's order.
fn dialog_json(d: &OpenDialogRequest) -> String {
    #[derive(Serialize)]
    struct R<'a> {
        trigger_id: &'a str,
        url: &'a str,
        dialog: &'a Value,
        #[serde(skip_serializing_if = "str::is_empty")]
        channel_id: &'a str,
    }
    serde_json::to_string(&R { trigger_id: &d.trigger_id, url: &d.url, dialog: &d.dialog, channel_id: &d.channel_id })
        .unwrap()
}

/// `SubmitDialogRequest`, in Go's order.
#[derive(Deserialize, Serialize, Default, Debug)]
#[serde(default)]
struct SubmitDialogRequest {
    #[serde(rename = "type", deserialize_with = "null_default")]
    kind: String,
    #[serde(skip_serializing_if = "String::is_empty", deserialize_with = "null_default")]
    url: String,
    #[serde(deserialize_with = "null_default")]
    callback_id: String,
    #[serde(deserialize_with = "null_default")]
    state: String,
    #[serde(deserialize_with = "null_default")]
    user_id: String,
    #[serde(deserialize_with = "null_default")]
    channel_id: String,
    #[serde(deserialize_with = "null_default")]
    team_id: String,
    submission: Option<Map<String, Value>>,
    #[serde(deserialize_with = "null_default")]
    cancelled: bool,
    #[serde(skip_serializing_if = "Vec::is_empty", deserialize_with = "null_default")]
    file_ids: Vec<String>,
}

/// `submitDialog`.
async fn submit_dialog(St(app): St<State>, Auth(s): Auth, body: axum::body::Bytes) -> ApiResult<Json<Value>> {
    let mut r: SubmitDialogRequest =
        serde_json::from_slice(&body).map_err(|e| AppError::invalid_param("dialog").detail(e.to_string()))?;
    if r.url.is_empty() {
        return Err(AppError::invalid_param("url"));
    }
    r.user_id = s.user_id.clone();
    let channel = app.channel_or_404(&r.channel_id).await?;
    if !app.can_read_channel(&s, &channel).await?.0 {
        return Err(AppError::forbidden_perm(perm::READ_CHANNEL_CONTENT));
    }
    if !channel.team_id.is_empty() && !app.has_team_perm(&s, &channel.team_id, perm::VIEW_TEAM).await {
        return Err(AppError::forbidden_perm(perm::VIEW_TEAM));
    }
    r.team_id = channel.team_id.clone();
    Ok(Json(app.submit_dialog(&s, r).await?))
}

impl App {
    /// `SubmitInteractiveDialog`.
    async fn submit_dialog(&self, s: &Session, mut r: SubmitDialogRequest) -> ApiResult<Value> {
        let url = std::mem::take(&mut r.url);
        // Dedup without sorting.
        let mut seen = std::collections::HashSet::new();
        r.file_ids.retain(|f| seen.insert(f.clone()));
        if r.file_ids.len() > 10 {
            return Err(
                AppError::bad_request("app.submit_interactive_dialog.too_many_file_ids", "").with_param("Max", "10")
            );
        }
        if !r.file_ids.is_empty() {
            let found: Vec<(String, String)> =
                sqlx::query_as("SELECT Id, COALESCE(CreatorId, '') FROM FileInfo WHERE Id = ANY($1) AND DeleteAt = 0")
                    .bind(&r.file_ids)
                    .fetch_all(&self.db)
                    .await
                    .map_err(|e| {
                        AppError::internal("app.submit_interactive_dialog.get_file_info_error", e.to_string())
                    })?;
            for (id, creator) in &found {
                if creator != &r.user_id {
                    return Err(AppError::new(
                        StatusCode::FORBIDDEN,
                        "app.submit_interactive_dialog.file_not_owned",
                        "",
                    )
                    .with_param("FileId", id));
                }
            }
            for id in &r.file_ids {
                if !found.iter().any(|(f, _)| f == id) {
                    return Err(AppError::bad_request("app.submit_interactive_dialog.invalid_file_id", "")
                        .with_param("FileId", id));
                }
            }
        }
        // File ids smuggled in submission values must be the submitter's too.
        let mut candidates: Vec<String> = vec![];
        let declared: std::collections::HashSet<&String> = r.file_ids.iter().collect();
        let mut over = false;
        fn collect(
            v: &Value,
            depth: usize,
            declared: &std::collections::HashSet<&String>,
            out: &mut Vec<String>,
            over: &mut bool,
        ) {
            if depth > 100 || *over {
                return;
            }
            match v {
                Value::String(s) => {
                    for tok in s.split(',').map(str::trim) {
                        if tok.is_empty()
                            || declared.contains(&tok.to_string())
                            || out.iter().any(|x| x == tok)
                            || !is_valid_id(tok)
                        {
                            continue;
                        }
                        if out.len() >= 256 {
                            *over = true;
                            return;
                        }
                        out.push(tok.to_string());
                    }
                }
                Value::Array(a) => a.iter().for_each(|e| collect(e, depth + 1, declared, out, over)),
                Value::Object(m) => m.values().for_each(|e| collect(e, depth + 1, declared, out, over)),
                _ => {}
            }
        }
        if let Some(sub) = &r.submission {
            for v in sub.values() {
                collect(v, 0, &declared, &mut candidates, &mut over);
            }
        }
        if over {
            return Err(AppError::bad_request("app.submit_interactive_dialog.too_many_submission_ids", "")
                .with_param("Max", "256"));
        }
        if !candidates.is_empty()
            && let Ok(found) = sqlx::query_as::<_, (String, String)>(
                "SELECT Id, COALESCE(CreatorId, '') FROM FileInfo WHERE Id = ANY($1) AND DeleteAt = 0",
            )
            .bind(&candidates)
            .fetch_all(&self.db)
            .await
        {
            for (id, creator) in &found {
                if creator != &r.user_id {
                    return Err(AppError::new(
                        StatusCode::FORBIDDEN,
                        "app.submit_interactive_dialog.file_not_owned",
                        "",
                    )
                    .with_param("FileId", id));
                }
            }
            if declared.len() + found.len() > 10 {
                return Err(AppError::bad_request("app.submit_interactive_dialog.too_many_file_ids", "")
                    .with_param("Max", "10"));
            }
        }
        if r.kind != "refresh" {
            r.kind = "dialog_submission".into();
        }
        let body = serde_json::to_vec(&r).unwrap();
        let resp = self.do_action_request(s, &url, body).await?;
        if resp.is_empty() {
            return Ok(json!({}));
        }
        let v: Value = serde_json::from_slice(&resp)
            .map_err(|e| AppError::internal("app.submit_interactive_dialog.decode_json_error", e.to_string()))?;
        // SubmitDialogResponse: error, errors, type, form — the known fields, in Go's order.
        #[derive(Deserialize, Serialize, Default)]
        #[serde(default)]
        struct Resp {
            #[serde(skip_serializing_if = "String::is_empty", deserialize_with = "null_default")]
            error: String,
            #[serde(skip_serializing_if = "Option::is_none")]
            errors: Option<BTreeMap<String, String>>,
            #[serde(rename = "type", skip_serializing_if = "String::is_empty", deserialize_with = "null_default")]
            kind: String,
            #[serde(skip_serializing_if = "Option::is_none")]
            form: Option<Value>,
        }
        let out: Resp = serde_json::from_value(v)
            .map_err(|e| AppError::internal("app.submit_interactive_dialog.decode_json_error", e.to_string()))?;
        let errors_set = !out.error.is_empty() || out.errors.as_ref().is_some_and(|e| !e.is_empty());
        if !errors_set {
            let form = out.form.as_ref().filter(|f| !f.is_null());
            match out.kind.as_str() {
                "" | "ok" | "navigate" if form.is_some() => {
                    return Err(AppError::bad_request("app.submit_interactive_dialog.invalid_response", "")
                        .detail(format!("form field must be nil for type {:?}", out.kind)));
                }
                "" | "ok" | "navigate" => {}
                "form" if form.is_none() => {
                    return Err(AppError::bad_request("app.submit_interactive_dialog.invalid_response", "")
                        .detail("form field is required for form type"));
                }
                "form" => {}
                other => {
                    return Err(AppError::bad_request("app.submit_interactive_dialog.invalid_response", "")
                        .detail(format!("invalid type {other:?}, must be one of: empty, ok, form, navigate")));
                }
            }
        }
        Ok(serde_json::to_value(out).unwrap())
    }
}

/// `IsValidLookupURL`: a plugin path without traversal, or an http(s) URL with a host.
fn is_valid_lookup_url(url: &str) -> bool {
    if url.starts_with("/plugins/") {
        return !url.contains("..") && !url.contains("//");
    }
    (url.starts_with("http://") || url.starts_with("https://"))
        && reqwest::Url::parse(url).is_ok_and(|u| u.host_str().is_some_and(|h| !h.is_empty()))
}

/// The channel of a dialog request, readable by the session, and its team (empty for DMs and GMs) if the
/// session may view it, as `lookupDialog` and `executeDialogAction` check them.
async fn dialog_channel(app: &App, s: &Session, channel_id: &str) -> ApiResult<crate::model::Channel> {
    let channel = app.channel_or_404(channel_id).await?;
    if !app.can_read_channel(s, &channel).await?.0 {
        return Err(AppError::forbidden_perm(perm::READ_CHANNEL_CONTENT));
    }
    if !channel.team_id.is_empty() && !app.has_team_perm(s, &channel.team_id, perm::VIEW_TEAM).await {
        return Err(AppError::forbidden_perm(perm::VIEW_TEAM));
    }
    Ok((*channel).clone())
}

/// `lookupDialog`: a dynamic select's options, from the integration.
async fn lookup_dialog(St(app): St<State>, Auth(s): Auth, body: axum::body::Bytes) -> ApiResult<Json<Value>> {
    let mut r: SubmitDialogRequest =
        serde_json::from_slice(&body).map_err(|e| AppError::invalid_param("dialog").detail(e.to_string()))?;
    if r.url.is_empty() || !is_valid_lookup_url(&r.url) {
        return Err(AppError::invalid_param("url"));
    }
    r.user_id = s.user_id.clone();
    let channel = dialog_channel(&app, &s, &r.channel_id).await?;
    r.team_id = channel.team_id.clone();
    // LookupInteractiveDialog
    let url = std::mem::take(&mut r.url);
    r.kind = "dialog_lookup".into();
    let resp = app.do_action_request(&s, &url, serde_json::to_vec(&r).unwrap()).await?;
    if resp.is_empty() {
        return Ok(Json(json!({"items": null})));
    }
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct Lookup {
        items: Option<Vec<Value>>,
    }
    let out: Lookup = serde_json::from_slice(&resp)
        .map_err(|e| AppError::internal("app.lookup_interactive_dialog.decode_json_error", e.to_string()))?;
    // DialogSelectOption: text and value.
    let items = out.items.map(|list| {
        list.into_iter()
            .map(|i| {
                let f = |k: &str| i.get(k).and_then(Value::as_str).unwrap_or("").to_string();
                json!({"text": f("text"), "value": f("value")})
            })
            .collect::<Vec<_>>()
    });
    Ok(Json(json!({ "items": items })))
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ExecuteDialogActionRequest {
    #[serde(deserialize_with = "null_default")]
    url: String,
    context: Option<BTreeMap<String, String>>,
    #[serde(deserialize_with = "null_default")]
    channel_id: String,
    #[serde(deserialize_with = "null_default")]
    team_id: String,
}

/// `executeDialogAction`: a dialog's action button sent to the integration; the answer is a new trigger id.
async fn execute_dialog_action(St(app): St<State>, Auth(s): Auth, body: axum::body::Bytes) -> ApiResult<Json<Value>> {
    let r: ExecuteDialogActionRequest =
        serde_json::from_slice(&body).map_err(|e| AppError::invalid_param("dialog_action").detail(e.to_string()))?;
    if r.url.is_empty() || !is_valid_lookup_url(&r.url) {
        return Err(AppError::invalid_param("url"));
    }
    let channel = dialog_channel(&app, &s, &r.channel_id).await?;
    // ExecuteDialogAction
    let ctx = r.context.unwrap_or_default();
    if ctx.len() > 50 {
        return Err(action_error("query exceeds maximum of 50 entries"));
    }
    if let Some((k, _)) = ctx.iter().find(|(k, v)| k.len() > 128 || v.len() > 2048) {
        return Err(action_error(format!("query entry {k:?} too long")));
    }
    let user = app.user(&s.user_id).await?.ok_or_else(|| AppError::internal("app.user.get.app_error", ""))?;
    let mut req = ActionRequest {
        user_id: s.user_id.clone(),
        user_name: user.username.clone(),
        channel_id: channel.id.clone(),
        channel_name: channel.name.clone(),
        team_id: channel.team_id.clone(),
        kind: "dialog_action".into(),
        context: (!ctx.is_empty()).then(|| ctx.into_iter().map(|(k, v)| (k, Value::String(v))).collect()),
        ..Default::default()
    };
    if !channel.team_id.is_empty() {
        req.team_domain = app
            .team(&channel.team_id)
            .await
            .map_err(|e| AppError::internal("app.team.get.finding.app_error", e.detailed_error))?
            .name
            .clone();
    }
    let keys = app.integration_keys().await?;
    let (client, signed) = generate_trigger_id(&s.user_id, &channel.id, &keys.signing);
    req.trigger_id = signed;
    let clean = r.url.trim_start_matches("./");
    if clean.starts_with("/plugins/") || clean.starts_with("plugins/") {
        // The plugin's answer must be a 200, like an integration's.
        let resp =
            crate::plugins::http::plugin_request(&app, &s, "POST", clean, serde_json::to_vec(&req).unwrap_or_default())
                .await?;
        if resp.status != 200 {
            let code = match resp.status {
                429 | 503 => resp.status,
                s if s >= 500 => 502,
                _ => 400,
            };
            return Err(AppError::new(
                StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_GATEWAY),
                "api.post.do_action.action_integration.app_error",
                "",
            )
            .detail(format!("status={}", resp.status)));
        }
        return Ok(Json(json!({ "trigger_id": client })));
    }
    app.do_action_request(&s, &r.url, serde_json::to_vec(&req).unwrap()).await?;
    Ok(Json(json!({ "trigger_id": client })))
}

/// The ephemeral post of `SendEphemeralPost`, with action ids and cookies, ready for `send_ephemeral`.
pub async fn prepare_ephemeral(app: &App, p: &mut Post) {
    generate_action_ids(&mut p.props.0);
    if p.props.0.contains_key("attachments") || p.props.0.contains_key("mm_blocks_actions") {
        if p.id.is_empty() {
            p.id = new_id();
        }
        if let Ok(k) = app.integration_keys().await {
            add_action_cookies(p, &k.cookie);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookie_round_trip() {
        let key = [7u8; 32];
        let c = encrypt_cookie("{\"post_id\":\"x\"}", &key);
        assert_eq!(decrypt_cookie(&c, &key).unwrap(), "{\"post_id\":\"x\"}");
    }

    #[test]
    fn trigger_id_round_trip() {
        let sk = p256::ecdsa::SigningKey::from_slice(&[3u8; 32]).unwrap();
        let (client, trigger) = generate_trigger_id("u1", "c1", &sk);
        let (c, u, ch) = verify_trigger_id(&trigger, &sk, Duration::from_secs(30)).unwrap();
        assert_eq!((c.as_str(), u.as_str(), ch.as_str()), (client.as_str(), "u1", "c1"));
    }

    #[test]
    fn decimal_round_trip() {
        let n = [1u8, 2, 3, 255];
        let mut padded = [0u8; 32];
        padded[28..].copy_from_slice(&n);
        assert_eq!(big_int_field(&format!("{{\"d\":{}}}", to_decimal(&n)), "d").unwrap(), padded);
    }

    #[test]
    fn strips_integrations_in_go_order() {
        let mut props = Map::new();
        props.insert(
            "attachments".into(),
            json!([{"text": "t", "actions": [{"id": "a1", "name": "Go", "integration": {"url": "http://x", "context": {"k": 1}}}]}]),
        );
        let p = Post { props: sqlx::types::Json(props), ..Default::default() };
        let v = serde_json::to_string(&p).unwrap();
        assert!(!v.contains("integration"), "{v}");
        assert!(v.contains(r#""attachments":[{"id":0,"fallback":"","color":"""#), "{v}");
    }

    #[test]
    fn slack_links() {
        assert_eq!(slack_links_to_markdown("see <http://a|A> now"), "see [A](http://a) now");
    }
}

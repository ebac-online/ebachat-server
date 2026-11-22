//! Properties (PSAv2): the property fields, field options and values of a property group under
//! `/api/v4/properties/groups/{group_name}/...`, as an unlicensed server answers them. The `access_control` group
//! needs an Enterprise license for every field and value it holds (and Enterprise Advanced for its channel
//! attributes), `session_attributes` Enterprise Advanced; `boards` and `post_attributes` work without one. The option
//! hierarchy of `graph` fields is in `graph`.

// Based on Mattermost server/channels/api4/properties.go, api4/property_options.go, app/property_field.go,
// app/property_value.go, app/property_options.go, app/property_group.go, app/property_errors.go,
// app/property_field_helpers.go, app/authorization.go (property field permission levels),
// app/properties/{property_field.go, property_value.go, field_options.go, license_check.go, field_limit.go,
// type_change_value_cleanup.go}, store/sqlstore/{property_group_store.go, property_field_store.go,
// property_field_options.go, property_value_store.go} and public/model/property_*.go, Copyright (c)
// 2015-present Mattermost, Inc. Licensed under AGPL-3.0.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::app::Query;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, State as St};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post};
use serde::Serialize;
use serde_json::value::RawValue;
use serde_json::{Map, Value, json};
use sqlx::Row;
use sqlx::postgres::PgRow;

use crate::app::{ApiResult, App, AppError, Auth, State, is_valid_id, new_id, now_ms};
use crate::hub::Broadcast;
use crate::model::Session;
use crate::perm::MANAGE_SYSTEM;

mod graph;

const ACCESS_CONTROL: &str = "access_control";
const SESSION_ATTRIBUTES: &str = "session_attributes";
const POST_ATTRIBUTES: &str = "post_attributes";

const OBJECT_TYPES: [&str; 6] = ["post", "channel", "user", "template", "session", "system"];
const TARGET_TYPES: [&str; 3] = ["system", "team", "channel"];
const FIELD_TYPES: [&str; 8] = ["text", "select", "multiselect", "date", "user", "multiuser", "rank", "graph"];
const OPTION_TYPES: [&str; 4] = ["select", "multiselect", "rank", "graph"];
const PERMISSION_LEVELS: [&str; 5] = ["none", "sysadmin", "member", "admin", "creator"];

const PER_PAGE_DEFAULT: i64 = 60;
const PER_PAGE_MAXIMUM: i64 = 200;
const MAX_HYDRATED_OPTIONS: i64 = 1000;
const OPTIONS_MAX_PER_REQUEST: usize = 200;
const GRAPH_MAX_OPTIONS: usize = 100_000;
const MAX_VALUE_PATCH_ITEMS: usize = 50;
const POST_ATTRIBUTES_MAX_FIELDS_PER_TARGET: i64 = 50;
const SYSTEM_TARGET_ID: &str = "system";

pub fn routes() -> Router<State> {
    const G: &str = "/api/v4/properties/groups/{group_name}";
    Router::new()
        .route(&format!("{G}/{{object_type}}/fields"), get(get_fields).post(create_field))
        .route(&format!("{G}/{{object_type}}/fields/{{field_id}}"), patch(patch_field).delete(delete_field))
        .route(
            &format!("{G}/{{object_type}}/fields/{{field_id}}/options"),
            get(get_options).post(create_options).patch(patch_options).delete(delete_options),
        )
        .route(&format!("{G}/fields/search"), post(search_fields))
        .route(&format!("{G}/{{object_type}}/values/{{target_id}}"), get(get_values).patch(patch_values))
        .route(&format!("{G}/system/values"), get(get_system_values).patch(patch_system_values))
}

// ---------- model ----------

#[derive(Clone)]
struct Group {
    id: String,
    name: String,
    version: i32,
}

/// `model.PropertyField`, in Go's field order.
#[derive(Serialize, Clone, Default, Debug)]
pub struct Field {
    id: String,
    group_id: String,
    name: String,
    #[serde(rename = "type")]
    kind: String,
    attrs: Option<Map<String, Value>>,
    target_id: String,
    target_type: String,
    object_type: String,
    protected: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    permission_field: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    permission_values: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    permission_options: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    linked_field_id: Option<String>,
    create_at: i64,
    update_at: i64,
    delete_at: i64,
    created_by: String,
    updated_by: String,
}

impl Field {
    fn supports_options(&self) -> bool {
        OPTION_TYPES.contains(&self.kind.as_str())
    }
    fn is_v1(&self) -> bool {
        self.object_type.is_empty()
    }
    fn link_source(&self) -> &str {
        self.linked_field_id.as_deref().unwrap_or("")
    }
    fn is_linked(&self) -> bool {
        !self.link_source().is_empty()
    }
    /// The field's own id and the template it links to: whose options it serves.
    fn option_owners(&self) -> Vec<String> {
        let mut v = vec![self.id.clone()];
        if self.is_linked() {
            v.push(self.link_source().to_string());
        }
        v
    }
    fn attr(&self, k: &str) -> Option<&Value> {
        self.attrs.as_ref().and_then(|a| a.get(k))
    }
    fn options_omitted(&self) -> bool {
        self.attr("options_omitted").and_then(Value::as_bool).unwrap_or(false)
    }
    fn supplies_options(&self) -> bool {
        matches!(self.attr("options"), Some(Value::Array(a)) if !a.is_empty())
    }
    /// `GetAccessMode`: public unless `attrs.access_mode` says otherwise.
    fn public(&self) -> bool {
        self.attr("access_mode").and_then(Value::as_str).unwrap_or("").is_empty()
    }
}

/// `model.PropertyValue`; the value is the stored JSON as it is, compacted.
#[derive(Serialize)]
pub struct PropValue {
    id: String,
    target_id: String,
    target_type: String,
    group_id: String,
    field_id: String,
    value: Box<RawValue>,
    create_at: i64,
    update_at: i64,
    delete_at: i64,
    created_by: String,
    updated_by: String,
}

/// `model.PropertyFieldOption`.
#[derive(Serialize, Clone)]
struct FieldOption {
    id: String,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    color: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    attrs: Option<Map<String, Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parents: Option<Vec<String>>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    read_only: bool,
    #[serde(skip_serializing_if = "is_zero")]
    create_at: i64,
}

fn is_zero(n: &i64) -> bool {
    *n == 0
}

/// `model.PropertyFieldOptionPage`.
#[derive(Serialize)]
struct OptionPage {
    options: Vec<FieldOption>,
    has_more: bool,
    #[serde(skip_serializing_if = "is_zero")]
    next_cursor_create_at: i64,
    #[serde(skip_serializing_if = "String::is_empty")]
    next_cursor_id: String,
}

/// A row of `PropertyOptions` (`propertyOptionRow`).
#[derive(Clone)]
struct OptionRow {
    id: String,
    field_id: String,
    name: Option<String>,
    color: Option<String>,
    rank: Option<i64>,
    sort_order: i32,
    attrs: Option<Map<String, Value>>,
    create_at: i64,
    delete_at: i64,
}

impl OptionRow {
    /// `wireOption`: the flat object a field's `attrs.options` carries.
    fn wire(&self) -> Value {
        let mut m = self.attrs.clone().unwrap_or_default();
        m.insert("id".into(), Value::String(self.id.clone()));
        if let Some(n) = &self.name {
            m.insert("name".into(), Value::String(n.clone()));
        }
        if let Some(c) = &self.color {
            m.insert("color".into(), Value::String(c.clone()));
        }
        if let Some(r) = self.rank {
            m.insert("rank".into(), Value::from(r));
        }
        Value::Object(m)
    }
    /// `fieldOption`: as the options endpoints carry it.
    fn option(&self, asked: &str) -> FieldOption {
        FieldOption {
            id: self.id.clone(),
            name: self.name.clone().unwrap_or_default(),
            color: self.color.clone(),
            attrs: self.attrs.clone().filter(|a| !a.is_empty()),
            parents: None,
            read_only: self.field_id != asked,
            create_at: self.create_at,
        }
    }
}

// ---------- errors ----------

fn err(status: StatusCode, id: &str) -> AppError {
    AppError::new(status, id, id)
}
fn bad(id: &str) -> AppError {
    err(StatusCode::BAD_REQUEST, id)
}
fn license_error() -> AppError {
    err(StatusCode::FORBIDDEN, "app.property.license_error")
}
fn property_not_found() -> AppError {
    err(StatusCode::NOT_FOUND, "app.property.not_found.app_error")
}
fn object_type_mismatch() -> AppError {
    err(StatusCode::NOT_FOUND, "api.property_field.object_type_mismatch.app_error")
}
fn field_invalid(name: &str, reason: &str) -> AppError {
    bad("model.property_field.is_valid.app_error").with_params(&[("FieldName", name), ("Reason", reason)])
}
/// `optionsChangeRefused`.
fn options_refused(reason: &str) -> AppError {
    bad("app.property_field.options.invalid.app_error").with_param("Reason", reason).detail(reason)
}
/// `optionsChangeError`.
fn option_error(index: usize, reason: &str) -> AppError {
    bad("app.property_field.options.invalid_item.app_error")
        .with_params(&[("Index", &index.to_string()), ("Reason", reason)])
        .detail(format!("options[{index}] {reason}"))
}

// ---------- request plumbing ----------

type Params = HashMap<String, String>;
type Q = Query<HashMap<String, String>>;

fn param<'a>(p: &'a Params, k: &str) -> &'a str {
    p.get(k).map(String::as_str).unwrap_or("")
}

/// The patterns of Go's routes (`{group_name:[a-z][a-z0-9_]*}`, `{object_type:[a-z]+}`,
/// `{field_id:[A-Za-z0-9]+}`, `{target_id:[A-Za-z0-9]+}`): a path that fails them is the router's 404.
fn route_ok(p: &Params) -> bool {
    let g = param(p, "group_name").as_bytes();
    let group = !g.is_empty()
        && g[0].is_ascii_lowercase()
        && g.iter().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_');
    let alnum = |k: &str| p.get(k).is_none_or(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_alphanumeric()));
    let ot = p.get("object_type").is_none_or(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_lowercase()));
    group && ot && alnum("field_id") && alnum("target_id")
}

/// The router, then the session (`APISessionRequired`).
fn begin(uri: &Uri, p: &Params, auth: Result<Auth, AppError>) -> ApiResult<Arc<Session>> {
    if !route_ok(p) {
        return Err(crate::api::route_not_found(uri.path()));
    }
    Ok(auth?.0)
}

/// `RequireObjectType`.
fn require_object_type(p: &Params) -> ApiResult<String> {
    let ot = param(p, "object_type");
    if !OBJECT_TYPES.contains(&ot) {
        return Err(AppError::invalid_url_param("object_type"));
    }
    Ok(ot.to_string())
}

/// `RequireFieldId`.
fn require_field_id(p: &Params) -> ApiResult<String> {
    let id = param(p, "field_id");
    if !is_valid_id(id) {
        return Err(AppError::invalid_url_param("field_id"));
    }
    Ok(id.to_string())
}

/// `c.Params.PerPage`.
fn per_page(q: &HashMap<String, String>) -> i64 {
    match q.get("per_page").map(String::as_str).unwrap_or("").parse::<i64>() {
        Ok(v) if v >= 0 => v.min(PER_PAGE_MAXIMUM),
        _ => PER_PAGE_DEFAULT,
    }
}

/// `strconv.ParseInt` of an optional query parameter; `SetInvalidParamWithErr` when it is not a number.
fn query_i64(q: &HashMap<String, String>, k: &str) -> ApiResult<Option<i64>> {
    match q.get(k).map(String::as_str).unwrap_or("") {
        "" => Ok(None),
        v => v.parse::<i64>().map(Some).map_err(|_| {
            AppError::invalid_param(k).detail(format!("strconv.ParseInt: parsing \"{v}\": invalid syntax"))
        }),
    }
}

fn connection_id(h: &HeaderMap) -> String {
    h.get("Connection-Id").and_then(|v| v.to_str().ok()).unwrap_or("").to_string()
}

fn created(v: impl Serialize) -> Response {
    (StatusCode::CREATED, axum::Json(v)).into_response()
}

/// `json.NewDecoder(r.Body).Decode`: the first JSON value of the body.
fn first_json(body: &[u8]) -> Result<Value, String> {
    match serde_json::Deserializer::from_slice(body).into_iter::<Value>().next() {
        None => Err("EOF".into()),
        Some(r) => r.map_err(|e| e.to_string()),
    }
}

/// A Go struct field read from a decoded object: absent or `null` is the zero value, a value of another JSON
/// type is a decode error.
fn go_str(o: &Map<String, Value>, k: &str) -> Result<Option<String>, String> {
    match o.get(k) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(v) => Err(format!("json: cannot unmarshal {} into Go value of type string", kind(v))),
    }
}
fn go_i64(o: &Map<String, Value>, k: &str) -> Result<i64, String> {
    match o.get(k) {
        None | Some(Value::Null) => Ok(0),
        Some(Value::Number(n)) => {
            n.as_i64().ok_or_else(|| format!("json: cannot unmarshal number {n} into Go value of type int64"))
        }
        Some(v) => Err(format!("json: cannot unmarshal {} into Go value of type int64", kind(v))),
    }
}
fn go_bool(o: &Map<String, Value>, k: &str) -> Result<bool, String> {
    match o.get(k) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(b)) => Ok(*b),
        Some(v) => Err(format!("json: cannot unmarshal {} into Go value of type bool", kind(v))),
    }
}
fn go_strings(o: &Map<String, Value>, k: &str) -> Result<Vec<String>, String> {
    match o.get(k) {
        None | Some(Value::Null) => Ok(vec![]),
        Some(Value::Array(a)) => a
            .iter()
            .map(|v| match v {
                Value::String(s) => Ok(s.clone()),
                Value::Null => Ok(String::new()),
                v => Err(format!("json: cannot unmarshal {} into Go value of type string", kind(v))),
            })
            .collect(),
        Some(v) => Err(format!("json: cannot unmarshal {} into Go value of type []string", kind(v))),
    }
}
fn go_map(o: &Map<String, Value>, k: &str) -> Result<Option<Map<String, Value>>, String> {
    match o.get(k) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Object(m)) => Ok(Some(m.clone())),
        Some(v) => Err(format!("json: cannot unmarshal {} into Go value of type model.StringInterface", kind(v))),
    }
}
fn kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// A body that must decode as a Go struct: the object, or `None` for `null`.
fn body_object(body: &[u8], name: &str) -> ApiResult<Option<Map<String, Value>>> {
    match first_json(body) {
        Ok(Value::Object(o)) => Ok(Some(o)),
        Ok(Value::Null) => Ok(None),
        Ok(v) => Err(AppError::invalid_param(name)
            .detail(format!("json: cannot unmarshal {} into Go value of type struct", kind(&v)))),
        Err(e) => Err(AppError::invalid_param(name).detail(e)),
    }
}

/// A body that must decode as a Go slice: its items, none for `null`.
fn body_array(body: &[u8], name: &str) -> ApiResult<Vec<Value>> {
    match first_json(body) {
        Ok(Value::Array(a)) => Ok(a),
        Ok(Value::Null) => Ok(vec![]),
        Ok(v) => Err(AppError::invalid_param(name)
            .detail(format!("json: cannot unmarshal {} into Go value of type slice", kind(&v)))),
        Err(e) => Err(AppError::invalid_param(name).detail(e)),
    }
}

/// jsonb's text without the whitespace between tokens, as `encoding/json` writes a `json.RawMessage`.
fn compact(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let (mut in_str, mut esc) = (false, false);
    for c in s.chars() {
        if in_str {
            out.push(c);
            if esc {
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == '"' {
                in_str = false;
            }
        } else if c == '"' {
            in_str = true;
            out.push(c);
        } else if !c.is_whitespace() {
            out.push(c);
        }
    }
    out
}

fn raw(s: &str) -> Box<RawValue> {
    RawValue::from_string(compact(s)).unwrap_or_else(|_| RawValue::from_string("null".into()).unwrap())
}

// ---------- store ----------

/// Bind arguments of a query built at run time.
enum Arg {
    S(String),
    I(i64),
    A(Vec<String>),
}

struct Sql {
    sql: String,
    args: Vec<Arg>,
}

impl Sql {
    fn new(s: &str) -> Sql {
        Sql { sql: s.to_string(), args: vec![] }
    }
    fn arg(&mut self, a: Arg) -> String {
        self.args.push(a);
        format!("${}", self.args.len())
    }
    async fn fetch_all(self, app: &App) -> ApiResult<Vec<PgRow>> {
        let mut q = sqlx::query(sqlx::AssertSqlSafe(self.sql));
        for a in self.args {
            q = match a {
                Arg::S(s) => q.bind(s),
                Arg::I(i) => q.bind(i),
                Arg::A(v) => q.bind(v),
            };
        }
        Ok(q.fetch_all(&app.db).await?)
    }
}

const FIELD_COLS: &str = "ID, GroupID, Name, Type::text AS Type, Attrs::text AS Attrs, COALESCE(TargetID, '') AS TargetID, \
    COALESCE(TargetType, '') AS TargetType, ObjectType, Protected, PermissionField::text AS PermissionField, \
    PermissionValues::text AS PermissionValues, PermissionOptions::text AS PermissionOptions, LinkedFieldID, CreateAt, \
    UpdateAt, DeleteAt, COALESCE(CreatedBy, '') AS CreatedBy, COALESCE(UpdatedBy, '') AS UpdatedBy";

fn parse_attrs(s: Option<String>) -> Option<Map<String, Value>> {
    match s.and_then(|s| serde_json::from_str::<Value>(&s).ok()) {
        Some(Value::Object(m)) => Some(m),
        _ => None,
    }
}

fn field_from_row(r: &PgRow) -> ApiResult<Field> {
    Ok(Field {
        id: r.try_get("id")?,
        group_id: r.try_get("groupid")?,
        name: r.try_get("name")?,
        kind: r.try_get::<Option<String>, _>("type")?.unwrap_or_default(),
        attrs: parse_attrs(r.try_get("attrs")?),
        target_id: r.try_get("targetid")?,
        target_type: r.try_get("targettype")?,
        object_type: r.try_get("objecttype")?,
        protected: r.try_get("protected")?,
        permission_field: r.try_get("permissionfield")?,
        permission_values: r.try_get("permissionvalues")?,
        permission_options: r.try_get("permissionoptions")?,
        linked_field_id: r.try_get("linkedfieldid")?,
        create_at: r.try_get("createat")?,
        update_at: r.try_get("updateat")?,
        delete_at: r.try_get("deleteat")?,
        created_by: r.try_get("createdby")?,
        updated_by: r.try_get("updatedby")?,
    })
}

const VALUE_COLS: &str = "ID, TargetID, TargetType, GroupID, FieldID, Value::text AS Value, CreateAt, UpdateAt, DeleteAt, \
    COALESCE(CreatedBy, '') AS CreatedBy, COALESCE(UpdatedBy, '') AS UpdatedBy";

fn value_from_row(r: &PgRow) -> ApiResult<PropValue> {
    Ok(PropValue {
        id: r.try_get("id")?,
        target_id: r.try_get("targetid")?,
        target_type: r.try_get("targettype")?,
        group_id: r.try_get("groupid")?,
        field_id: r.try_get("fieldid")?,
        value: raw(&r.try_get::<String, _>("value")?),
        create_at: r.try_get("createat")?,
        update_at: r.try_get("updateat")?,
        delete_at: r.try_get("deleteat")?,
        created_by: r.try_get("createdby")?,
        updated_by: r.try_get("updatedby")?,
    })
}

const OPTION_COLS: &str = "ID, FieldID, Name, Color, Rank, SortOrder, Attrs::text AS Attrs, CreateAt, DeleteAt";

fn option_from_row(r: &PgRow) -> ApiResult<OptionRow> {
    Ok(OptionRow {
        id: r.try_get("id")?,
        field_id: r.try_get("fieldid")?,
        name: r.try_get("name")?,
        color: r.try_get("color")?,
        rank: r.try_get("rank")?,
        sort_order: r.try_get("sortorder")?,
        attrs: parse_attrs(r.try_get("attrs")?),
        create_at: r.try_get("createat")?,
        delete_at: r.try_get("deleteat")?,
    })
}

impl App {
    /// `PropertyGroupStore.Get` through `App.GetPropertyGroup`.
    async fn property_group(&self, name: &str) -> ApiResult<Group> {
        let r = sqlx::query("SELECT ID, Name, Version FROM PropertyGroups WHERE Name = $1")
            .bind(name)
            .fetch_optional(&self.db)
            .await?
            .ok_or_else(|| err(StatusCode::NOT_FOUND, "app.property_group.get.app_error"))?;
        Ok(Group { id: r.try_get("id")?, name: r.try_get("name")?, version: r.try_get("version")? })
    }

    /// `getV2Group`: a PSAv2 group, and session attributes only with Enterprise Advanced.
    async fn v2_group(&self, name: &str) -> ApiResult<Group> {
        let g = self.property_group(name).await?;
        if g.version != 2 {
            return Err(err(StatusCode::NOT_FOUND, "api.property.v2_group_not_found.app_error"));
        }
        if g.name == SESSION_ATTRIBUTES {
            return Err(err(StatusCode::NOT_IMPLEMENTED, "api.property.session_attributes.license.app_error"));
        }
        Ok(g)
    }

    /// `hydratePropertyFieldOptions`: the options of option-bearing fields into `attrs.options`, or their count when
    /// there are too many to carry inline.
    async fn hydrate_options(&self, fields: &mut [Field]) -> ApiResult<()> {
        let mut owners: Vec<String> = vec![];
        for f in fields.iter().filter(|f| f.supports_options()) {
            for o in f.option_owners() {
                if !owners.contains(&o) {
                    owners.push(o);
                }
            }
        }
        if owners.is_empty() {
            return Ok(());
        }
        let counts: HashMap<String, i64> = sqlx::query_as::<_, (String, i64)>(
            "SELECT FieldID, COUNT(ID) FROM PropertyOptions WHERE FieldID = ANY($1) AND DeleteAt = 0 GROUP BY FieldID",
        )
        .bind(&owners)
        .fetch_all(&self.db)
        .await?
        .into_iter()
        .collect();
        let total = |f: &Field| f.option_owners().iter().map(|o| counts.get(o).copied().unwrap_or(0)).sum::<i64>();
        let mut load: Vec<String> = vec![];
        for f in fields.iter().filter(|f| f.supports_options()) {
            let t = total(f);
            if t == 0 || t > MAX_HYDRATED_OPTIONS {
                continue;
            }
            for o in f.option_owners() {
                if !load.contains(&o) {
                    load.push(o);
                }
            }
        }
        let mut by_owner: HashMap<String, Vec<OptionRow>> = HashMap::new();
        if !load.is_empty() {
            let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
                "SELECT {OPTION_COLS} FROM PropertyOptions WHERE FieldID = ANY($1) AND DeleteAt = 0"
            )))
            .bind(&load)
            .fetch_all(&self.db)
            .await?;
            for r in &rows {
                let o = option_from_row(r)?;
                by_owner.entry(o.field_id.clone()).or_default().push(o);
            }
        }
        for f in fields.iter_mut().filter(|f| f.supports_options()) {
            let t = total(f);
            if let Some(a) = f.attrs.as_mut() {
                a.remove("options");
                a.remove("options_count");
                a.remove("options_omitted");
            }
            if t == 0 {
                continue;
            }
            let a = f.attrs.get_or_insert_with(Map::new);
            if t > MAX_HYDRATED_OPTIONS {
                a.insert("options_count".into(), Value::from(t));
                a.insert("options_omitted".into(), Value::Bool(true));
                continue;
            }
            let mut rows: Vec<&OptionRow> = vec![];
            for o in f.option_owners() {
                rows.extend(by_owner.get(&o).into_iter().flatten());
            }
            rows.sort_by(|a, b| (a.sort_order, a.create_at, &a.id).cmp(&(b.sort_order, b.create_at, &b.id)));
            let opts: Vec<Value> = rows.iter().map(|r| r.wire()).collect();
            f.attrs.get_or_insert_with(Map::new).insert("options".into(), Value::Array(opts));
        }
        Ok(())
    }

    /// `PropertyFieldStore.Get` (any group when `group_id` is empty), hydrated.
    async fn property_field_row(&self, group_id: &str, id: &str) -> ApiResult<Option<Field>> {
        let mut q = Sql::new(&format!("SELECT {FIELD_COLS} FROM PropertyFields WHERE ID = $1"));
        q.args.push(Arg::S(id.into()));
        if !group_id.is_empty() {
            let a = q.arg(Arg::S(group_id.into()));
            q.sql += &format!(" AND GroupID = {a}");
        }
        let Some(r) = q.fetch_all(self).await?.into_iter().next() else { return Ok(None) };
        let mut f = vec![field_from_row(&r)?];
        self.hydrate_options(&mut f).await?;
        Ok(f.pop())
    }

    /// `App.GetPropertyField`: the field, refused without a license in a licensed group.
    async fn get_property_field(&self, group: &Group, id: &str) -> ApiResult<Field> {
        let f = self.property_field_row(&group.id, id).await?.ok_or_else(property_not_found)?;
        if group.name == ACCESS_CONTROL {
            return Err(license_error());
        }
        Ok(f)
    }

    /// `PropertyFieldStore.GetMany`: every field asked for, deleted or not, or `ErrFieldNotFound`.
    async fn property_fields_many(&self, group_id: &str, ids: &[String]) -> ApiResult<Vec<Field>> {
        let mut q = Sql::new(&format!("SELECT {FIELD_COLS} FROM PropertyFields WHERE ID = ANY($1)"));
        q.args.push(Arg::A(ids.to_vec()));
        if !group_id.is_empty() {
            let a = q.arg(Arg::S(group_id.into()));
            q.sql += &format!(" AND GroupID = {a}");
        }
        let rows = q.fetch_all(self).await?;
        let mut fields = rows.iter().map(field_from_row).collect::<ApiResult<Vec<_>>>()?;
        if fields.len() < ids.len() {
            return Err(err(StatusCode::NOT_FOUND, "app.property_field.not_found.app_error"));
        }
        self.hydrate_options(&mut fields).await?;
        Ok(fields)
    }

    /// `CountLinkedFields`.
    async fn count_linked_fields(&self, id: &str) -> ApiResult<i64> {
        Ok(sqlx::query_scalar("SELECT COUNT(ID) FROM PropertyFields WHERE LinkedFieldID = $1 AND DeleteAt = 0")
            .bind(id)
            .fetch_one(&self.db)
            .await?)
    }

    /// `CheckPropertyNameConflict`: the level of a live field of that name the new one would shadow or be shadowed
    /// by, or none.
    async fn name_conflict(&self, f: &Field, exclude: &str) -> ApiResult<String> {
        if f.is_v1() {
            return Ok(String::new());
        }
        let sub = |level: &str, extra: &str| {
            format!(
                "(SELECT '{level}' FROM PropertyFields pf {extra} WHERE pf.ObjectType = $1 AND pf.GroupID = $2 AND \
                 pf.TargetType = '{level}' AND pf.Name = $3 AND pf.DeleteAt = 0 AND pf.ID <> $4{} LIMIT 1)",
                match (level, f.target_type.as_str()) {
                    ("team", "team") | ("channel", "channel") => " AND pf.TargetID = $5",
                    ("team", "channel") => " AND pf.TargetID = (SELECT TeamId FROM Channels WHERE Id = $5)",
                    _ => "",
                }
            )
        };
        let (a, b, c) = match f.target_type.as_str() {
            "system" => (sub("system", ""), sub("team", ""), sub("channel", "")),
            "team" => (
                sub("team", ""),
                sub("system", ""),
                sub("channel", "JOIN Channels c ON c.Id = pf.TargetID AND c.TeamId = $5"),
            ),
            "channel" => (sub("channel", ""), sub("system", ""), sub("team", "")),
            _ => return Ok(String::new()),
        };
        let sql = format!("SELECT COALESCE({a}, {b}, {c}, '') WHERE $5::text IS NOT NULL OR TRUE");
        let level: Option<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
            .bind(&f.object_type)
            .bind(&f.group_id)
            .bind(&f.name)
            .bind(exclude)
            .bind(&f.target_id)
            .fetch_optional(&self.db)
            .await?;
        Ok(level.unwrap_or_default())
    }
}

// ---------- permissions (app/authorization.go) ----------

impl App {
    async fn sess_channel(&self, s: &Session, channel_id: &str, perm: &str) -> bool {
        self.channel_perm(s, channel_id, perm).await.map(|p| p.0).unwrap_or(false)
    }

    /// `hasChannelPropertyAdmin`.
    async fn channel_property_admin(&self, user_id: &str, channel_id: &str) -> bool {
        if self.user_channel_perm(user_id, channel_id, "manage_channel_roles").await.0 {
            return true;
        }
        let Ok(Some(c)) = self.channel(channel_id).await else { return false };
        if c.r#type != "D" && c.r#type != "G" {
            return false;
        }
        if !self.user_channel_perm(user_id, channel_id, "read_channel").await.1 {
            return false;
        }
        matches!(self.user(user_id).await, Ok(Some(u)) if !u.is_guest())
    }

    /// `hasPropertyFieldScopeAccess`.
    async fn field_scope_access(&self, user_id: &str, f: &Field) -> bool {
        match f.target_type.as_str() {
            "system" => true,
            "team" => self.user_team_perm(user_id, &f.target_id, "view_team").await,
            "channel" => self.user_channel_perm(user_id, &f.target_id, "read_channel").await.0,
            _ => false,
        }
    }

    /// `hasPropertyFieldPermissionLevel`.
    async fn field_level(&self, user_id: &str, f: &Field, level: &str) -> bool {
        match level {
            "sysadmin" => self.user_has_perm(user_id, MANAGE_SYSTEM).await,
            "member" => self.field_scope_access(user_id, f).await,
            "admin" => self.field_admin(user_id, f).await,
            "creator" => {
                (!f.created_by.is_empty() && f.created_by == user_id && self.field_scope_access(user_id, f).await)
                    || self.field_admin(user_id, f).await
            }
            _ => false,
        }
    }

    async fn field_admin(&self, user_id: &str, f: &Field) -> bool {
        match f.target_type.as_str() {
            "system" => self.user_has_perm(user_id, MANAGE_SYSTEM).await,
            "team" => self.user_team_perm(user_id, &f.target_id, "manage_team").await,
            "channel" => self.channel_property_admin(user_id, &f.target_id).await,
            _ => false,
        }
    }

    /// `SessionHasPermissionToEditPropertyField`.
    async fn can_edit_field(&self, s: &Session, f: &Field) -> bool {
        if f.protected {
            return false;
        }
        match &f.permission_field {
            Some(l) => self.field_level(&s.user_id, f, l).await,
            None => false,
        }
    }

    /// `SessionHasPermissionToManagePropertyFieldOptions`.
    async fn can_manage_options(&self, s: &Session, f: &Field) -> bool {
        match &f.permission_options {
            Some(l) => self.field_level(&s.user_id, f, l).await,
            None => false,
        }
    }

    /// `post.ChannelId` of a live post, for the value permission checks (`Post().GetSingle`).
    async fn post_channel(&self, post_id: &str) -> Option<(String, String)> {
        self.get_single_post(post_id, false).await.ok().flatten().map(|p| (p.channel_id, p.user_id))
    }

    /// `hasPropertyFieldValueAdmin`.
    async fn value_admin(&self, user_id: &str, f: &Field, target: &str) -> bool {
        match f.object_type.as_str() {
            "channel" => self.channel_property_admin(user_id, target).await,
            "post" => match self.post_channel(target).await {
                Some((c, _)) => self.channel_property_admin(user_id, &c).await,
                None => false,
            },
            "user" | "system" | "template" => self.field_admin(user_id, f).await,
            _ => false,
        }
    }

    /// `SessionHasPermissionToSetPropertyFieldValues`.
    async fn can_set_values(&self, s: &Session, f: &Field, target: &str) -> bool {
        let Some(level) = &f.permission_values else { return false };
        let u = &s.user_id;
        match level.as_str() {
            "sysadmin" => self.user_has_perm(u, MANAGE_SYSTEM).await,
            "admin" => self.value_admin(u, f, target).await,
            "creator" => match f.object_type.as_str() {
                "post" => match self.post_channel(target).await {
                    Some((_, author)) if !author.is_empty() && author == *u => true,
                    Some(_) => self.value_admin(u, f, target).await,
                    None => false,
                },
                "channel" => match self.channel(target).await {
                    Ok(Some(c)) if !c.creator_id.is_empty() && c.creator_id == *u => true,
                    Ok(Some(_)) => self.value_admin(u, f, target).await,
                    _ => false,
                },
                _ => false,
            },
            "member" => match f.object_type.as_str() {
                "channel" => self.user_channel_perm(u, target, "read_channel").await.0,
                "post" => match self.post_channel(target).await {
                    Some((c, _)) => self.user_channel_perm(u, &c, "read_channel").await.0,
                    None => false,
                },
                "user" | "system" | "template" => self.field_scope_access(u, f).await,
                _ => false,
            },
            _ => false,
        }
    }
}

/// `DefaultPropertyFieldPermissionLevel`.
fn default_level(f: &Field) -> &'static str {
    if f.object_type == "template" || f.object_type == "system" || f.target_type == "system" {
        "sysadmin"
    } else {
        "member"
    }
}

/// `DefaultPropertyFieldValuesPermissionLevel`.
fn default_values_level(f: &Field) -> &'static str {
    if f.object_type == "template" || f.object_type == "system" { "sysadmin" } else { "member" }
}

/// `CanonicalizeSystemObjectField`.
fn canonicalize_system(f: &mut Field) {
    if f.object_type != "system" {
        return;
    }
    f.target_type = "system".into();
    f.target_id.clear();
    f.permission_field = Some("sysadmin".into());
    f.permission_values = Some("sysadmin".into());
    f.permission_options = Some("sysadmin".into());
}

/// `requireChannelAttributeLicense`: channel objects of the access control group need Enterprise Advanced.
fn require_channel_attribute_license(group: &Group, object_types: &[String]) -> ApiResult<()> {
    if group.name == ACCESS_CONTROL && object_types.iter().any(|o| o == "channel") {
        return Err(err(StatusCode::NOT_IMPLEMENTED, "api.property.channel_attributes.license.app_error"));
    }
    Ok(())
}

// ---------- field search ----------

#[derive(Default)]
struct SearchOpts {
    object_types: Vec<String>,
    target_type: String,
    target_ids: Vec<String>,
    channel_id: String,
    team_id: String,
    since: i64,
    cursor_id: String,
    cursor_create_at: i64,
    cursor_update_at: i64,
    per_page: i64,
}

impl SearchOpts {
    fn cursor_empty(&self) -> bool {
        self.cursor_id.is_empty() && self.cursor_create_at == 0 && self.cursor_update_at == 0
    }
    /// `PropertyFieldSearchCursor.IsValid`.
    fn cursor_err(&self) -> Option<&'static str> {
        if self.cursor_empty() {
            return None;
        }
        if !is_valid_id(&self.cursor_id) {
            return Some("property field id is invalid");
        }
        if (self.cursor_create_at > 0) == (self.cursor_update_at > 0) {
            return Some("cursor must have exactly one of create_at or update_at set");
        }
        None
    }
    /// `PropertyFieldSearchOpts.IsValid`.
    fn invalid(&self) -> Option<String> {
        for ot in &self.object_types {
            if !OBJECT_TYPES.contains(&ot.as_str()) {
                return Some(format!("invalid object_type {ot:?}"));
            }
        }
        if (!self.channel_id.is_empty() || !self.team_id.is_empty())
            && (!self.target_type.is_empty() || !self.target_ids.is_empty())
        {
            return Some("channel_id/team_id cannot be combined with target_type/target_id".into());
        }
        if let Some(e) = self.cursor_err() {
            return Some(e.into());
        }
        if !self.cursor_empty() {
            if self.since > 0 && self.cursor_update_at == 0 {
                return Some("cursor_update_at required when since is set".into());
            }
            if self.since <= 0 && self.cursor_create_at == 0 {
                return Some("cursor_create_at required when since is not set".into());
            }
        }
        None
    }
}

/// `resolveScopeAndCheckPermissions`.
async fn resolve_scope(app: &App, s: &Session, o: &mut SearchOpts) -> ApiResult<()> {
    let by_chan_team = !o.channel_id.is_empty() || !o.team_id.is_empty();
    let by_target = !o.target_type.is_empty() || !o.target_ids.is_empty();
    if by_chan_team && by_target {
        return Err(bad("api.property_field.get.scope_conflict.app_error"));
    }
    if !o.channel_id.is_empty() {
        if !app.sess_channel(s, &o.channel_id, "read_channel").await {
            return Err(AppError::forbidden_perm("read_channel"));
        }
        let c = app.get_channel(&o.channel_id).await?;
        o.channel_id = c.id.clone();
        o.team_id = c.team_id.clone();
    } else if !o.team_id.is_empty() {
        if !app.has_team_perm(s, &o.team_id, "view_team").await {
            return Err(AppError::forbidden_perm("view_team"));
        }
    } else if !o.target_type.is_empty() {
        if !TARGET_TYPES.contains(&o.target_type.as_str()) {
            return Err(bad("api.property_field.get.invalid_target_type.app_error"));
        }
        match o.target_type.as_str() {
            "channel" => {
                let Some(t) = o.target_ids.first() else {
                    return Err(bad("api.property_field.get.target_id_required.app_error"));
                };
                if !app.sess_channel(s, t, "read_channel").await {
                    return Err(AppError::forbidden_perm("read_channel"));
                }
            }
            "team" => {
                let Some(t) = o.target_ids.first() else {
                    return Err(bad("api.property_field.get.target_id_required.app_error"));
                };
                if !app.has_team_perm(s, t, "view_team").await {
                    return Err(AppError::forbidden_perm("view_team"));
                }
            }
            _ => {}
        }
    } else if !o.target_ids.is_empty() {
        return Err(bad("api.property_field.get.target_type_required.app_error"));
    } else {
        return Err(bad("api.property_field.get.scope_required.app_error"));
    }
    Ok(())
}

/// `searchPropertyFieldsCore`.
async fn search_core(app: &App, s: &Session, group: &Group, mut o: SearchOpts) -> ApiResult<Response> {
    if o.object_types.len() == 1 && o.object_types[0] == "system" {
        o.channel_id.clear();
        o.team_id.clear();
        o.target_ids.clear();
        o.target_type = "system".into();
    }
    require_channel_attribute_license(group, &o.object_types)?;
    resolve_scope(app, s, &mut o).await?;
    if let Some(e) = o.invalid() {
        return Err(bad("api.property_field.get.invalid_opts.app_error").detail(e));
    }
    if o.per_page < 1 {
        return Err(AppError::internal(
            "app.property_field.search.app_error",
            "per page must be positive integer greater than zero",
        ));
    }
    // SqlPropertyFieldStore.SearchPropertyFields
    let delta = o.since > 0;
    let mut q = Sql::new(&format!("SELECT {FIELD_COLS} FROM PropertyFields WHERE GroupID = $1"));
    q.args.push(Arg::S(group.id.clone()));
    if !o.cursor_empty() {
        let (col, at) = if delta { ("UpdateAt", o.cursor_update_at) } else { ("CreateAt", o.cursor_create_at) };
        let a = q.arg(Arg::I(at));
        let b = q.arg(Arg::S(o.cursor_id.clone()));
        q.sql += &format!(" AND ({col} > {a} OR ({col} = {a} AND Id > {b}))");
    }
    if !delta {
        q.sql += " AND DeleteAt = 0";
    }
    if !o.object_types.is_empty() {
        let a = q.arg(Arg::A(o.object_types.clone()));
        q.sql += &format!(" AND ObjectType = ANY({a})");
    }
    if !o.channel_id.is_empty() && !o.team_id.is_empty() {
        let t = q.arg(Arg::S(o.team_id.clone()));
        let c = q.arg(Arg::S(o.channel_id.clone()));
        q.sql += &format!(
            " AND (TargetType = 'system' OR (TargetType = 'team' AND TargetID = {t}) OR (TargetType = 'channel' AND TargetID = {c}))"
        );
    } else if !o.channel_id.is_empty() {
        let c = q.arg(Arg::S(o.channel_id.clone()));
        q.sql += &format!(" AND (TargetType = 'system' OR (TargetType = 'channel' AND TargetID = {c}))");
    } else if !o.team_id.is_empty() {
        let t = q.arg(Arg::S(o.team_id.clone()));
        q.sql += &format!(" AND (TargetType = 'system' OR (TargetType = 'team' AND TargetID = {t}))");
    } else {
        if !o.target_type.is_empty() {
            let a = q.arg(Arg::S(o.target_type.clone()));
            q.sql += &format!(" AND TargetType = {a}");
        }
        if !o.target_ids.is_empty() {
            let a = q.arg(Arg::A(o.target_ids.clone()));
            q.sql += &format!(" AND TargetID = ANY({a})");
        }
    }
    if delta {
        let a = q.arg(Arg::I(o.since));
        q.sql += &format!(" AND UpdateAt >= {a}");
    }
    q.sql += if delta { " ORDER BY UpdateAt ASC, Id ASC" } else { " ORDER BY CreateAt ASC, Id ASC" };
    q.sql += &format!(" LIMIT {}", o.per_page);
    let rows = q.fetch_all(app).await?;
    let mut fields = rows.iter().map(field_from_row).collect::<ApiResult<Vec<_>>>()?;
    app.hydrate_options(&mut fields).await?;
    // LicenseCheckHook.PostGetPropertyFields
    if group.name == ACCESS_CONTROL && !fields.is_empty() {
        return Err(license_error());
    }
    Ok(axum::Json(fields).into_response())
}

/// `getPropertyFields`.
async fn get_fields(
    St(app): St<State>,
    uri: Uri,
    Path(p): Path<Params>,
    Query(q): Q,
    auth: Result<Auth, AppError>,
) -> ApiResult<Response> {
    let s = begin(&uri, &p, auth)?;
    let ot = require_object_type(&p)?;
    let group = app.v2_group(param(&p, "group_name")).await?;
    let mut o = SearchOpts { object_types: vec![ot], per_page: per_page(&q), ..Default::default() };
    if let Some(v) = query_i64(&q, "since")? {
        o.since = v;
    }
    if let Some(c) = q.get("cursor_id").filter(|c| !c.is_empty()) {
        o.cursor_id = c.clone();
        o.cursor_update_at = query_i64(&q, "cursor_update_at")?.unwrap_or(0);
        o.cursor_create_at = query_i64(&q, "cursor_create_at")?.unwrap_or(0);
        if let Some(e) = o.cursor_err() {
            return Err(AppError::invalid_param("cursor").detail(e));
        }
    }
    let get = |k: &str| q.get(k).cloned().unwrap_or_default();
    o.channel_id = get("channel_id");
    o.team_id = get("team_id");
    o.target_type = get("target_type");
    if let Some(t) = q.get("target_id").filter(|t| !t.is_empty()) {
        o.target_ids = vec![t.clone()];
    }
    search_core(&app, &s, &group, o).await
}

/// `searchPropertyFields`.
async fn search_fields(
    St(app): St<State>,
    uri: Uri,
    Path(p): Path<Params>,
    auth: Result<Auth, AppError>,
    body: Bytes,
) -> ApiResult<Response> {
    let s = begin(&uri, &p, auth)?;
    let group = app.v2_group(param(&p, "group_name")).await?;
    let b = body_object(&body, "property_field_search")?.unwrap_or_default();
    let decode = || -> Result<SearchOpts, String> {
        Ok(SearchOpts {
            object_types: go_strings(&b, "object_types")?,
            target_type: go_str(&b, "target_type")?.unwrap_or_default(),
            target_ids: go_str(&b, "target_id")?.filter(|t| !t.is_empty()).into_iter().collect(),
            channel_id: go_str(&b, "channel_id")?.unwrap_or_default(),
            team_id: go_str(&b, "team_id")?.unwrap_or_default(),
            since: go_i64(&b, "since")?,
            cursor_id: go_str(&b, "cursor_id")?.unwrap_or_default(),
            cursor_create_at: go_i64(&b, "cursor_create_at")?,
            cursor_update_at: go_i64(&b, "cursor_update_at")?,
            per_page: go_i64(&b, "per_page")?,
        })
    };
    let mut o = decode().map_err(|e| AppError::invalid_param("property_field_search").detail(e))?;
    if o.object_types.is_empty() || o.object_types.iter().any(|t| !OBJECT_TYPES.contains(&t.as_str())) {
        return Err(AppError::invalid_param("object_types"));
    }
    if o.cursor_id.is_empty() {
        o.cursor_create_at = 0;
        o.cursor_update_at = 0;
    }
    o.per_page = if o.per_page <= 0 { PER_PAGE_DEFAULT } else { o.per_page.min(PER_PAGE_MAXIMUM) };
    search_core(&app, &s, &group, o).await
}

// ---------- field writes ----------

/// A `model.PropertyField` decoded from a request body.
fn decode_field(o: &Map<String, Value>) -> Result<Field, String> {
    Ok(Field {
        id: go_str(o, "id")?.unwrap_or_default(),
        group_id: go_str(o, "group_id")?.unwrap_or_default(),
        name: go_str(o, "name")?.unwrap_or_default(),
        kind: go_str(o, "type")?.unwrap_or_default(),
        attrs: go_map(o, "attrs")?,
        target_id: go_str(o, "target_id")?.unwrap_or_default(),
        target_type: go_str(o, "target_type")?.unwrap_or_default(),
        object_type: go_str(o, "object_type")?.unwrap_or_default(),
        protected: go_bool(o, "protected")?,
        permission_field: go_str(o, "permission_field")?,
        permission_values: go_str(o, "permission_values")?,
        permission_options: go_str(o, "permission_options")?,
        linked_field_id: go_str(o, "linked_field_id")?,
        create_at: go_i64(o, "create_at")?,
        update_at: go_i64(o, "update_at")?,
        delete_at: go_i64(o, "delete_at")?,
        created_by: go_str(o, "created_by")?.unwrap_or_default(),
        updated_by: go_str(o, "updated_by")?.unwrap_or_default(),
    })
}

/// `PropertyField.IsValid` (PSAv2 fields only reach it here).
fn validate_field(f: &Field) -> ApiResult<()> {
    let n = |s: &str| s.chars().count();
    if !is_valid_id(&f.id) {
        return Err(field_invalid("id", "invalid id"));
    }
    let with_id = |e: AppError| e.detail(format!("id={}", f.id));
    if !is_valid_id(&f.group_id) {
        return Err(with_id(field_invalid("group_id", "invalid id")));
    }
    if f.name.is_empty() {
        return Err(with_id(field_invalid("name", "value cannot be empty")));
    }
    for (k, v) in [
        ("name", &f.name),
        ("target_type", &f.target_type),
        ("target_id", &f.target_id),
        ("object_type", &f.object_type),
    ] {
        if n(v) > 255 {
            return Err(with_id(field_invalid(k, "value exceeds maximum length")));
        }
    }
    if !OBJECT_TYPES.contains(&f.object_type.as_str()) {
        return Err(with_id(field_invalid("object_type", "unknown value")));
    }
    if !TARGET_TYPES.contains(&f.target_type.as_str()) {
        return Err(with_id(field_invalid("target_type", "unknown value")));
    }
    match f.target_type.as_str() {
        "system" if !f.target_id.is_empty() => {
            return Err(with_id(field_invalid("target_id", "must be empty for system target type")));
        }
        "team" | "channel" if !is_valid_id(&f.target_id) => {
            return Err(with_id(field_invalid("target_id", "must be a valid ID for team or channel target type")));
        }
        _ => {}
    }
    if f.object_type == "system" && f.target_type != "system" {
        return Err(with_id(field_invalid("target_type", "must be system for system object type")));
    }
    if !FIELD_TYPES.contains(&f.kind.as_str()) {
        return Err(with_id(field_invalid("type", "unknown value")));
    }
    if f.kind == "graph" {
        let opts = inline_options_lenient(f);
        if opts.len() > GRAPH_MAX_OPTIONS {
            return Err(with_id(field_invalid(
                "attrs.options",
                &format!("a graph field cannot have more than {GRAPH_MAX_OPTIONS} options"),
            )));
        }
        if let Some(i) = opts.iter().position(|o| o.get("rank").is_some_and(|r| !r.is_null())) {
            return Err(with_id(field_invalid(
                &format!("attrs.options[{i}].rank"),
                "rank is not supported on a graph field",
            )));
        }
        let mut seen = HashSet::new();
        for (i, o) in opts.iter().enumerate() {
            let name = o.get("name").and_then(Value::as_str).unwrap_or("");
            if !name.is_empty() && !seen.insert(name.to_string()) {
                return Err(with_id(field_invalid(
                    &format!("attrs.options[{i}].name"),
                    &format!("two options are called {name:?}, and a name has to identify one option"),
                )));
            }
        }
    }
    if f.is_linked() && !is_valid_id(f.link_source()) {
        return Err(with_id(field_invalid("linked_field_id", "invalid id")));
    }
    if f.object_type == "template" && f.is_linked() {
        return Err(with_id(field_invalid("linked_field_id", "template fields cannot have a linked field")));
    }
    if f.create_at == 0 {
        return Err(with_id(field_invalid("create_at", "value cannot be zero")));
    }
    if f.update_at == 0 {
        return Err(with_id(field_invalid("update_at", "value cannot be zero")));
    }
    let slots = [
        ("permission_field", &f.permission_field),
        ("permission_values", &f.permission_values),
        ("permission_options", &f.permission_options),
    ];
    for (k, v) in slots {
        if v.as_ref().is_some_and(|l| !PERMISSION_LEVELS.contains(&l.as_str())) {
            return Err(with_id(field_invalid(k, "invalid permission level")));
        }
    }
    if f.object_type != "post" && f.object_type != "channel" {
        for (k, v) in slots {
            if v.as_deref() == Some("creator") {
                return Err(with_id(field_invalid(
                    k,
                    "creator permission level requires a post or channel object type",
                )));
            }
        }
    }
    if f.protected {
        match f.permission_field.as_deref() {
            None => {
                return Err(with_id(field_invalid(
                    "permission_field",
                    "protected fields must have explicit permissions with field set to none",
                )));
            }
            Some(l) if l != "none" => {
                return Err(with_id(field_invalid(
                    "permission_field",
                    "protected fields must have field permission set to none",
                )));
            }
            _ => {}
        }
    }
    if !f.protected && f.permission_field.as_deref() == Some("none") {
        return Err(with_id(field_invalid(
            "permission_field",
            "non-protected fields cannot have field permission set to none",
        )));
    }
    Ok(())
}

/// `decodedInlineOptions`: the option objects of `attrs.options`, whatever else it holds.
fn inline_options_lenient(f: &Field) -> Vec<Map<String, Value>> {
    match f.attr("options") {
        Some(Value::Array(a)) => a.iter().filter_map(|o| o.as_object().cloned()).collect(),
        _ => vec![],
    }
}

/// `EnsureOptionIDs`: every inline option an object with an id; a list in another shape is a server error under
/// `fail`.
fn ensure_option_ids(f: &mut Field, fail: &str) -> ApiResult<()> {
    if f.supports_options()
        && let Some(raw) = f.attrs.as_mut().and_then(|a| a.get_mut("options"))
        && !raw.is_null()
    {
        let invalid = || AppError::internal(fail, format!("invalid options format for field ID {}", f.id));
        let Value::Array(list) = raw else { return Err(invalid()) };
        for o in list.iter_mut() {
            let Value::Object(m) = o else { return Err(invalid()) };
            if !m.get("id").and_then(Value::as_str).is_some_and(|s| !s.is_empty()) {
                m.insert("id".into(), Value::String(new_id()));
            }
        }
    }
    Ok(())
}

/// `storedFieldAttrs`: an option-bearing field's attributes without its options, which have rows of their own.
fn stored_attrs(f: &Field) -> Option<String> {
    let mut a = f.attrs.clone()?;
    if f.supports_options() {
        a.remove("options");
        a.remove("options_count");
        a.remove("options_omitted");
    }
    Some(Value::Object(a).to_string())
}

/// `newPropertyOptionRow`.
fn option_row(f: &Field, o: &Map<String, Value>, sort_order: i32, now: i64) -> OptionRow {
    let mut attrs = Map::new();
    let name = o.get("name").and_then(Value::as_str).map(str::to_string);
    let color = o.get("color").and_then(Value::as_str).map(str::to_string);
    let rank = o
        .get("rank")
        .and_then(|r| r.as_f64())
        .filter(|f| f.fract() == 0.0 && f.abs() <= (1u64 << 53) as f64)
        .map(|f| f as i64);
    for (k, v) in o {
        let promoted = k == "id"
            || k == "parents"
            || (k == "name" && name.is_some())
            || (k == "color" && color.is_some())
            || (k == "rank" && rank.is_some());
        if !promoted {
            attrs.insert(k.clone(), v.clone());
        }
    }
    OptionRow {
        id: o.get("id").and_then(Value::as_str).unwrap_or("").to_string(),
        field_id: f.id.clone(),
        name,
        color,
        rank,
        sort_order,
        attrs: (!attrs.is_empty()).then_some(attrs),
        create_at: now,
        delete_at: 0,
    }
}

impl App {
    /// `upsertPropertyOptions`.
    async fn upsert_option_rows(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        group_id: &str,
        rows: &[OptionRow],
        now: i64,
    ) -> ApiResult<()> {
        for r in rows {
            sqlx::query(
                "INSERT INTO PropertyOptions (ID, GroupID, FieldID, Name, Color, Rank, SortOrder, Attrs, CreateAt, UpdateAt, DeleteAt)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8::jsonb, $9, $10, 0)
                 ON CONFLICT (FieldID, ID) DO UPDATE SET Name = EXCLUDED.Name, Color = EXCLUDED.Color, Rank = EXCLUDED.Rank,
                 SortOrder = EXCLUDED.SortOrder, Attrs = EXCLUDED.Attrs, UpdateAt = EXCLUDED.UpdateAt, DeleteAt = 0",
            )
            .bind(&r.id)
            .bind(group_id)
            .bind(&r.field_id)
            .bind(&r.name)
            .bind(&r.color)
            .bind(r.rank)
            .bind(r.sort_order)
            .bind(r.attrs.as_ref().map(|a| Value::Object(a.clone()).to_string()))
            .bind(r.create_at)
            .bind(now)
            .execute(&mut **tx)
            .await?;
        }
        Ok(())
    }

    /// `syncPropertyFieldOptions`: the option rows made to match the fields' option lists; the ids of the fields
    /// whose options changed.
    async fn sync_options(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        fields: &[Field],
        now: i64,
    ) -> ApiResult<Vec<String>> {
        let mut changed = vec![];
        for f in fields {
            if !f.supports_options() || f.options_omitted() {
                continue;
            }
            let existing = sqlx::query(sqlx::AssertSqlSafe(format!(
                "SELECT {OPTION_COLS} FROM PropertyOptions WHERE FieldID = ANY($1)"
            )))
            .bind(f.option_owners())
            .fetch_all(&mut **tx)
            .await?
            .iter()
            .map(option_from_row)
            .collect::<ApiResult<Vec<_>>>()?;
            let own: HashMap<&str, &OptionRow> =
                existing.iter().filter(|r| r.field_id == f.id).map(|r| (r.id.as_str(), r)).collect();
            let inherited: HashSet<&str> = existing
                .iter()
                .filter(|r| f.is_linked() && r.field_id == f.link_source())
                .map(|r| r.id.as_str())
                .collect();
            let (add, replacing) = graph::option_parent_links(f)
                .map_err(|e| graph::invalid_input("PropertyOption", "parents", &f.id, &e))?;
            let mut upserts = vec![];
            let mut keep = HashSet::new();
            for (i, o) in inline_options_lenient(f).iter().enumerate() {
                let mut row = option_row(f, o, i as i32 + 1, now);
                if row.id.is_empty() {
                    return Err(AppError::internal(
                        "app.property_field.create.app_error",
                        format!("option at index {} of field {} has no id", i + 1, f.id),
                    ));
                }
                keep.insert(row.id.clone());
                match own.get(row.id.as_str()) {
                    None if inherited.contains(row.id.as_str()) => continue,
                    None => {}
                    Some(cur) => {
                        row.create_at = cur.create_at;
                        let same = cur.sort_order == row.sort_order
                            && cur.name == row.name
                            && cur.color == row.color
                            && cur.rank == row.rank
                            && cur.attrs.clone().unwrap_or_default() == row.attrs.clone().unwrap_or_default();
                        if cur.delete_at == 0 && same {
                            continue;
                        }
                    }
                }
                upserts.push(row);
            }
            let deletes: Vec<String> =
                own.values().filter(|r| r.delete_at == 0 && !keep.contains(&r.id)).map(|r| r.id.clone()).collect();
            if !upserts.is_empty() || !deletes.is_empty() {
                changed.push(f.id.clone());
            }
            self.upsert_option_rows(tx, &f.group_id, &upserts, now).await?;
            if !deletes.is_empty() {
                sqlx::query(
                    "UPDATE PropertyOptions SET DeleteAt = $1, UpdateAt = $1 WHERE FieldID = $2 AND ID = ANY($3)",
                )
                .bind(now)
                .bind(&f.id)
                .bind(&deletes)
                .execute(&mut **tx)
                .await?;
                sqlx::query(
                    "DELETE FROM PropertyOptionEdges WHERE FieldID = $1 AND (ChildOptionID = ANY($2) OR ParentOptionID = ANY($2))",
                )
                .bind(&f.id)
                .bind(&deletes)
                .execute(&mut **tx)
                .await?;
            }
            if (!add.is_empty() || !replacing.is_empty())
                && graph::apply_option_parent_links(tx, f, &add, &replacing, now).await?
                && !changed.contains(&f.id)
            {
                changed.push(f.id.clone());
            }
        }
        Ok(changed)
    }

    /// `publishPropertyFieldEvent`.
    fn publish_field(&self, event: &str, f: &Field, connection_id: &str) {
        if f.is_v1() {
            return;
        }
        let mut b = Broadcast { omit_connection_id: connection_id.into(), ..Default::default() };
        match f.target_type.as_str() {
            "team" => b.team_id = f.target_id.clone(),
            "channel" => b.channel_id = f.target_id.clone(),
            "system" => {}
            _ => return,
        }
        let mut f = f.clone();
        if !f.public() && f.supports_options() {
            let a = f.attrs.get_or_insert_with(Map::new);
            a.insert("options".into(), json!([]));
            a.remove("options_count");
            a.insert("options_omitted".into(), Value::Bool(true));
        }
        let data = json!({
            "property_field": serde_json::to_string(&f).unwrap_or_default(),
            "object_type": f.object_type,
        });
        self.hub.publish(event, data, b);
    }
}

/// `createPropertyField`.
async fn create_field(
    St(app): St<State>,
    uri: Uri,
    Path(p): Path<Params>,
    headers: HeaderMap,
    auth: Result<Auth, AppError>,
    body: Bytes,
) -> ApiResult<Response> {
    let s = begin(&uri, &p, auth)?;
    let ot = require_object_type(&p)?;
    let group = app.v2_group(param(&p, "group_name")).await?;
    require_channel_attribute_license(&group, std::slice::from_ref(&ot))?;
    let o = body_object(&body, "property_field")?.ok_or_else(|| AppError::invalid_param("property_field"))?;
    let mut f = decode_field(&o).map_err(|e| AppError::invalid_param("property_field").detail(e))?;
    f.object_type = ot;
    f.group_id = group.id.clone();
    if f.protected {
        return Err(bad("api.property_field.create.protected_via_api.app_error"));
    }
    canonicalize_system(&mut f);
    if f.object_type == "template" && !app.has_perm(&s, MANAGE_SYSTEM).await {
        return Err(AppError::forbidden_perm(MANAGE_SYSTEM));
    }
    match f.target_type.as_str() {
        "channel" => {
            if f.target_id.is_empty() {
                return Err(bad("api.property_field.create.target_id_required.app_error"));
            }
            if !app.sess_channel(&s, &f.target_id, "create_post").await {
                return Err(AppError::forbidden_perm("create_post"));
            }
        }
        "team" => {
            if f.target_id.is_empty() {
                return Err(bad("api.property_field.create.target_id_required.app_error"));
            }
            if !app.has_team_perm(&s, &f.target_id, "manage_team").await {
                return Err(AppError::forbidden_perm("manage_team"));
            }
        }
        "system" => {
            if !app.has_perm(&s, MANAGE_SYSTEM).await {
                return Err(AppError::forbidden_perm(MANAGE_SYSTEM));
            }
        }
        _ => return Err(bad("api.property_field.create.invalid_target_type.app_error")),
    }
    f.created_by = s.user_id.clone();
    f.updated_by = s.user_id.clone();
    let can_pin = app.field_admin(&s.user_id, &f).await;
    let (level, values_level) = (default_level(&f), default_values_level(&f));
    if !can_pin {
        f.permission_field = Some(level.into());
        f.permission_values = Some(values_level.into());
        f.permission_options = Some(level.into());
    } else {
        f.permission_field.get_or_insert_with(|| level.into());
        f.permission_values.get_or_insert_with(|| values_level.into());
        f.permission_options.get_or_insert_with(|| level.into());
        if !f.is_linked() && !app.can_edit_field(&s, &f).await {
            return Err(err(StatusCode::FORBIDDEN, "api.property_field.create.creator_cannot_edit.app_error"));
        }
    }
    // App.CreatePropertyField
    canonicalize_system(&mut f);
    f.name = f.name.trim().to_string();
    // PropertyService.CreatePropertyField: the hooks, then createPropertyField. Whether the caller sent options of
    // its own is decided before a link copies the template's in.
    let supplied_options = f.supplies_options();
    if group.name == ACCESS_CONTROL {
        return Err(license_error());
    }
    if group.name == POST_ATTRIBUTES {
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(ID) FROM PropertyFields WHERE GroupID = $1 AND TargetType = $2 AND TargetID = $3 AND DeleteAt = 0",
        )
        .bind(&f.group_id)
        .bind(&f.target_type)
        .bind(&f.target_id)
        .fetch_one(&app.db)
        .await?;
        if n >= POST_ATTRIBUTES_MAX_FIELDS_PER_TARGET {
            return Err(err(StatusCode::UNPROCESSABLE_ENTITY, "app.property_field.create.target_limit_reached.app_error").detail(
                format!(
                    "limit_reached: field limit of {POST_ATTRIBUTES_MAX_FIELDS_PER_TARGET} reached for target {}/{}: per-target field limit reached",
                    f.target_type, f.target_id
                ),
            ));
        }
    }
    if f.is_linked() {
        link_to_template(&app, &mut f).await?;
    }
    let level = app.name_conflict(&f, "").await?;
    if !level.is_empty() {
        return Err(err(StatusCode::CONFLICT, "app.property_field.create.name_conflict.app_error")
            .with_params(&[("Name", &f.name), ("ConflictLevel", &level)]));
    }
    if f.kind == "graph" && f.is_linked() {
        if supplied_options {
            return Err(options_refused(&format!(
                "a field linking to field {0} serves that field's option hierarchy and cannot own options of its own; add them to field {0} instead",
                f.link_source()
            )));
        }
    } else {
        app.validate_option_blob_links(&mut f, None, "app.property_field.create.app_error").await?;
    }
    // SqlPropertyFieldStore.Create
    if !f.id.is_empty() {
        return Err(bad("app.property.invalid_input.app_error")
            .detail(format!("invalid input: entity: PropertyField field: id value: {}", f.id)));
    }
    f.id = new_id();
    f.create_at = now_ms();
    f.update_at = f.create_at;
    f.delete_at = 0;
    ensure_option_ids(&mut f, "app.property_field.create.app_error")?;
    validate_field(&f)?;
    let mut tx = app.db.begin().await?;
    sqlx::query(
        "INSERT INTO PropertyFields (ID, GroupID, Name, Type, Attrs, TargetID, TargetType, ObjectType, Protected, PermissionField,
         PermissionValues, PermissionOptions, LinkedFieldID, CreateAt, UpdateAt, DeleteAt, CreatedBy, UpdatedBy)
         VALUES ($1, $2, $3, $4::property_field_type, $5::jsonb, $6, $7, $8, $9, $10::permission_level, $11::permission_level,
         $12::permission_level, $13, $14, $15, $16, $17, $18)",
    )
    .bind(&f.id)
    .bind(&f.group_id)
    .bind(&f.name)
    .bind(&f.kind)
    .bind(stored_attrs(&f).unwrap_or_else(|| "null".into()))
    .bind(&f.target_id)
    .bind(&f.target_type)
    .bind(&f.object_type)
    .bind(f.protected)
    .bind(&f.permission_field)
    .bind(&f.permission_values)
    .bind(&f.permission_options)
    .bind(&f.linked_field_id)
    .bind(f.create_at)
    .bind(f.update_at)
    .bind(f.delete_at)
    .bind(&f.created_by)
    .bind(&f.updated_by)
    .execute(&mut *tx)
    .await?;
    app.sync_options(&mut tx, std::slice::from_ref(&f), f.create_at).await?;
    tx.commit().await?;
    app.publish_field("property_field_created", &f, &connection_id(&headers));
    Ok(created(&f))
}

/// The part of `createPropertyField` that links a new field to a template of its group.
async fn link_to_template(app: &App, f: &mut Field) -> ApiResult<()> {
    let refuse = |id: &str, detail: String| bad(id).detail(detail);
    if f.object_type == "template" {
        return Err(refuse(
            "app.property_field.create.template_cannot_be_linked.app_error",
            "template fields cannot have a linked_field_id".into(),
        ));
    }
    let src_id = f.link_source().to_string();
    let Some(src) = app.property_field_row("", &src_id).await? else {
        return Err(refuse(
            "app.property_field.create.linked_source_not_found.app_error",
            format!("linked source field {src_id:?} not found"),
        ));
    };
    if src.group_id != f.group_id {
        return Err(refuse(
            "app.property_field.create.linked_source_cross_group.app_error",
            format!(
                "cannot link to field {src_id:?} in group {:?}: source must be in the same group {:?}",
                src.group_id, f.group_id
            ),
        ));
    }
    if src.delete_at != 0 {
        return Err(refuse(
            "app.property_field.create.linked_source_deleted.app_error",
            format!("linked source field {src_id:?} is deleted"),
        ));
    }
    if src.object_type != "template" {
        return Err(refuse(
            "app.property_field.create.linked_source_not_template.app_error",
            "can only link to template fields".into(),
        ));
    }
    if f.target_type != src.target_type {
        return Err(refuse(
            "app.property_field.create.linked_target_type_mismatch.app_error",
            format!(
                "linked field target_type {:?} must match source template target_type {:?}",
                f.target_type, src.target_type
            ),
        ));
    }
    if src.is_linked() {
        return Err(refuse(
            "app.property_field.create.linked_source_is_linked.app_error",
            "cannot link to a field that is itself linked (no chains allowed)".into(),
        ));
    }
    f.kind = src.kind.clone();
    let a = f.attrs.get_or_insert_with(Map::new);
    if let Some(o) = src.attr("options") {
        a.insert("options".into(), o.clone());
    }
    let user = f.object_type == "user";
    for k in ["ldap", "saml", "openid"] {
        a.remove(k);
        if user && let Some(v) = src.attr(k) {
            a.insert(k.into(), v.clone());
        }
    }
    if src.permission_field.is_some() {
        f.permission_field = src.permission_field.clone();
    }
    // The handler always pins PermissionValues before the service sees the field.
    if src.permission_options.is_some() {
        f.permission_options = src.permission_options.clone();
    }
    Ok(())
}

/// `model.PropertyFieldPatch`, decoded.
#[derive(Default)]
struct FieldPatch {
    name: Option<String>,
    kind: Option<String>,
    attrs: Option<Option<Map<String, Value>>>,
    linked_field_id: Option<String>,
    permission_values: Option<String>,
}

fn decode_patch(o: &Map<String, Value>) -> Result<FieldPatch, String> {
    let attrs = match o.get("attrs") {
        None | Some(Value::Null) => None,
        Some(Value::Object(m)) => Some(Some(m.clone())),
        Some(v) => {
            return Err(format!("json: cannot unmarshal {} into Go value of type model.StringInterface", kind(v)));
        }
    };
    // target_id and target_type are read and then dropped by the handler; a bad type still fails the decode.
    go_str(o, "target_id")?;
    go_str(o, "target_type")?;
    Ok(FieldPatch {
        name: go_str(o, "name")?,
        kind: go_str(o, "type")?,
        attrs,
        linked_field_id: go_str(o, "linked_field_id")?,
        permission_values: go_str(o, "permission_values")?,
    })
}

/// `patchPropertyField`.
async fn patch_field(
    St(app): St<State>,
    uri: Uri,
    Path(p): Path<Params>,
    headers: HeaderMap,
    auth: Result<Auth, AppError>,
    body: Bytes,
) -> ApiResult<Response> {
    let s = begin(&uri, &p, auth)?;
    let ot = require_object_type(&p)?;
    let field_id = require_field_id(&p)?;
    let group = app.v2_group(param(&p, "group_name")).await?;
    require_channel_attribute_license(&group, std::slice::from_ref(&ot))?;
    let o =
        body_object(&body, "property_field_patch")?.ok_or_else(|| AppError::invalid_param("property_field_patch"))?;
    let mut patch = decode_patch(&o).map_err(|e| AppError::invalid_param("property_field_patch").detail(e))?;
    if let Some(n) = patch.name.as_mut() {
        *n = n.trim().to_string();
    }
    // PropertyFieldPatch.IsValid
    let patch_invalid = |name: &str, reason: &str| {
        bad("model.property_field.is_valid.app_error").with_params(&[("FieldName", name), ("Reason", reason)])
    };
    if patch.name.as_deref() == Some("") {
        return Err(patch_invalid("name", "value cannot be empty"));
    }
    if patch.name.as_ref().is_some_and(|n| n.chars().count() > 255) {
        return Err(patch_invalid("name", "value exceeds maximum length"));
    }
    if patch.kind.as_ref().is_some_and(|k| !FIELD_TYPES.contains(&k.as_str())) {
        return Err(patch_invalid("type", "unknown value"));
    }
    if patch.permission_values.as_ref().is_some_and(|l| !PERMISSION_LEVELS.contains(&l.as_str())) {
        return Err(patch_invalid("permission_values", "invalid permission level"));
    }
    let existing = app.get_property_field(&group, &field_id).await?;
    if existing.is_v1() {
        return Err(bad("api.property_field.patch.legacy_field.app_error"));
    }
    if existing.object_type != ot {
        return Err(object_type_mismatch());
    }
    if let Some(pv) = &patch.permission_values {
        if !existing.is_linked() {
            return Err(bad("api.property_field.patch.permission_values_not_linked.app_error"));
        }
        if pv != "member" && pv != "sysadmin" {
            return Err(bad("api.property_field.patch.permission_values_invalid.app_error"));
        }
    }
    let options_only = patch.name.is_none()
        && patch.kind.is_none()
        && patch.linked_field_id.is_none()
        && patch.permission_values.is_none()
        && matches!(&patch.attrs, Some(Some(a)) if a.len() == 1 && a.contains_key("options"))
        && existing.supports_options();
    if options_only {
        if !app.can_manage_options(&s, &existing).await {
            return Err(err(StatusCode::FORBIDDEN, "api.property_field.update.no_options_permission.app_error"));
        }
    } else if !app.can_edit_field(&s, &existing).await {
        return Err(err(StatusCode::FORBIDDEN, "api.property_field.update.no_field_permission.app_error"));
    }
    // PropertyField.Patch, merging attrs
    let mut f = existing.clone();
    if let Some(n) = patch.name.take() {
        f.name = n;
    }
    if let Some(k) = patch.kind.take() {
        f.kind = k;
    }
    if let Some(Some(a)) = patch.attrs.take() {
        let cur = f.attrs.get_or_insert_with(Map::new);
        for (k, v) in a {
            if v.is_null() {
                cur.remove(&k);
            } else {
                cur.insert(k, v);
            }
        }
    }
    if let Some(l) = patch.linked_field_id.take() {
        f.linked_field_id = (!l.is_empty()).then_some(l);
    }
    if let Some(pv) = patch.permission_values.take() {
        f.permission_values = Some(pv);
    }
    f.updated_by = s.user_id.clone();
    f.name = f.name.trim().to_string();
    let (updated, cleared) = update_field(&app, &group, &existing, f).await?;
    let conn = connection_id(&headers);
    app.publish_field("property_field_updated", &updated, &conn);
    for id in cleared {
        app.hub.publish("property_values_updated", json!({"field_id": id, "values": "[]"}), Broadcast::default());
    }
    Ok(axum::Json(updated).into_response())
}

/// `App.UpdatePropertyFields` and the service's checks for one field, then `SqlPropertyFieldStore.Update` and the
/// type-change value cleanup: the field as written and the fields whose values were cleared.
async fn update_field(app: &App, group: &Group, existing: &Field, mut f: Field) -> ApiResult<(Field, Vec<String>)> {
    let linked_change = |id: &str, detail: &str| bad(id).with_param("FieldID", &existing.id).detail(detail.to_string());
    if existing.is_linked() {
        if f.kind != existing.kind {
            return Err(linked_change(
                "app.property_field.update.linked_type_change.app_error",
                "cannot modify type of a linked field",
            ));
        }
        if existing.attr("options") != f.attr("options") {
            return Err(linked_change(
                "app.property_field.update.linked_options_change.app_error",
                "cannot modify options of a linked field",
            ));
        }
        if f.is_linked() && f.link_source() != existing.link_source() {
            return Err(linked_change(
                "app.property_field.update.cannot_change_link_target.app_error",
                "cannot change link target",
            ));
        }
    } else if f.is_linked() {
        return Err(linked_change(
            "app.property_field.update.cannot_link_existing.app_error",
            "linked_field_id can only be set at creation time",
        ));
    }
    if existing.protected {
        return Err(err(StatusCode::FORBIDDEN, "app.property_field.update.protected.app_error")
            .with_param("FieldID", &existing.id));
    }
    // PropertyService.UpdatePropertyFields: the hooks (the license), then updatePropertyFields.
    if group.name == ACCESS_CONTROL {
        return Err(license_error());
    }
    let stored = app.property_field_row(&group.id, &f.id).await?.ok_or_else(property_not_found)?;
    if stored.options_omitted() && (f.supplies_options() || !f.options_omitted()) {
        return Err(bad("app.property_field.update.options_withheld.app_error")
            .with_params(&[("FieldID", &stored.id), ("Max", &MAX_HYDRATED_OPTIONS.to_string())]));
    }
    if f.kind != stored.kind && (f.kind == "graph" || stored.kind == "graph") {
        return Err(bad("app.property_field.update.graph_type_change.app_error").with_param("FieldID", &stored.id));
    }
    app.validate_option_blob_links(&mut f, Some(&stored), "app.property_field.update.app_error").await?;
    if stored.kind == "graph" && !f.options_omitted() {
        let keeping: HashSet<String> = inline_options_lenient(&f)
            .iter()
            .filter_map(|o| o.get("id").and_then(Value::as_str).map(str::to_string))
            .collect();
        let dropped: Vec<String> = inline_options_lenient(&stored)
            .iter()
            .filter_map(|o| o.get("id").and_then(Value::as_str).map(str::to_string))
            .filter(|id| !keeping.contains(id))
            .collect();
        if let Some((at, child)) = blocking_child_below(app, &stored, &dropped).await? {
            return Err(options_refused(&format!(
                "the option list leaves out option {:?}, which option {child:?} is still below; leave that one out as well or move it first",
                dropped[at]
            )));
        }
    }
    let held: HashMap<String, String> = inline_options_lenient(&stored)
        .iter()
        .filter_map(|o| {
            Some((o.get("id")?.as_str()?.to_string(), o.get("name").and_then(Value::as_str).unwrap_or("").to_string()))
        })
        .collect();
    let added: Vec<String> = inline_options_lenient(&f)
        .iter()
        .filter_map(|o| {
            let name = o.get("name").and_then(Value::as_str).unwrap_or("");
            let id = o.get("id").and_then(Value::as_str).unwrap_or("");
            (!name.is_empty() && held.get(id).map(String::as_str) != Some(name)).then(|| name.to_string())
        })
        .collect();
    require_names_free_of_dependents(app, &stored, &added).await?;
    if f.kind != stored.kind && app.count_linked_fields(&f.id).await? > 0 {
        return Err(err(StatusCode::CONFLICT, "app.property_field.update.type_change_with_dependents.app_error")
            .detail("cannot change type of a field with active linked dependents"));
    }
    if stored.name != f.name
        || stored.target_type != f.target_type
        || stored.target_id != f.target_id
        || stored.object_type != f.object_type
    {
        let level = app.name_conflict(&f, &f.id).await?;
        if !level.is_empty() {
            return Err(err(StatusCode::CONFLICT, "app.property_field.update.name_conflict.app_error")
                .with_params(&[("Name", &f.name), ("ConflictLevel", &level)]));
        }
    }
    // SqlPropertyFieldStore.Update
    let now = now_ms();
    f.update_at = now;
    validate_field(&f)?;
    let mut tx = app.db.begin().await?;
    let n = sqlx::query(
        "UPDATE PropertyFields SET Name = $1, Type = $2::property_field_type, Attrs = $3::jsonb, TargetID = $4, TargetType = $5,
         Protected = $6, PermissionField = $7::permission_level, PermissionValues = $8::permission_level,
         PermissionOptions = $9::permission_level, LinkedFieldID = $10, UpdateAt = $11, DeleteAt = $12, UpdatedBy = $13
         WHERE ID = $14 AND GroupID = $15 AND UpdateAt = $16",
    )
    .bind(&f.name)
    .bind(&f.kind)
    .bind(stored_attrs(&f).unwrap_or_else(|| "null".into()))
    .bind(&f.target_id)
    .bind(&f.target_type)
    .bind(f.protected)
    .bind(&f.permission_field)
    .bind(&f.permission_values)
    .bind(&f.permission_options)
    .bind(&f.linked_field_id)
    .bind(now)
    .bind(f.delete_at)
    .bind(&f.updated_by)
    .bind(&f.id)
    .bind(&group.id)
    .bind(stored.update_at)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if n != 1 {
        return Err(err(StatusCode::CONFLICT, "app.property_field.update.conflict.app_error")
            .detail("concurrent modification detected; please retry"));
    }
    if stored.is_linked() && !f.is_linked() && f.supports_options() {
        graph::take_over_link_source(&mut tx, &f, stored.link_source(), now).await?;
    }
    app.sync_options(&mut tx, std::slice::from_ref(&f), now).await?;
    tx.commit().await?;
    // TypeChangeValueCleanupHook
    let mut cleared = vec![];
    let select_rank = |a: &str, b: &str| (a == "select" && b == "rank") || (a == "rank" && b == "select");
    if stored.kind != f.kind && !select_rank(&stored.kind, &f.kind) {
        sqlx::query("UPDATE PropertyValues SET DeleteAt = $1 WHERE FieldID = $2 AND GroupID = $3")
            .bind(now_ms())
            .bind(&f.id)
            .bind(&group.id)
            .execute(&app.db)
            .await?;
        cleared.push(f.id.clone());
    }
    Ok((f, cleared))
}

/// `requireNamesFreeOfDependents`.
async fn require_names_free_of_dependents(app: &App, f: &Field, names: &[String]) -> ApiResult<()> {
    if names.is_empty() {
        return Ok(());
    }
    let taken: Vec<(String, String)> = sqlx::query_as(
        "SELECT opt.Name, opt.FieldID FROM PropertyOptions opt JOIN PropertyFields dependent ON dependent.ID = opt.FieldID
         WHERE dependent.LinkedFieldID = $1 AND dependent.DeleteAt = 0 AND opt.Name = ANY($2) AND opt.DeleteAt = 0",
    )
    .bind(&f.id)
    .bind(names)
    .fetch_all(&app.db)
    .await?;
    let taken: HashMap<String, String> = taken.into_iter().collect();
    for n in names {
        if let Some(owner) = taken.get(n) {
            return Err(options_refused(&format!(
                "names option {n:?}, which field {owner} already has as a local option of its own; that field serves this field's options as well, so the name would answer to two options there"
            )));
        }
    }
    Ok(())
}

/// `blockingChildBelow`: the first option of a graph field to go that another option staying is still below.
async fn blocking_child_below(app: &App, f: &Field, ids: &[String]) -> ApiResult<Option<(usize, String)>> {
    if f.kind != "graph" || ids.is_empty() {
        return Ok(None);
    }
    let edges: Vec<(String, String)> =
        // In the order of the (FieldID, ParentOptionID, ChildOptionID) index Go's query is answered from: the last
        // child of a parent is the one reported.
        sqlx::query_as(
            "SELECT ParentOptionID, ChildOptionID FROM PropertyOptionEdges WHERE FieldID = $1 AND ParentOptionID = ANY($2)
             ORDER BY ParentOptionID, ChildOptionID",
        )
            .bind(&f.id)
            .bind(ids)
            .fetch_all(&app.db)
            .await?;
    let going: HashSet<&String> = ids.iter().collect();
    let staying: HashMap<&String, &String> =
        edges.iter().filter(|(_, c)| !going.contains(c)).map(|(p, c)| (p, c)).collect();
    Ok(ids.iter().enumerate().find_map(|(i, id)| staying.get(id).map(|c| (i, (*c).clone()))))
}

/// `deletePropertyField`.
async fn delete_field(
    St(app): St<State>,
    uri: Uri,
    Path(p): Path<Params>,
    headers: HeaderMap,
    auth: Result<Auth, AppError>,
) -> ApiResult<Response> {
    let s = begin(&uri, &p, auth)?;
    let ot = require_object_type(&p)?;
    let field_id = require_field_id(&p)?;
    let group = app.v2_group(param(&p, "group_name")).await?;
    require_channel_attribute_license(&group, std::slice::from_ref(&ot))?;
    let existing = app.get_property_field(&group, &field_id).await?;
    if existing.object_type != ot {
        return Err(object_type_mismatch());
    }
    if !app.can_edit_field(&s, &existing).await {
        return Err(err(StatusCode::FORBIDDEN, "api.property_field.delete.no_permission.app_error"));
    }
    // App.DeletePropertyField
    if existing.protected {
        return Err(err(StatusCode::FORBIDDEN, "app.property_field.delete.protected.app_error"));
    }
    if app.count_linked_fields(&existing.id).await? > 0 {
        return Err(err(StatusCode::CONFLICT, "app.property_field.delete.has_linked_dependents.app_error")
            .detail("cannot delete field with active linked dependents; unlink or delete dependent fields first"));
    }
    let now = now_ms();
    let mut tx = app.db.begin().await?;
    sqlx::query("UPDATE PropertyValues SET DeleteAt = $1 WHERE FieldID = $2 AND GroupID = $3")
        .bind(now)
        .bind(&existing.id)
        .bind(&group.id)
        .execute(&mut *tx)
        .await?;
    let n = sqlx::query(
        "UPDATE PropertyFields SET DeleteAt = $1, UpdateAt = GREATEST($1, UpdateAt + 1) WHERE ID = $2 AND GroupID = $3",
    )
    .bind(now)
    .bind(&existing.id)
    .bind(&group.id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if n == 0 {
        return Err(property_not_found());
    }
    sqlx::query("UPDATE PropertyOptions SET DeleteAt = $1, UpdateAt = $1 WHERE FieldID = $2 AND DeleteAt = 0")
        .bind(now)
        .bind(&existing.id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM PropertyOptionEdges WHERE FieldID = $1").bind(&existing.id).execute(&mut *tx).await?;
    tx.commit().await?;
    let mut b = Broadcast { omit_connection_id: connection_id(&headers), ..Default::default() };
    let ok = match existing.target_type.as_str() {
        "team" => {
            b.team_id = existing.target_id.clone();
            true
        }
        "channel" => {
            b.channel_id = existing.target_id.clone();
            true
        }
        "system" => true,
        _ => false,
    };
    if ok {
        app.hub.publish(
            "property_field_deleted",
            json!({"field_id": existing.id, "object_type": existing.object_type}),
            b,
        );
    }
    Ok(crate::users::ok().into_response())
}

// ---------- field options ----------

/// `propertyFieldForOptions`.
async fn field_for_options(app: &App, p: &Params) -> ApiResult<(Group, Field)> {
    let ot = require_object_type(p)?;
    let field_id = require_field_id(p)?;
    let group = app.v2_group(param(p, "group_name")).await?;
    require_channel_attribute_license(&group, std::slice::from_ref(&ot))?;
    let f = app.get_property_field(&group, &field_id).await?;
    if f.is_v1() {
        return Err(bad("api.property_field.options.legacy_field.app_error"));
    }
    if f.object_type != ot {
        return Err(object_type_mismatch());
    }
    Ok((group, f))
}

/// `requireOptionsAddressable`.
fn require_options_addressable(f: &Field) -> ApiResult<()> {
    if f.is_v1() {
        return Err(options_refused(&format!(
            "field {} predates addressable options; its option list is part of the field",
            f.id
        )));
    }
    if !f.supports_options() {
        return Err(options_refused(&format!("a {} field has no options", f.kind)));
    }
    Ok(())
}

impl App {
    /// `attachOptionParentNames`: a graph field's options with the names of the options right above them.
    async fn attach_parents(&self, f: &Field, options: &mut [FieldOption]) -> ApiResult<()> {
        let owner = if f.is_linked() { f.link_source().to_string() } else { f.id.clone() };
        let ids: Vec<String> = options.iter().map(|o| o.id.clone()).collect();
        let edges: Vec<(String, String)> = sqlx::query_as(
            "SELECT ParentOptionID, ChildOptionID FROM PropertyOptionEdges WHERE FieldID = $1 AND ChildOptionID = ANY($2)",
        )
        .bind(&owner)
        .bind(&ids)
        .fetch_all(&self.db)
        .await?;
        let parents: Vec<String> = edges.iter().map(|(p, _)| p.clone()).collect();
        let names: HashMap<String, Option<String>> =
            sqlx::query_as("SELECT ID, Name FROM PropertyOptions WHERE FieldID = $1 AND ID = ANY($2)")
                .bind(&owner)
                .bind(&parents)
                .fetch_all(&self.db)
                .await?
                .into_iter()
                .collect();
        let mut by_child: HashMap<String, Vec<String>> = HashMap::new();
        for (p, c) in &edges {
            let Some(Some(n)) = names.get(p).filter(|n| n.as_deref().is_some_and(|n| !n.is_empty())) else {
                return Err(AppError::internal(
                    "app.property_field.options.get.app_error",
                    format!("option {p} of field {owner} is above option {c} but has no name to report it by"),
                ));
            };
            by_child.entry(c.clone()).or_default().push(n.clone());
        }
        for o in options.iter_mut() {
            let mut ps = by_child.remove(&o.id).unwrap_or_default();
            ps.sort();
            o.parents = Some(ps);
        }
        Ok(())
    }

    /// The live options of a field (or of the template it links to) with these ids or names.
    async fn options_where(&self, f: &Field, column: &str, keys: &[String]) -> ApiResult<Vec<FieldOption>> {
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {OPTION_COLS} FROM PropertyOptions WHERE FieldID = ANY($1) AND {column} = ANY($2) AND DeleteAt = 0"
        )))
        .bind(f.option_owners())
        .bind(keys)
        .fetch_all(&self.db)
        .await?;
        let mut out =
            rows.iter().map(|r| option_from_row(r).map(|o| o.option(&f.id))).collect::<ApiResult<Vec<_>>>()?;
        if f.kind == "graph" {
            self.attach_parents(f, &mut out).await?;
        }
        Ok(out)
    }

    /// `writableField`: the field as stored, if its options may be written.
    async fn writable_field(&self, group: &Group, id: &str) -> ApiResult<Field> {
        let f = self.property_field_row(&group.id, id).await?.ok_or_else(property_not_found)?;
        if group.name == ACCESS_CONTROL {
            return Err(license_error());
        }
        if f.update_at == 0 {
            return Err(options_refused("a field's options can only be changed through a field read from the store"));
        }
        require_options_addressable(&f)?;
        if f.delete_at != 0 {
            return Err(options_refused(&format!("field {} has been deleted", f.id)));
        }
        if f.kind == "rank" {
            return Err(options_refused(
                "the options of a rank field carry an order that is set by writing them as the field's option list",
            ));
        }
        if f.kind == "graph" && f.is_linked() {
            return Err(options_refused(&format!(
                "field {} serves the option hierarchy of the template it links to and cannot own options of its own; change them on field {} instead",
                f.id,
                f.link_source()
            )));
        }
        Ok(f)
    }

    /// `propertyFieldOptionsChanged`: the field and the fields linking to it, re-read and announced.
    async fn options_changed(&self, group_id: &str, field_id: &str, connection_id: &str) {
        let Ok(Some(f)) = self.property_field_row(group_id, field_id).await else { return };
        self.publish_field("property_field_updated", &f, connection_id);
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {FIELD_COLS} FROM PropertyFields WHERE LinkedFieldID = $1 AND DeleteAt = 0 AND ID <> $1"
        )))
        .bind(field_id)
        .fetch_all(&self.db)
        .await
        .unwrap_or_default();
        let mut deps: Vec<Field> = rows.iter().filter_map(|r| field_from_row(r).ok()).collect();
        if self.hydrate_options(&mut deps).await.is_ok() {
            for d in &deps {
                self.publish_field("property_field_updated", d, "");
            }
        }
    }
}

/// `getPropertyFieldOptions`.
async fn get_options(
    St(app): St<State>,
    uri: Uri,
    Path(p): Path<Params>,
    Query(q): Q,
    auth: Result<Auth, AppError>,
) -> ApiResult<Response> {
    let s = begin(&uri, &p, auth)?;
    let (_, f) = field_for_options(&app, &p).await?;
    let mut o = SearchOpts { target_type: f.target_type.clone(), ..Default::default() };
    if !f.target_id.is_empty() {
        o.target_ids = vec![f.target_id.clone()];
    }
    resolve_scope(&app, &s, &mut o).await?;
    let cursor_id = q.get("cursor_id").cloned().unwrap_or_default();
    let cursor_at = query_i64(&q, "cursor_create_at")?.unwrap_or(0);
    if cursor_at < 0 || cursor_id.is_empty() != (cursor_at == 0) {
        return Err(bad("api.property_field.options.invalid_cursor.app_error"));
    }
    let per_page = per_page(&q).min(OPTIONS_MAX_PER_REQUEST as i64);
    // PropertyService.GetFieldOptions
    if per_page <= 0 {
        return Err(options_refused("a page of options has to be asked for with a positive page size"));
    }
    require_options_addressable(&f)?;
    let mut sql = format!("SELECT {OPTION_COLS} FROM PropertyOptions WHERE FieldID = ANY($1) AND DeleteAt = 0");
    if !cursor_id.is_empty() {
        sql += " AND (CreateAt > $2 OR (CreateAt = $2 AND ID > $3))";
    } else {
        sql += " AND $2::bigint IS NOT NULL AND $3::text IS NOT NULL";
    }
    sql += &format!(" ORDER BY CreateAt ASC, ID ASC LIMIT {per_page}");
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(f.option_owners())
        .bind(cursor_at)
        .bind(&cursor_id)
        .fetch_all(&app.db)
        .await?
        .iter()
        .map(option_from_row)
        .collect::<ApiResult<Vec<_>>>()?;
    let mut page = OptionPage {
        options: vec![],
        has_more: rows.len() as i64 == per_page,
        next_cursor_create_at: 0,
        next_cursor_id: String::new(),
    };
    if page.has_more
        && let Some(last) = rows.last()
    {
        page.next_cursor_create_at = last.create_at;
        page.next_cursor_id = last.id.clone();
    }
    page.options = rows.iter().map(|r| r.option(&f.id)).collect();
    if f.kind == "graph" {
        app.attach_parents(&f, &mut page.options).await?;
    }
    Ok(axum::Json(page).into_response())
}

/// `decodePropertyFieldOptions`.
fn decode_options(body: &[u8]) -> ApiResult<Vec<Option<Map<String, Value>>>> {
    let items = body_array(body, "property_field_options")?;
    let mut out = vec![];
    for v in items {
        match v {
            Value::Object(m) => {
                for k in ["id", "name", "color"] {
                    go_str(&m, k).map_err(|e| AppError::invalid_param("property_field_options").detail(e))?;
                }
                go_map(&m, "attrs").map_err(|e| AppError::invalid_param("property_field_options").detail(e))?;
                go_strings(&m, "parents").map_err(|e| AppError::invalid_param("property_field_options").detail(e))?;
                out.push(Some(m));
            }
            Value::Null => out.push(None),
            v => {
                return Err(AppError::invalid_param("property_field_options").detail(format!(
                    "json: cannot unmarshal {} into Go value of type model.PropertyFieldOption",
                    kind(&v)
                )));
            }
        }
    }
    check_option_count(out.len())?;
    if let Some(i) = out.iter().position(Option::is_none) {
        return Err(AppError::invalid_param("property_field_options").detail(format!("options[{i}] is not an option")));
    }
    Ok(out)
}

/// `checkPropertyFieldOptionCount`.
fn check_option_count(n: usize) -> ApiResult<()> {
    if n == 0 {
        return Err(bad("api.property_field.options.empty_body.app_error"));
    }
    if n > OPTIONS_MAX_PER_REQUEST {
        return Err(bad("api.property_field.options.too_many_items.request_error")
            .with_param("Max", &OPTIONS_MAX_PER_REQUEST.to_string()));
    }
    Ok(())
}

/// An option of a request, as `model.PropertyFieldOption` holds it.
struct NewOption {
    id: String,
    name: String,
    color: Option<String>,
    attrs: Option<Map<String, Value>>,
    parents: Option<Vec<String>>,
}

/// `prepareOptionPayload`.
fn prepare_options(f: &Field, items: &[Option<Map<String, Value>>], require_id: bool) -> ApiResult<Vec<NewOption>> {
    if items.is_empty() {
        return Err(options_refused("no options to write"));
    }
    if items.len() > OPTIONS_MAX_PER_REQUEST {
        return Err(options_refused(&format!(
            "names {} options to write, and no more than {OPTIONS_MAX_PER_REQUEST} may be named in one call",
            items.len()
        )));
    }
    let mut seen = HashSet::new();
    let mut out = vec![];
    for (i, m) in items.iter().enumerate() {
        let Some(m) = m else { return Err(option_error(i, "is not an option")) };
        let o = NewOption {
            id: m.get("id").and_then(Value::as_str).unwrap_or("").to_string(),
            name: m.get("name").and_then(Value::as_str).unwrap_or("").trim().to_string(),
            color: m.get("color").and_then(Value::as_str).map(str::to_string),
            attrs: m.get("attrs").and_then(Value::as_object).cloned(),
            parents: m
                .get("parents")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect()),
        };
        if require_id && o.id.is_empty() {
            return Err(option_error(i, "names no option"));
        }
        if !require_id && !o.id.is_empty() {
            return Err(option_error(
                i,
                &format!("carries the id {:?}; an option's identifier is assigned when it is created", o.id),
            ));
        }
        if require_id && !seen.insert(o.id.clone()) {
            return Err(option_error(i, &format!("names option {:?}, which is already in the list", o.id)));
        }
        if o.parents.is_some() && f.kind != "graph" {
            return Err(option_error(
                i,
                &format!("carries parents, and the options of a {} field form no hierarchy", f.kind),
            ));
        }
        if o.name.is_empty() {
            return Err(option_error(i, "is not a valid option: name cannot be empty"));
        }
        if let Some(ps) = &o.parents {
            if ps.len() > graph::MAX_PARENTS {
                return Err(option_error(
                    i,
                    &format!(
                        "is not a valid option: no option may have more than {} options directly above it",
                        graph::MAX_PARENTS
                    ),
                ));
            }
            let mut seen = HashSet::new();
            for p in ps {
                if p.is_empty() {
                    return Err(option_error(i, "is not a valid option: a parent name cannot be empty"));
                }
                if !seen.insert(p) {
                    return Err(option_error(
                        i,
                        &format!("is not a valid option: parent {p:?} is named more than once"),
                    ));
                }
            }
        }
        if let Some(k) =
            o.attrs.as_ref().and_then(|a| ["id", "name", "color", "rank"].into_iter().find(|k| a.contains_key(*k)))
        {
            return Err(option_error(i, &format!("is not a valid option: attrs cannot carry the reserved key {k:?}")));
        }
        out.push(o);
    }
    // PropertyOptions.IsValid
    let (mut ids, mut names) = (HashSet::new(), HashSet::new());
    for (i, o) in out.iter().enumerate() {
        if !o.id.is_empty() && !is_valid_id(&o.id) && require_id {
            return Err(options_refused(&format!("invalid option at index {i}: id is not a valid ID")));
        }
        if !o.id.is_empty() && !ids.insert(o.id.clone()) {
            return Err(options_refused(&format!("duplicate option id found at index {i}: {}", o.id)));
        }
        if !names.insert(o.name.clone()) {
            return Err(options_refused(&format!("duplicate option name found at index {i}: {}", o.name)));
        }
    }
    Ok(out)
}

impl App {
    /// `MutateOptions`: the options written (appended after the field's last) and the parent links added and removed,
    /// the field's `UpdateAt` moved on so a concurrent field write fails.
    async fn write_options(
        &self,
        f: &Field,
        options: &[NewOption],
        append: bool,
        add: &[graph::Edge],
        remove: &[graph::Edge],
    ) -> ApiResult<()> {
        let now = now_ms();
        let mut tx = self.db.begin().await?;
        if !add.is_empty() || !remove.is_empty() {
            graph::require_live_graph_field(&mut tx, &f.id).await?;
        }
        let n = sqlx::query("UPDATE PropertyFields SET UpdateAt = GREATEST($1, UpdateAt + 1) WHERE ID = $2 AND GroupID = $3 AND UpdateAt = $4")
            .bind(now)
            .bind(&f.id)
            .bind(&f.group_id)
            .bind(f.update_at)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if n != 1 {
            return Err(err(StatusCode::CONFLICT, "app.property_field.update.conflict.app_error")
                .detail("concurrent modification detected; please retry"));
        }
        let after: i32 = if append {
            sqlx::query_scalar("SELECT COALESCE(MAX(SortOrder), 0) FROM PropertyOptions WHERE FieldID = $1")
                .bind(&f.id)
                .fetch_one(&mut *tx)
                .await?
        } else {
            0
        };
        for (i, o) in options.iter().enumerate() {
            let attrs = o.attrs.as_ref().filter(|a| !a.is_empty()).map(|a| Value::Object(a.clone()).to_string());
            if append {
                sqlx::query(
                    "INSERT INTO PropertyOptions (ID, GroupID, FieldID, Name, Color, Rank, SortOrder, Attrs, CreateAt, UpdateAt, DeleteAt)
                     VALUES ($1, $2, $3, $4, $5, NULL, $6, $7::jsonb, $8, $8, 0)",
                )
                .bind(&o.id)
                .bind(&f.group_id)
                .bind(&f.id)
                .bind(&o.name)
                .bind(&o.color)
                .bind(after + i as i32 + 1)
                .bind(attrs)
                .bind(now)
                .execute(&mut *tx)
                .await?;
            } else {
                sqlx::query(
                    "UPDATE PropertyOptions SET Name = $1, Color = $2, Attrs = $3::jsonb, UpdateAt = $4 WHERE FieldID = $5 AND ID = $6",
                )
                .bind(&o.name)
                .bind(&o.color)
                .bind(attrs)
                .bind(now)
                .bind(&f.id)
                .bind(&o.id)
                .execute(&mut *tx)
                .await?;
            }
        }
        graph::write_edges(&mut tx, &f.id, add, remove, now).await?;
        tx.commit().await?;
        Ok(())
    }

    /// `readBackOptions`: the written options as stored, in the order they were given.
    async fn read_back(&self, f: &Field, ids: &[String]) -> ApiResult<Vec<FieldOption>> {
        let stored = self.options_where(f, "ID", ids).await?;
        let by_id: HashMap<&str, &FieldOption> = stored.iter().map(|o| (o.id.as_str(), o)).collect();
        Ok(ids.iter().filter_map(|id| by_id.get(id.as_str()).map(|o| (*o).clone())).collect())
    }
}

/// `createPropertyFieldOptions`.
async fn create_options(
    St(app): St<State>,
    uri: Uri,
    Path(p): Path<Params>,
    headers: HeaderMap,
    auth: Result<Auth, AppError>,
    body: Bytes,
) -> ApiResult<Response> {
    let s = begin(&uri, &p, auth)?;
    let (group, f) = field_for_options(&app, &p).await?;
    let items = decode_options(&body)?;
    if !app.can_manage_options(&s, &f).await {
        return Err(err(StatusCode::FORBIDDEN, "api.property_field.options.no_permission.app_error"));
    }
    // PropertyService.CreateFieldOptions
    let f = app.writable_field(&group, &f.id).await?;
    let mut options = prepare_options(&f, &items, false)?;
    if f.kind == "graph" {
        let held: i64 = sqlx::query_scalar("SELECT COUNT(ID) FROM PropertyOptions WHERE FieldID = $1 AND DeleteAt = 0")
            .bind(&f.id)
            .fetch_one(&app.db)
            .await?;
        let total = held as usize + options.len();
        if total > GRAPH_MAX_OPTIONS {
            return Err(options_refused(&format!(
                "the field would hold {total} options, and no field may hold more than {GRAPH_MAX_OPTIONS}"
            )));
        }
    }
    let names: Vec<String> = options.iter().map(|o| o.name.clone()).collect();
    let taken = app.options_where(&f, "Name", &names).await?;
    for (i, o) in options.iter_mut().enumerate() {
        if let Some(t) = taken.iter().find(|t| t.name == o.name) {
            let owner = if t.read_only { f.link_source() } else { &f.id };
            return Err(option_error(i, &format!("names option {:?}, which field {owner} already has", o.name)));
        }
        o.id = new_id();
    }
    require_names_free_of_dependents(&app, &f, &names).await?;
    let add = app.resolve_option_parents(&f, &options, None).await?;
    if !add.is_empty() {
        app.validate_option_edges(&f, &add, &[], "app.property_field.options.create.app_error").await?;
    }
    app.write_options(&f, &options, true, &add, &[]).await?;
    let ids: Vec<String> = options.iter().map(|o| o.id.clone()).collect();
    let created_options = app.read_back(&f, &ids).await?;
    app.options_changed(&group.id, &f.id, &connection_id(&headers)).await;
    Ok(created(created_options))
}

/// `patchPropertyFieldOptions`.
async fn patch_options(
    St(app): St<State>,
    uri: Uri,
    Path(p): Path<Params>,
    headers: HeaderMap,
    auth: Result<Auth, AppError>,
    body: Bytes,
) -> ApiResult<Response> {
    let s = begin(&uri, &p, auth)?;
    let (group, f) = field_for_options(&app, &p).await?;
    let items = decode_options(&body)?;
    if !app.can_manage_options(&s, &f).await {
        return Err(err(StatusCode::FORBIDDEN, "api.property_field.options.no_permission.app_error"));
    }
    // PropertyService.UpdateFieldOptions
    let f = app.writable_field(&group, &f.id).await?;
    let mut options = prepare_options(&f, &items, true)?;
    let ids: Vec<String> = options.iter().map(|o| o.id.clone()).collect();
    let stored = app.options_where(&f, "ID", &ids).await?;
    let by_id: HashMap<&str, &FieldOption> = stored.iter().map(|o| (o.id.as_str(), o)).collect();
    let mut renamed = vec![];
    for (i, o) in options.iter_mut().enumerate() {
        let Some(cur) = by_id.get(o.id.as_str()) else {
            return Err(option_error(i, &format!("names option {:?}, which field {} does not have", o.id, f.id)));
        };
        if cur.read_only {
            return Err(option_error(
                i,
                &format!(
                    "names option {:?}, which field {} inherits from field {}; change it there instead",
                    o.id,
                    f.id,
                    f.link_source()
                ),
            ));
        }
        if o.color.is_none() {
            o.color = cur.color.clone();
        }
        if o.attrs.is_none() {
            o.attrs = cur.attrs.clone();
        }
        if cur.name != o.name {
            renamed.push(o.name.clone());
        }
    }
    if !renamed.is_empty() {
        let taken = app.options_where(&f, "Name", &renamed).await?;
        for (i, o) in options.iter().enumerate() {
            if let Some(t) = taken.iter().find(|t| t.name == o.name && t.id != o.id) {
                let owner = if t.read_only { f.link_source() } else { &f.id };
                return Err(option_error(
                    i,
                    &format!("renames option {:?} to {:?}, which field {owner} already has", o.id, o.name),
                ));
            }
        }
        require_names_free_of_dependents(&app, &f, &renamed).await?;
    }
    let add = app.resolve_option_parents(&f, &options, Some(&by_id)).await?;
    let replacing: Vec<String> = options.iter().filter(|o| o.parents.is_some()).map(|o| o.id.clone()).collect();
    let remove = app.parents_to_detach(&f.id, &replacing, &add).await?;
    if !add.is_empty() || !remove.is_empty() {
        app.validate_option_edges(&f, &add, &remove, "app.property_field.options.update.app_error").await?;
    }
    app.write_options(&f, &options, false, &add, &remove).await?;
    let updated = app.read_back(&f, &ids).await?;
    app.options_changed(&group.id, &f.id, &connection_id(&headers)).await;
    Ok(axum::Json(updated).into_response())
}

/// `deletePropertyFieldOptions`.
async fn delete_options(
    St(app): St<State>,
    uri: Uri,
    Path(p): Path<Params>,
    headers: HeaderMap,
    auth: Result<Auth, AppError>,
    body: Bytes,
) -> ApiResult<Response> {
    let s = begin(&uri, &p, auth)?;
    let (group, f) = field_for_options(&app, &p).await?;
    let items = body_array(&body, "property_field_option_ids")?;
    let mut ids = vec![];
    for v in items {
        match v {
            Value::String(s) => ids.push(s),
            Value::Null => ids.push(String::new()),
            v => {
                return Err(AppError::invalid_param("property_field_option_ids")
                    .detail(format!("json: cannot unmarshal {} into Go value of type string", kind(&v))));
            }
        }
    }
    check_option_count(ids.len())?;
    if !app.can_manage_options(&s, &f).await {
        return Err(err(StatusCode::FORBIDDEN, "api.property_field.options.no_permission.app_error"));
    }
    // PropertyService.DeleteFieldOptions
    let f = app.writable_field(&group, &f.id).await?;
    let mut seen = HashSet::new();
    for (i, id) in ids.iter().enumerate() {
        if id.is_empty() {
            return Err(option_error(i, "names no option"));
        }
        if !seen.insert(id) {
            return Err(option_error(i, &format!("names option {id:?}, which is already in the list")));
        }
    }
    let stored = app.options_where(&f, "ID", &ids).await?;
    let by_id: HashMap<&str, &FieldOption> = stored.iter().map(|o| (o.id.as_str(), o)).collect();
    for (i, id) in ids.iter().enumerate() {
        let Some(o) = by_id.get(id.as_str()) else {
            return Err(option_error(i, &format!("names option {id:?}, which field {} does not have", f.id)));
        };
        if o.read_only {
            return Err(option_error(
                i,
                &format!(
                    "names option {id:?}, which field {} inherits from field {}; delete it there instead",
                    f.id,
                    f.link_source()
                ),
            ));
        }
    }
    if let Some((at, child)) = blocking_child_below(&app, &f, &ids).await? {
        return Err(option_error(
            at,
            &format!(
                "names option {:?}, which option {child:?} is still below; remove that one too or move it first",
                ids[at]
            ),
        ));
    }
    let now = now_ms();
    let mut tx = app.db.begin().await?;
    let n = sqlx::query("UPDATE PropertyFields SET UpdateAt = GREATEST($1, UpdateAt + 1) WHERE ID = $2 AND GroupID = $3 AND UpdateAt = $4")
        .bind(now)
        .bind(&f.id)
        .bind(&f.group_id)
        .bind(f.update_at)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if n != 1 {
        return Err(err(StatusCode::CONFLICT, "app.property_field.update.conflict.app_error")
            .detail("concurrent modification detected; please retry"));
    }
    sqlx::query("UPDATE PropertyOptions SET DeleteAt = $1, UpdateAt = $1 WHERE FieldID = $2 AND ID = ANY($3)")
        .bind(now)
        .bind(&f.id)
        .bind(&ids)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "DELETE FROM PropertyOptionEdges WHERE FieldID = $1 AND (ChildOptionID = ANY($2) OR ParentOptionID = ANY($2))",
    )
    .bind(&f.id)
    .bind(&ids)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    app.options_changed(&group.id, &f.id, &connection_id(&headers)).await;
    Ok(crate::users::ok().into_response())
}

// ---------- values ----------

/// `hasTargetAccess`.
async fn target_access(app: &App, s: &Session, group: &Group, ot: &str, target: &str, write: bool) -> ApiResult<()> {
    match ot {
        "channel" => {
            if !write {
                if !app.sess_channel(s, target, "read_channel").await {
                    return Err(AppError::forbidden_perm("read_channel"));
                }
            } else {
                let c = app.get_channel(target).await?;
                let perm = match c.r#type.as_str() {
                    "O" => "manage_public_channel_properties",
                    "P" => "manage_private_channel_properties",
                    _ => "read_channel",
                };
                if !app.sess_channel(s, target, perm).await {
                    return Err(AppError::forbidden_perm(perm));
                }
            }
        }
        "post" => {
            let post = app.single_post(target, false).await?;
            let perm = if write { "create_post" } else { "read_channel" };
            if !app.sess_channel(s, &post.channel_id, perm).await {
                return Err(AppError::forbidden_perm(perm));
            }
        }
        "user" => {
            if target == s.user_id {
                return Ok(());
            }
            if !write {
                if !app.can_see_user(s, target).await? {
                    return Err(AppError::forbidden_perm("view_members"));
                }
                return Ok(());
            }
            if !app.has_perm(s, crate::perm::EDIT_OTHER_USERS).await
                && !(group.name == ACCESS_CONTROL && app.require_manage_bot(s, target).await.is_ok())
            {
                return Err(AppError::forbidden_perm(crate::perm::EDIT_OTHER_USERS));
            }
            if !app.has_perm(s, MANAGE_SYSTEM).await {
                let u = app.user(target).await?.ok_or_else(crate::users::missing_account)?;
                if u.is_system_admin() {
                    return Err(AppError::forbidden_perm(MANAGE_SYSTEM));
                }
            }
        }
        "system" => {
            if write && !app.has_perm(s, MANAGE_SYSTEM).await {
                return Err(AppError::forbidden_perm(MANAGE_SYSTEM));
            }
        }
        "template" => {
            return Err(
                bad("api.property_value.template_no_values.app_error").detail("template fields cannot have values")
            );
        }
        _ => return Err(bad("api.property_value.invalid_object_type.app_error")),
    }
    Ok(())
}

/// The object type and target of the value routes: `template` and `system` objects are refused before the target
/// id is checked.
fn value_target(p: &Params, what: &str) -> ApiResult<(String, String)> {
    let ot = require_object_type(p)?;
    if ot == "template" {
        return Err(bad("api.property_value.template_no_values.app_error").detail("template fields cannot have values"));
    }
    if ot == "system" {
        return Err(bad("api.property_value.system_use_dedicated_route.app_error")
            .detail(format!("system values must use the dedicated system values endpoint{what}")));
    }
    let t = param(p, "target_id");
    if !is_valid_id(t) {
        return Err(AppError::invalid_url_param("target_id"));
    }
    Ok((ot, t.to_string()))
}

/// `getPropertyValues`.
async fn get_values(
    St(app): St<State>,
    uri: Uri,
    Path(p): Path<Params>,
    Query(q): Q,
    auth: Result<Auth, AppError>,
) -> ApiResult<Response> {
    let s = begin(&uri, &p, auth)?;
    let (ot, target) = value_target(&p, "")?;
    values_core(&app, &s, &p, &q, &ot, &target).await
}

/// `getSystemPropertyValues`.
async fn get_system_values(
    St(app): St<State>,
    uri: Uri,
    Path(p): Path<Params>,
    Query(q): Q,
    auth: Result<Auth, AppError>,
) -> ApiResult<Response> {
    let s = begin(&uri, &p, auth)?;
    values_core(&app, &s, &p, &q, "system", SYSTEM_TARGET_ID).await
}

/// `getPropertyValuesCore`.
async fn values_core(
    app: &App,
    s: &Session,
    p: &Params,
    q: &HashMap<String, String>,
    ot: &str,
    target: &str,
) -> ApiResult<Response> {
    let group = app.v2_group(param(p, "group_name")).await?;
    require_channel_attribute_license(&group, &[ot.to_string()])?;
    target_access(app, s, &group, ot, target, false).await?;
    let since = query_i64(q, "since")?.unwrap_or(0);
    let (mut cursor_id, mut cursor_create, mut cursor_update) = (String::new(), 0, 0);
    if let Some(c) = q.get("cursor_id").filter(|c| !c.is_empty()) {
        cursor_id = c.clone();
        cursor_update = query_i64(q, "cursor_update_at")?.unwrap_or(0);
        cursor_create = query_i64(q, "cursor_create_at")?.unwrap_or(0);
    }
    // PropertyValueSearchOpts.IsValid
    let cursor_empty = cursor_id.is_empty() && cursor_create == 0 && cursor_update == 0;
    let invalid = if cursor_empty {
        None
    } else if !is_valid_id(&cursor_id) {
        Some("property value id is invalid")
    } else if (cursor_create > 0) == (cursor_update > 0) {
        Some("cursor must have exactly one of create_at or update_at set")
    } else if since > 0 && cursor_update == 0 {
        Some("cursor_update_at required when since is set")
    } else if since <= 0 && cursor_create == 0 {
        Some("cursor_create_at required when since is not set")
    } else {
        None
    };
    if let Some(e) = invalid {
        return Err(bad("api.property_value.get.invalid_opts.app_error").detail(e));
    }
    let per_page = per_page(q);
    if per_page < 1 {
        return Err(AppError::internal(
            "app.property_value.search.app_error",
            "per page must be positive integer greater than zero",
        ));
    }
    // SqlPropertyValueStore.SearchPropertyValues
    let delta = since > 0;
    let mut sql = Sql::new(&format!(
        "SELECT {VALUE_COLS} FROM PropertyValues WHERE GroupID = $1 AND TargetType = $2 AND TargetID = $3"
    ));
    sql.args.extend([Arg::S(group.id.clone()), Arg::S(ot.into()), Arg::S(target.into())]);
    if !cursor_empty {
        let (col, at) = if delta { ("UpdateAt", cursor_update) } else { ("CreateAt", cursor_create) };
        let a = sql.arg(Arg::I(at));
        let b = sql.arg(Arg::S(cursor_id));
        sql.sql += &format!(" AND ({col} > {a} OR ({col} = {a} AND Id > {b}))");
    }
    if delta {
        let a = sql.arg(Arg::I(since));
        sql.sql += &format!(" AND UpdateAt >= {a} ORDER BY UpdateAt ASC, Id ASC");
    } else {
        sql.sql += " AND DeleteAt = 0 ORDER BY CreateAt ASC, Id ASC";
    }
    sql.sql += &format!(" LIMIT {per_page}");
    let rows = sql.fetch_all(app).await?;
    let values = rows.iter().map(value_from_row).collect::<ApiResult<Vec<_>>>()?;
    if group.name == ACCESS_CONTROL && !values.is_empty() {
        return Err(license_error());
    }
    // A nil slice when nothing matched: Go answers `null`.
    if values.is_empty() {
        return Ok(axum::Json(Value::Null).into_response());
    }
    Ok(axum::Json(values).into_response())
}

/// `patchPropertyValues`.
async fn patch_values(
    St(app): St<State>,
    uri: Uri,
    Path(p): Path<Params>,
    headers: HeaderMap,
    auth: Result<Auth, AppError>,
    body: Bytes,
) -> ApiResult<Response> {
    let s = begin(&uri, &p, auth)?;
    let (ot, target) = value_target(&p, "")?;
    patch_values_core(&app, &s, &p, &headers, &body, &ot, &target).await
}

/// `patchSystemPropertyValues`.
async fn patch_system_values(
    St(app): St<State>,
    uri: Uri,
    Path(p): Path<Params>,
    headers: HeaderMap,
    auth: Result<Auth, AppError>,
    body: Bytes,
) -> ApiResult<Response> {
    let s = begin(&uri, &p, auth)?;
    patch_values_core(&app, &s, &p, &headers, &body, "system", SYSTEM_TARGET_ID).await
}

/// `SanitizePropertyValue`: a string trimmed; a list of strings trimmed, its blanks dropped.
fn sanitize_value(v: &Value) -> Value {
    match v {
        Value::String(s) => Value::String(s.trim().to_string()),
        Value::Array(a) if a.iter().all(Value::is_string) => Value::Array(
            a.iter()
                .filter_map(|x| x.as_str().map(str::trim).filter(|t| !t.is_empty()).map(|t| Value::String(t.into())))
                .collect(),
        ),
        v => v.clone(),
    }
}

/// `patchPropertyValuesCore`.
async fn patch_values_core(
    app: &App,
    s: &Session,
    p: &Params,
    headers: &HeaderMap,
    body: &[u8],
    ot: &str,
    target: &str,
) -> ApiResult<Response> {
    let group = app.v2_group(param(p, "group_name")).await?;
    require_channel_attribute_license(&group, &[ot.to_string()])?;
    let raw_items = body_array(body, "property_values")?;
    let mut items: Vec<(String, Option<Value>)> = vec![];
    for v in raw_items {
        match v {
            Value::Object(m) => {
                let fid = go_str(&m, "field_id").map_err(|e| AppError::invalid_param("property_values").detail(e))?;
                items.push((fid.unwrap_or_default(), m.get("value").cloned()));
            }
            Value::Null => items.push((String::new(), None)),
            v => {
                return Err(AppError::invalid_param("property_values").detail(format!(
                    "json: cannot unmarshal {} into Go value of type model.PropertyValuePatchItem",
                    kind(&v)
                )));
            }
        }
    }
    target_access(app, s, &group, ot, target, true).await?;
    if items.is_empty() {
        return Err(bad("api.property_value.patch.empty_body.app_error"));
    }
    if items.len() > MAX_VALUE_PATCH_ITEMS {
        return Err(bad("api.property_value.patch.too_many_items.request_error")
            .with_param("Max", &MAX_VALUE_PATCH_ITEMS.to_string()));
    }
    let mut seen = HashSet::new();
    for (fid, _) in &items {
        if !is_valid_id(fid) {
            return Err(bad("api.property_value.patch.invalid_field_id.app_error"));
        }
        if !seen.insert(fid.clone()) {
            return Err(bad("api.property_value.patch.duplicate_field_id.app_error"));
        }
    }
    let ids: Vec<String> = items.iter().map(|(f, _)| f.clone()).collect();
    let fields = app.property_fields_many(&group.id, &ids).await?;
    if group.name == ACCESS_CONTROL {
        return Err(license_error());
    }
    let by_id: HashMap<&str, &Field> = fields.iter().map(|f| (f.id.as_str(), f)).collect();
    for (fid, _) in &items {
        let Some(f) = by_id.get(fid.as_str()) else {
            return Err(err(StatusCode::NOT_FOUND, "api.property_value.patch.field_not_found.app_error")
                .with_param("FieldID", fid));
        };
        if f.object_type != ot {
            return Err(object_type_mismatch());
        }
        if !app.can_set_values(s, f, target).await {
            return Err(err(StatusCode::FORBIDDEN, "api.property_value.patch.no_values_permission.app_error"));
        }
    }
    // App.UpsertPropertyValues
    for (fid, _) in &items {
        let f = by_id[fid.as_str()];
        if f.delete_at != 0 {
            return Err(err(StatusCode::NOT_FOUND, "app.property_value.upsert.field_not_found.app_error")
                .with_param("FieldID", fid)
                .detail("field not found"));
        }
    }
    // PropertyService.UpsertPropertyValues: rejectTemplateValues, then the store.
    if fields.iter().any(|f| f.object_type == "template") {
        let f = fields.iter().find(|f| f.object_type == "template").unwrap();
        return Err(bad("app.property_value.template_no_values.app_error")
            .detail(format!("template field {:?} cannot have values", f.id)));
    }
    let now = now_ms();
    let mut tx = app.db.begin().await?;
    let mut out = vec![];
    for (fid, value) in &items {
        let Some(v) = value else {
            return Err(AppError::internal("app.property_value.upsert_many.app_error", "null value"));
        };
        let v = sanitize_value(v);
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "INSERT INTO PropertyValues (ID, TargetID, TargetType, GroupID, FieldID, Value, CreateAt, UpdateAt, DeleteAt, CreatedBy, UpdatedBy)
             VALUES ($1, $2, $3, $4, $5, $6::jsonb, $7, $7, 0, $8, $8)
             ON CONFLICT (GroupID, TargetID, FieldID) WHERE DeleteAt = 0 DO UPDATE SET Value = $6::jsonb, UpdateAt = $7, DeleteAt = 0,
             UpdatedBy = $8 RETURNING {VALUE_COLS}"
        )))
        .bind(new_id())
        .bind(target)
        .bind(ot)
        .bind(&group.id)
        .bind(fid)
        .bind(v.to_string())
        .bind(now)
        .bind(&s.user_id)
        .fetch_one(&mut *tx)
        .await?;
        out.push(value_from_row(&row)?);
    }
    tx.commit().await?;
    // the broadcast (resolveValueBroadcastParams)
    let mut b = Broadcast { omit_connection_id: connection_id(headers), ..Default::default() };
    let ok = match ot {
        "post" => match app.single_post(target, false).await {
            Ok(post) => {
                b.channel_id = post.channel_id;
                true
            }
            Err(_) => false,
        },
        "channel" => {
            b.channel_id = target.into();
            true
        }
        "user" | "system" => true,
        _ => false,
    };
    if ok {
        let withheld: Vec<Value> = out
            .iter()
            .map(|v| {
                let mut j = serde_json::to_value(v).unwrap_or_default();
                if !by_id.get(v.field_id.as_str()).is_some_and(|f| f.public()) {
                    j["value"] = json!({"withheld": true});
                }
                j
            })
            .collect();
        let values = order_value_json(&withheld, &out);
        app.hub.publish(
            "property_values_updated",
            json!({"object_type": ot, "target_id": target, "values": values}),
            b,
        );
    }
    Ok(axum::Json(out).into_response())
}

/// The values of a `property_values_updated` event as Go writes them: struct field order, values compacted.
fn order_value_json(withheld: &[Value], values: &[PropValue]) -> String {
    let items: Vec<String> = withheld
        .iter()
        .zip(values)
        .map(|(w, v)| {
            let value = if w["value"] == json!({"withheld": true}) { r#"{"withheld":true}"#.to_string() } else { v.value.get().to_string() };
            format!(
                r#"{{"id":{},"target_id":{},"target_type":{},"group_id":{},"field_id":{},"value":{value},"create_at":{},"update_at":{},"delete_at":{},"created_by":{},"updated_by":{}}}"#,
                json!(v.id),
                json!(v.target_id),
                json!(v.target_type),
                json!(v.group_id),
                json!(v.field_id),
                v.create_at,
                v.update_at,
                v.delete_at,
                json!(v.created_by),
                json!(v.updated_by)
            )
        })
        .collect();
    format!("[{}]", items.join(","))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compacts_jsonb_text() {
        assert_eq!(compact(r#"{"a": 1, "b": [1, 2], "c": "x y"}"#), r#"{"a":1,"b":[1,2],"c":"x y"}"#);
        assert_eq!(compact(r#""a \" b""#), r#""a \" b""#);
    }

    #[test]
    fn route_patterns() {
        let p = |g: &str, o: &str| {
            HashMap::from([("group_name".to_string(), g.to_string()), ("object_type".to_string(), o.to_string())])
        };
        assert!(route_ok(&p("access_control", "system")));
        assert!(!route_ok(&p("9abc", "system")));
        assert!(!route_ok(&p("_x", "system")));
        assert!(!route_ok(&p("Bad", "system")));
        assert!(!route_ok(&p("ok", "a1")));
    }

    #[test]
    fn sanitizes_values() {
        assert_eq!(sanitize_value(&json!(" a ")), json!("a"));
        assert_eq!(sanitize_value(&json!([" a", "", " "])), json!(["a"]));
        assert_eq!(sanitize_value(&json!(3)), json!(3));
    }
}

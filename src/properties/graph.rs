//! The option hierarchy of `graph` fields on the way in: parent names resolved to links, the links an option list or
//! an options payload replaces, the checks the hierarchy a change produces has to pass (parents per option, links per
//! field, no cycle, chain length), and the writes of the links.

// Based on Mattermost server/channels/app/properties/{graph_validation.go, graph_resolution.go, field_options.go},
// store/sqlstore/{property_field_option_edges.go, property_field_options.go} and public/model/{property_field.go,
// property_option.go}, Copyright (c) 2015-present Mattermost, Inc. Licensed under AGPL-3.0.

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use super::{Field, FieldOption, NewOption, option_error, options_refused};
use crate::app::{ApiResult, App, AppError};

/// `model.PropertyGraphMaxEdges`.
const MAX_EDGES: i64 = 1_000_000;
/// `model.PropertyGraphMaxDepth`.
const MAX_DEPTH: usize = 100;
/// `model.PropertyGraphMaxParentsPerOption`.
pub(super) const MAX_PARENTS: usize = 100;
/// `model.PropertyGraphMaxWalkRows`.
const MAX_WALK_ROWS: usize = 250_000;

type Tx<'a> = sqlx::Transaction<'a, sqlx::Postgres>;

/// `model.PropertyOptionEdge` of one field: the child option sits right below the parent.
#[derive(Clone, PartialEq, Eq, Hash)]
pub(super) struct Edge {
    pub child: String,
    pub parent: String,
}

/// `graphOptionOwnerID`: the field whose rows hold the hierarchy a field serves.
fn owner(f: &Field) -> &str {
    if f.is_linked() { f.link_source() } else { &f.id }
}

/// `invalid input` as `store.ErrInvalidInput` words it, answered as `mapPropertyServiceError` does.
pub(super) fn invalid_input(entity: &str, field: &str, value: &str, wrapped: &str) -> AppError {
    super::bad("app.property.invalid_input.app_error")
        .detail(format!("invalid input: entity: {entity} field: {field} value: {value} error: {wrapped}"))
}

/// `PropertyField.OptionParentLinks`: the links an inline option list asks for, and the options whose parents it
/// states in full. A parent is named, and the name resolves within the list itself.
pub(super) fn option_parent_links(f: &Field) -> Result<(Vec<Edge>, Vec<String>), String> {
    let Some(raw) = f.attr("options").filter(|v| !v.is_null()) else { return Ok(Default::default()) };
    let Value::Array(list) = raw else { return Err("the option list is not a list".into()) };
    let mut options = Vec::with_capacity(list.len());
    for (i, o) in list.iter().enumerate() {
        let Value::Object(m) = o else { return Err(format!("the option at index {i} is not an object")) };
        options.push(m);
    }
    let str_of =
        |m: &serde_json::Map<String, Value>, k: &str| m.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let mut ids_by_name: HashMap<String, String> = HashMap::new();
    let mut ambiguous = HashSet::new();
    for o in &options {
        let name = str_of(o, "name");
        if name.is_empty() {
            continue;
        }
        if ids_by_name.contains_key(&name) {
            ambiguous.insert(name);
            continue;
        }
        ids_by_name.insert(name, str_of(o, "id"));
    }
    let (mut add, mut replacing) = (vec![], vec![]);
    for (i, o) in options.iter().enumerate() {
        let Some(raw) = o.get("parents").filter(|v| !v.is_null()) else { continue };
        if f.kind != "graph" {
            return Err(format!(
                "the option at index {i} carries parents, and the options of a {} field form no hierarchy",
                f.kind
            ));
        }
        let parents: Option<Vec<&str>> = raw.as_array().and_then(|a| a.iter().map(Value::as_str).collect());
        let Some(parents) = parents else {
            return Err(format!("the parents of the option at index {i} are not a list of option names"));
        };
        let id = str_of(o, "id");
        if id.is_empty() {
            return Err(format!("the option at index {i} carries parents but has no id"));
        }
        replacing.push(id.clone());
        if parents.len() > MAX_PARENTS {
            return Err(format!(
                "the option at index {i} would have {} options directly above it, and no option may have more than {MAX_PARENTS}",
                parents.len()
            ));
        }
        let mut seen = HashSet::new();
        for p in parents {
            let pid = ids_by_name.get(p).map(String::as_str).unwrap_or("");
            if p.is_empty() {
                return Err(format!("the option at index {i} is put under an option with no name"));
            } else if seen.contains(p) {
                return Err(format!("the option at index {i} names {p:?} as a parent more than once"));
            } else if ambiguous.contains(p) {
                return Err(format!(
                    "the option at index {i} is put under {p:?}, and two of the field's options are called that"
                ));
            } else if pid.is_empty() {
                return Err(format!(
                    "the option at index {i} is put under {p:?}, which the field has no option called"
                ));
            } else if pid == id {
                return Err(format!("the option at index {i}, {:?}, is put under itself", str_of(o, "name")));
            }
            seen.insert(p);
            add.push(Edge { child: id.clone(), parent: pid.to_string() });
        }
    }
    Ok((add, replacing))
}

/// `optionEdgeChange`: the parents each touched option ends up with, and how far the field's link total moves.
fn edge_change(stored: &[Edge], add: &[Edge], remove: &[Edge]) -> (HashMap<String, HashSet<String>>, i64) {
    let mut after: HashMap<String, HashSet<String>> = HashMap::new();
    let mut before = 0i64;
    for e in stored {
        if after.entry(e.child.clone()).or_default().insert(e.parent.clone()) {
            before += 1;
        }
    }
    for e in remove {
        if let Some(s) = after.get_mut(&e.child) {
            s.remove(&e.parent);
        }
    }
    for e in add {
        after.entry(e.child.clone()).or_default().insert(e.parent.clone());
    }
    let total: i64 = after.values().map(|s| s.len() as i64).sum();
    (after, total - before)
}

/// `graphNeighbourhood`: the part of the hierarchy a change can reach, with the change folded in.
#[derive(Default)]
struct Neighbourhood {
    parents: HashMap<String, Vec<String>>,
    children: HashMap<String, Vec<String>>,
}

impl Neighbourhood {
    fn link(&mut self, child: &str, parent: &str) {
        self.parents.entry(child.to_string()).or_default().push(parent.to_string());
        self.children.entry(parent.to_string()).or_default().push(child.to_string());
    }
}

/// `reachable`: every option reached from `from` along `adj`, `from` only if something leads back to it.
fn reachable(from: &str, adj: &HashMap<String, Vec<String>>) -> HashSet<String> {
    let mut reached = HashSet::new();
    let mut pending: Vec<&String> = adj.get(from).map(|v| v.iter().collect()).unwrap_or_default();
    while let Some(o) = pending.pop() {
        if reached.insert(o.clone())
            && let Some(next) = adj.get(o)
        {
            pending.extend(next);
        }
    }
    reached
}

/// `longestChain`: how many options lie on the longest chain from `from` along `adj`; a cycle is an error. Walked
/// without recursion, since an option list may arrive as one chain of thousands.
fn longest_chain(
    from: &str,
    adj: &HashMap<String, Vec<String>>,
    memo: &mut HashMap<String, usize>,
) -> Result<usize, String> {
    const WALKING: usize = 0;
    match memo.get(from) {
        Some(&WALKING) => return Err(format!("option {from} sits above itself")),
        Some(&n) => return Ok(n),
        None => {}
    }
    memo.insert(from.to_string(), WALKING);
    let mut stack: Vec<(String, usize)> = vec![(from.to_string(), 0)];
    while let Some((node, idx)) = stack.last_mut() {
        let next = adj.get(node.as_str()).and_then(|v| v.get(*idx)).cloned();
        if let Some(n) = next {
            *idx += 1;
            match memo.get(&n) {
                Some(&WALKING) => return Err(format!("option {n} sits above itself")),
                Some(_) => {}
                None => {
                    memo.insert(n.clone(), WALKING);
                    stack.push((n, 0));
                }
            }
            continue;
        }
        let longest = 1 + adj.get(node.as_str()).map(|v| v.iter().map(|n| memo[n]).max().unwrap_or(0)).unwrap_or(0);
        memo.insert(node.clone(), longest);
        stack.pop();
    }
    Ok(memo[from])
}

fn dedup(ids: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut seen = HashSet::new();
    ids.into_iter().filter(|i| seen.insert(i.clone())).collect()
}

impl App {
    /// `GetOptionParentEdges`: the links landing on these options of the field's own hierarchy.
    async fn option_parent_edges(&self, field_id: &str, children: &[String]) -> ApiResult<Vec<Edge>> {
        if children.is_empty() {
            return Ok(vec![]);
        }
        let rows: Vec<(String, String)> =
            sqlx::query_as("SELECT ChildOptionID, ParentOptionID FROM PropertyOptionEdges WHERE FieldID = $1 AND ChildOptionID = ANY($2)")
                .bind(field_id)
                .bind(children)
                .fetch_all(&self.db)
                .await?;
        Ok(rows.into_iter().map(|(child, parent)| Edge { child, parent }).collect())
    }

    /// `walkOptionHierarchy`: each live option asked about with every option at or above it (`up`) or at or below
    /// it, refused past `PropertyGraphMaxWalkRows` rows rather than answered in part.
    async fn walk_options(
        &self,
        f: &Field,
        ids: &[String],
        up: bool,
        fail: &str,
    ) -> ApiResult<HashMap<String, Vec<String>>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let (step_to, standing_on) =
            if up { ("ParentOptionID", "ChildOptionID") } else { ("ChildOptionID", "ParentOptionID") };
        let rows: Vec<(String, String)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "WITH RECURSIVE hierarchy (SeedID, OptionID) AS (
               SELECT opt.ID, opt.ID FROM PropertyOptions opt WHERE opt.FieldID = $1 AND opt.ID = ANY($2) AND opt.DeleteAt = 0
               UNION SELECT walk.SeedID, edge.{step_to} FROM PropertyOptionEdges edge
               JOIN hierarchy walk ON walk.OptionID = edge.{standing_on} WHERE edge.FieldID = $1)
             SELECT SeedID, OptionID FROM hierarchy LIMIT $3"
        )))
        .bind(owner(f))
        .bind(ids)
        .bind(MAX_WALK_ROWS as i64 + 1)
        .fetch_all(&self.db)
        .await?;
        if rows.len() > MAX_WALK_ROWS {
            return Err(AppError::internal(
                fail,
                format!(
                    "property field {} hierarchy walk exceeded its {MAX_WALK_ROWS} row bound; refused rather than returning a partial result",
                    f.id
                ),
            ));
        }
        let mut out: HashMap<String, Vec<String>> = HashMap::new();
        for (seed, o) in rows {
            out.entry(seed).or_default().push(o);
        }
        Ok(out)
    }

    /// `GetOptionChildren`: the options right below each of these.
    async fn option_children(&self, f: &Field, ids: &[String]) -> ApiResult<Vec<(String, String)>> {
        if ids.is_empty() {
            return Ok(vec![]);
        }
        Ok(sqlx::query_as("SELECT ParentOptionID, ChildOptionID FROM PropertyOptionEdges WHERE FieldID = $1 AND ParentOptionID = ANY($2)")
            .bind(owner(f))
            .bind(ids)
            .fetch_all(&self.db)
            .await?)
    }

    /// `loadGraphNeighbourhood`: everything above the parents and below the children the change names, and the stored
    /// links among them, less `remove`, plus `add`.
    async fn neighbourhood(&self, f: &Field, add: &[Edge], remove: &[Edge], fail: &str) -> ApiResult<Neighbourhood> {
        let children = dedup(add.iter().map(|e| e.child.clone()));
        let parents = dedup(add.iter().map(|e| e.parent.clone()));
        let above = self.walk_options(f, &parents, true, fail).await?;
        let below = self.walk_options(f, &children, false, fail).await?;
        let region = dedup(above.into_values().chain(below.into_values()).flatten());
        let stored = self.option_children(f, &region).await?;
        let removed: HashSet<(&str, &str)> = remove.iter().map(|e| (e.child.as_str(), e.parent.as_str())).collect();
        let mut n = Neighbourhood::default();
        for (parent, child) in &stored {
            if !removed.contains(&(child.as_str(), parent.as_str())) {
                n.link(child, parent);
            }
        }
        for e in add {
            n.link(&e.child, &e.parent);
        }
        Ok(n)
    }

    /// `ValidateOptionEdges` as `optionsChangeFromValidation` answers it: a change to a graph field's hierarchy
    /// checked as a whole, a rejection as `app.property_field.options.invalid.app_error` with its reason, a hierarchy
    /// that could not be read as a server error under `fail`.
    pub(super) async fn validate_option_edges(
        &self,
        f: &Field,
        add: &[Edge],
        remove: &[Edge],
        fail: &str,
    ) -> ApiResult<()> {
        if f.kind != "graph" {
            return Err(options_refused(&format!(
                "property field {} is of type {}, whose options form no hierarchy",
                f.id, f.kind
            )));
        }
        if add.is_empty() {
            return Ok(());
        }
        let touched = dedup(add.iter().chain(remove).map(|e| e.child.clone()));
        let stored = self.option_parent_edges(&f.id, &touched).await?;
        let (after, delta) = edge_change(&stored, add, remove);
        for e in add {
            let count = after.get(&e.child).map_or(0, HashSet::len);
            if count > MAX_PARENTS {
                return Err(options_refused(&format!(
                    "invalid options: option {} would have {count} options directly above it, and no option may have more than {MAX_PARENTS}",
                    e.child
                )));
            }
        }
        let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM PropertyOptionEdges WHERE FieldID = $1")
            .bind(&f.id)
            .fetch_one(&self.db)
            .await?;
        if total + delta > MAX_EDGES {
            return Err(options_refused(&format!(
                "invalid options: the field would hold {} parent links between its options, and no field may hold more than {MAX_EDGES}",
                total + delta
            )));
        }
        let n = self.neighbourhood(f, add, remove, fail).await?;
        // WouldCreateCycle: a link closes a cycle when its child is already at or above its parent.
        let mut reached_up: HashMap<&str, HashSet<String>> = HashMap::new();
        for e in add {
            let above = reached_up.entry(e.parent.as_str()).or_insert_with(|| reachable(&e.parent, &n.parents));
            if above.contains(&e.child) {
                return Err(options_refused("invalid options: the change would put an option below itself"));
            }
        }
        // DepthAfterAdding: the longest chain through a link is everything above its parent and below its child.
        let (mut up, mut down) = (HashMap::new(), HashMap::new());
        let mut deepest = 0;
        for e in add {
            let measure = |r: Result<usize, String>| {
                r.map_err(|m| {
                    AppError::internal(
                        fail,
                        format!("failed to measure a graph property field's option hierarchy: {m}"),
                    )
                })
            };
            let above = measure(longest_chain(&e.parent, &n.parents, &mut up))?;
            let below = measure(longest_chain(&e.child, &n.children, &mut down))?;
            deepest = deepest.max(above + below);
        }
        if deepest > MAX_DEPTH {
            return Err(options_refused(&format!(
                "invalid options: the change would put {deepest} options on one chain, and no chain may be longer than {MAX_DEPTH}"
            )));
        }
        Ok(())
    }

    /// `parentsToDetachFor`: the stored links of these options that `add` does not restate.
    pub(super) async fn parents_to_detach(
        &self,
        field_id: &str,
        replacing: &[String],
        add: &[Edge],
    ) -> ApiResult<Vec<Edge>> {
        if replacing.is_empty() {
            return Ok(vec![]);
        }
        let keep: HashSet<&Edge> = add.iter().collect();
        Ok(self.option_parent_edges(field_id, replacing).await?.into_iter().filter(|e| !keep.contains(e)).collect())
    }

    /// `validateOptionBlobLinks`: the hierarchy a field's inline option list asks for, checked against the field as
    /// stored (`None` for a field being created). Every inline option gets an id first.
    pub(super) async fn validate_option_blob_links(
        &self,
        f: &mut Field,
        stored: Option<&Field>,
        fail: &str,
    ) -> ApiResult<()> {
        super::ensure_option_ids(f, fail)?;
        let (add, replacing) = option_parent_links(f).map_err(|e| options_refused(&e))?;
        if add.is_empty() && replacing.is_empty() {
            return Ok(());
        }
        let field = stored.unwrap_or(f);
        let remove = self.parents_to_detach(&field.id, &replacing, &add).await?;
        self.validate_option_edges(field, &add, &remove, fail).await
    }

    /// `resolveOptionParents`: the parent names of a payload as links. A name resolves against the field's options
    /// and the names the payload gives its own; `stored` holds the payload's options as they are (`None` when they
    /// are being created).
    pub(super) async fn resolve_option_parents(
        &self,
        f: &Field,
        options: &[NewOption],
        stored: Option<&HashMap<&str, &FieldOption>>,
    ) -> ApiResult<Vec<Edge>> {
        let names: Vec<String> = options.iter().filter_map(|o| o.parents.as_ref()).flatten().cloned().collect();
        if names.is_empty() {
            return Ok(vec![]);
        }
        let existing = self.options_where(f, "Name", &names).await?;
        let mut by_name: HashMap<&str, (&str, bool)> =
            existing.iter().map(|o| (o.name.as_str(), (o.id.as_str(), o.read_only))).collect();
        for o in options {
            by_name.insert(&o.name, (&o.id, false));
        }
        for o in options {
            if let Some(cur) = stored.and_then(|s| s.get(o.id.as_str()))
                && cur.name != o.name
            {
                by_name.remove(cur.name.as_str());
            }
        }
        let mut add = vec![];
        for (i, o) in options.iter().enumerate() {
            let Some(parents) = &o.parents else { continue };
            for name in parents {
                match by_name.get(name.as_str()) {
                    None => {
                        return Err(option_error(
                            i,
                            &format!(
                                "puts option {:?} under {name:?}, which field {} has no option called",
                                o.name, f.id
                            ),
                        ));
                    }
                    Some((_, true)) => {
                        return Err(option_error(
                            i,
                            &format!(
                                "puts option {:?} under {name:?}, which field {} inherits from field {}; an option can only sit under one of the same field's options",
                                o.name,
                                f.id,
                                f.link_source()
                            ),
                        ));
                    }
                    Some((id, _)) if *id == o.id => {
                        return Err(option_error(i, &format!("puts option {:?} under itself", o.name)));
                    }
                    Some((id, _)) => add.push(Edge { child: o.id.clone(), parent: id.to_string() }),
                }
            }
        }
        Ok(add)
    }
}

/// `requireLiveGraphField`: links are only written on a live graph field.
pub(super) async fn require_live_graph_field(tx: &mut Tx<'_>, field_id: &str) -> ApiResult<()> {
    let kind: Option<String> =
        sqlx::query_scalar("SELECT Type::text FROM PropertyFields WHERE ID = $1 AND DeleteAt = 0")
            .bind(field_id)
            .fetch_optional(&mut **tx)
            .await?;
    match kind {
        None => Err(super::property_not_found()),
        Some(k) if k != "graph" => Err(invalid_input(
            "PropertyOptionEdge",
            "field_id",
            field_id,
            &format!("options of a {k} field have no hierarchy to link"),
        )),
        _ => Ok(()),
    }
}

/// `requireOwnedOptions`: both ends of a new link are live options of the field's own.
async fn require_owned_options(tx: &mut Tx<'_>, field_id: &str, edges: &[Edge]) -> ApiResult<()> {
    let ids = dedup(edges.iter().flat_map(|e| [e.child.clone(), e.parent.clone()]));
    if ids.is_empty() {
        return Ok(());
    }
    let live: HashSet<String> =
        sqlx::query_scalar("SELECT ID FROM PropertyOptions WHERE FieldID = $1 AND ID = ANY($2) AND DeleteAt = 0")
            .bind(field_id)
            .bind(&ids)
            .fetch_all(&mut **tx)
            .await?
            .into_iter()
            .collect();
    match ids.iter().find(|id| !live.contains(*id)) {
        Some(id) => Err(invalid_input(
            "PropertyOptionEdge",
            "option_id",
            id,
            &format!("field {field_id} has no live option {id} of its own"),
        )),
        None => Ok(()),
    }
}

/// The link writes of `MutateOptions`, after its options: removals first, then additions (an existing link is kept
/// with its `CreateAt`).
pub(super) async fn write_edges(
    tx: &mut Tx<'_>,
    field_id: &str,
    add: &[Edge],
    remove: &[Edge],
    now: i64,
) -> ApiResult<()> {
    require_owned_options(tx, field_id, add).await?;
    for e in remove {
        sqlx::query(
            "DELETE FROM PropertyOptionEdges WHERE FieldID = $1 AND ChildOptionID = $2 AND ParentOptionID = $3",
        )
        .bind(field_id)
        .bind(&e.child)
        .bind(&e.parent)
        .execute(&mut **tx)
        .await?;
    }
    for e in add {
        sqlx::query(
            "INSERT INTO PropertyOptionEdges (FieldID, ChildOptionID, ParentOptionID, CreateAt) VALUES ($1, $2, $3, $4)
             ON CONFLICT (FieldID, ChildOptionID, ParentOptionID) DO NOTHING",
        )
        .bind(field_id)
        .bind(&e.child)
        .bind(&e.parent)
        .bind(now)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

/// `applyOptionParentLinks`: every option whose parents the list stated ends up with exactly those; whether a link
/// moved.
pub(super) async fn apply_option_parent_links(
    tx: &mut Tx<'_>,
    f: &Field,
    add: &[Edge],
    replacing: &[String],
    now: i64,
) -> ApiResult<bool> {
    if f.kind != "graph" {
        return Err(invalid_input(
            "PropertyOptionEdge",
            "field_id",
            &f.id,
            &format!("options of a {} field have no hierarchy to link", f.kind),
        ));
    }
    let current: Vec<(String, String)> = if replacing.is_empty() {
        vec![]
    } else {
        sqlx::query_as("SELECT ChildOptionID, ParentOptionID FROM PropertyOptionEdges WHERE FieldID = $1 AND ChildOptionID = ANY($2)")
            .bind(&f.id)
            .bind(replacing)
            .fetch_all(&mut **tx)
            .await?
    };
    let keep: HashSet<&Edge> = add.iter().collect();
    let stored: HashSet<Edge> = current.into_iter().map(|(child, parent)| Edge { child, parent }).collect();
    let remove: Vec<Edge> = stored.iter().filter(|e| !keep.contains(e)).cloned().collect();
    let added = add.iter().filter(|e| !stored.contains(*e)).count();
    write_edges(tx, &f.id, add, &remove, now).await?;
    Ok(added > 0 || !remove.is_empty())
}

/// `takeOverLinkSourceOptions` + `takeOverLinkSourceOptionEdges`: a field unlinked from its template keeps the
/// options (and hierarchy) it served, as its own.
pub(super) async fn take_over_link_source(tx: &mut Tx<'_>, f: &Field, source: &str, now: i64) -> ApiResult<()> {
    sqlx::query(
        "INSERT INTO PropertyOptions (ID, GroupID, FieldID, Name, Color, Rank, SortOrder, Attrs, CreateAt, UpdateAt, DeleteAt)
         SELECT ID, $1, $2, Name, Color, Rank, SortOrder, Attrs, CreateAt, $3, 0 FROM PropertyOptions WHERE FieldID = $4 AND DeleteAt = 0
         ON CONFLICT (FieldID, ID) DO NOTHING",
    )
    .bind(&f.group_id)
    .bind(&f.id)
    .bind(now)
    .bind(source)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "INSERT INTO PropertyOptionEdges (FieldID, ChildOptionID, ParentOptionID, CreateAt)
         SELECT $1, edge.ChildOptionID, edge.ParentOptionID, edge.CreateAt FROM PropertyOptionEdges edge WHERE edge.FieldID = $2
         AND EXISTS (SELECT 1 FROM PropertyOptions own WHERE own.FieldID = $1 AND own.ID = edge.ChildOptionID AND own.DeleteAt = 0)
         AND EXISTS (SELECT 1 FROM PropertyOptions own WHERE own.FieldID = $1 AND own.ID = edge.ParentOptionID AND own.DeleteAt = 0)
         ON CONFLICT (FieldID, ChildOptionID, ParentOptionID) DO NOTHING",
    )
    .bind(&f.id)
    .bind(source)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adj(pairs: &[(&str, &str)]) -> HashMap<String, Vec<String>> {
        let mut m: HashMap<String, Vec<String>> = HashMap::new();
        for (a, b) in pairs {
            m.entry(a.to_string()).or_default().push(b.to_string());
        }
        m
    }

    #[test]
    fn longest_chain_counts_options_and_finds_cycles() {
        let a = adj(&[("a", "b"), ("b", "c"), ("a", "c")]);
        assert_eq!(longest_chain("a", &a, &mut HashMap::new()), Ok(3));
        assert_eq!(longest_chain("z", &a, &mut HashMap::new()), Ok(1));
        let c = adj(&[("a", "b"), ("b", "a")]);
        assert!(longest_chain("a", &c, &mut HashMap::new()).is_err());
        let long: Vec<(String, String)> = (0..50_000).map(|i| (i.to_string(), (i + 1).to_string())).collect();
        let refs: Vec<(&str, &str)> = long.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
        assert_eq!(longest_chain("0", &adj(&refs), &mut HashMap::new()), Ok(50_001));
    }

    #[test]
    fn edge_change_counts_what_moves() {
        let e = |c: &str, p: &str| Edge { child: c.into(), parent: p.into() };
        let (after, delta) =
            edge_change(&[e("x", "a"), e("x", "b")], &[e("x", "c"), e("x", "a")], &[e("x", "a"), e("x", "b")]);
        assert_eq!(after["x"].len(), 2);
        assert_eq!(delta, 0);
    }
}

//! User statuses: the platform's status service and the `/users/status` endpoints.

// Based on Mattermost server/channels/app/platform/status.go, app/status.go and api4/status.go,
// Copyright (c) 2015-present Mattermost, Inc. Licensed under AGPL-3.0.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use axum::extract::{Path, State as St};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::app::{ApiResult, App, AppError, Auth, Body, State, is_valid_id, now_ms};
use crate::db::Table;
use crate::hub::Broadcast;
use crate::model::Status;
use crate::model::status::{self, AWAY, DND, OFFLINE, ONLINE, OUT_OF_OFFICE};

pub fn routes() -> Router<State> {
    Router::new()
        .route("/api/v4/users/status/ids", post(statuses_by_ids))
        .route("/api/v4/users/{user_id}/status", get(get_status).put(update_status))
}

/// Offline statuses waiting to be written in one batch (`statusUpdateChan`).
#[derive(Default)]
pub struct OfflineQueue(Mutex<HashMap<String, Status>>);

impl App {
    fn statuses_on(&self) -> bool {
        self.cfg.bool("ServiceSettings.EnableUserStatuses")
    }

    /// `GetStatus`: the cache, then the database.
    pub async fn status(&self, user_id: &str) -> ApiResult<Option<Status>> {
        if let Some(s) = self.caches.status.get(user_id) {
            return Ok(Some(s));
        }
        Ok(crate::db::one(&self.db, "userid = $1", user_id).await?)
    }

    /// `GetAllStatuses`: the cached statuses, as user id -> status; none while statuses are off.
    pub fn all_cached_statuses(&self) -> Vec<(String, Value)> {
        if !self.statuses_on() {
            return vec![];
        }
        self.caches.status.values().into_iter().map(|s| (s.user_id, Value::String(s.status))).collect()
    }

    /// `GetUserStatusesByIds`: the cache, then the database (cached), then offline for the rest.
    pub async fn statuses(&self, ids: &[String]) -> ApiResult<Vec<Status>> {
        if !self.statuses_on() {
            return Ok(vec![]);
        }
        let mut out = Vec::with_capacity(ids.len());
        let mut missing = vec![];
        for id in ids {
            match self.caches.status.get(id) {
                Some(s) => out.push(s),
                None => missing.push(id.clone()),
            }
        }
        if !missing.is_empty() {
            let sql = format!("{} WHERE userid = ANY($1)", Status::SELECT);
            let found: Vec<Status> =
                sqlx::query_as(sqlx::AssertSqlSafe(sql)).bind(&missing).fetch_all(&self.db).await?;
            for s in &found {
                self.caches.status.put(s.user_id.clone(), s.clone());
            }
            missing.retain(|id| !found.iter().any(|s| &s.user_id == id));
            out.extend(found);
            out.extend(missing.iter().map(|id| Status::offline(id)));
        }
        Ok(out)
    }

    async fn save_status(&self, s: &Status) {
        let r = sqlx::query(
            "INSERT INTO Status (UserId, Status, Manual, LastActivityAt, DNDEndTime, PrevStatus) VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (UserId) DO UPDATE SET Status = EXCLUDED.Status, Manual = EXCLUDED.Manual,
             LastActivityAt = EXCLUDED.LastActivityAt, DNDEndTime = EXCLUDED.DNDEndTime, PrevStatus = EXCLUDED.PrevStatus",
        )
        .bind(&s.user_id)
        .bind(&s.status)
        .bind(s.manual)
        .bind(s.last_activity_at)
        .bind(s.dnd_end_time)
        .bind(&s.prev_status)
        .execute(&self.db)
        .await;
        if let Err(e) = r {
            eprintln!("failed to save status of {}: {e}", s.user_id);
        }
    }

    /// `BroadcastStatus`: to the user only; others ask `status/ids`. Nothing while the server is busy.
    pub fn broadcast_status(&self, s: &Status) {
        if crate::admin::is_busy() {
            // A non-critical service, off while the server is busy.
            return;
        }
        self.hub.publish(
            "status_change",
            json!({"status": s.status, "user_id": s.user_id}),
            Broadcast { user_id: s.user_id.clone(), ..Default::default() },
        );
    }

    /// `SaveAndBroadcastStatus`.
    pub async fn save_and_broadcast_status(&self, s: Status) {
        self.caches.status.put(s.user_id.clone(), s.clone());
        self.save_status(&s).await;
        self.broadcast_status(&s);
    }

    /// `SetStatusOnline`.
    pub async fn set_status_online(&self, user_id: &str, manual: bool) {
        if !self.statuses_on() {
            return;
        }
        let now = now_ms();
        let (mut s, broadcast, old) = match self.status(user_id).await.ok().flatten() {
            None => (
                Status { user_id: user_id.into(), status: ONLINE.into(), last_activity_at: now, ..Default::default() },
                true,
                (OFFLINE.to_string(), 0, false),
            ),
            Some(s) => {
                if s.manual && !manual {
                    return;
                }
                let old = (s.status.clone(), s.last_activity_at, s.manual);
                (s, false, old)
            }
        };
        let broadcast = broadcast || s.status != ONLINE;
        s.status = ONLINE.into();
        s.manual = false;
        s.last_activity_at = now;
        self.caches.status.put(user_id.into(), s.clone());
        if s.status != old.0 || s.manual != old.2 || s.last_activity_at - old.1 > status::MIN_UPDATE_TIME {
            if broadcast {
                self.save_status(&s).await;
            } else {
                let _ = sqlx::query("UPDATE Status SET LastActivityAt = $1 WHERE UserId = $2")
                    .bind(s.last_activity_at)
                    .bind(user_id)
                    .execute(&self.db)
                    .await;
            }
        }
        if broadcast {
            self.broadcast_status(&s);
        }
    }

    /// `QueueSetStatusOffline`: the last connection is gone; written with the next batch.
    pub async fn queue_status_offline(&self, user_id: &str, manual: bool) {
        if !self.statuses_on() {
            return;
        }
        if let Some(s) = self.status(user_id).await.ok().flatten()
            && s.manual
            && !manual
        {
            return;
        }
        let s = Status {
            user_id: user_id.into(),
            status: OFFLINE.into(),
            manual,
            last_activity_at: now_ms(),
            ..Default::default()
        };
        self.offline_queue.0.lock().unwrap().insert(user_id.into(), s);
    }

    /// `processStatusUpdates`: every 500 ms.
    pub async fn flush_offline_statuses(&self) {
        let batch: Vec<Status> = self.offline_queue.0.lock().unwrap().drain().map(|(_, s)| s).collect();
        for s in batch {
            // A connection may have come back meanwhile.
            if self.hub.is_connected(&s.user_id) {
                continue;
            }
            self.caches.status.put(s.user_id.clone(), s.clone());
            self.save_status(&s).await;
            self.broadcast_status(&s);
        }
    }

    /// `SetStatusOffline`.
    pub async fn set_status_offline(&self, user_id: &str, manual: bool, force: bool) {
        if !self.statuses_on() {
            return;
        }
        if let Some(s) = self.status(user_id).await.ok().flatten()
            && !force
            && s.manual
            && !manual
        {
            return;
        }
        let s = Status {
            user_id: user_id.into(),
            status: OFFLINE.into(),
            manual,
            last_activity_at: now_ms(),
            ..Default::default()
        };
        self.save_and_broadcast_status(s).await;
    }

    /// `SetStatusAwayIfNeeded`.
    pub async fn set_status_away_if_needed(&self, user_id: &str, manual: bool) {
        if !self.statuses_on() {
            return;
        }
        let mut s = self.status(user_id).await.ok().flatten().unwrap_or_else(|| Status {
            user_id: user_id.into(),
            status: OFFLINE.into(),
            manual,
            ..Default::default()
        });
        if !manual && s.manual {
            return;
        }
        if !manual {
            let timeout = self.cfg.i64("TeamSettings.UserStatusAwayTimeout") * 1000;
            if s.status == AWAY || now_ms() - s.last_activity_at < timeout {
                return;
            }
        }
        s.status = AWAY.into();
        s.manual = manual;
        s.active_channel.clear();
        self.save_and_broadcast_status(s).await;
    }

    /// `SetStatusDoNotDisturb` / `SetStatusDoNotDisturbTimed` (end in unix seconds, cut to 15 min).
    pub async fn set_status_dnd(&self, user_id: &str, end: Option<i64>) {
        if !self.statuses_on() {
            return;
        }
        let mut s = self.status(user_id).await.ok().flatten().unwrap_or_else(|| Status::offline(user_id));
        if let Some(end) = end {
            s.prev_status = s.status.clone();
            s.dnd_end_time = end - end.rem_euclid(15 * 60);
        }
        s.status = DND.into();
        s.manual = true;
        self.save_and_broadcast_status(s).await;
    }

    /// `SetStatusOutOfOffice`.
    pub async fn set_status_ooo(&self, user_id: &str) {
        let mut s = self.status(user_id).await.ok().flatten().unwrap_or_else(|| Status {
            user_id: user_id.into(),
            status: OUT_OF_OFFICE.into(),
            ..Default::default()
        });
        s.status = OUT_OF_OFFICE.into();
        s.manual = true;
        self.save_and_broadcast_status(s).await;
    }

    /// `UpdateDNDStatusOfUsers`: timed DND ended; back to the status before it.
    pub async fn expire_dnd(&self) {
        let now = now_ms() / 1000;
        let sql = format!("{} WHERE Status = 'dnd' AND DNDEndTime > 0 AND DNDEndTime <= $1", Status::SELECT);
        let Ok(due) = sqlx::query_as::<_, Status>(sqlx::AssertSqlSafe(sql)).bind(now).fetch_all(&self.db).await else {
            return;
        };
        for mut s in due {
            s.status = if s.prev_status.is_empty() { ONLINE.into() } else { std::mem::take(&mut s.prev_status) };
            s.dnd_end_time = 0;
            s.manual = false;
            self.save_and_broadcast_status(s).await;
        }
    }
}

/// Background work of the status service.
pub fn spawn_jobs(app: State) {
    let a = app.clone();
    tokio::spawn(async move {
        let mut t = tokio::time::interval(Duration::from_millis(500));
        loop {
            t.tick().await;
            a.flush_offline_statuses().await;
        }
    });
    tokio::spawn(async move {
        let mut t = tokio::time::interval(Duration::from_secs(60));
        loop {
            t.tick().await;
            app.expire_dnd().await;
        }
    });
}

async fn statuses_by_ids(St(app): St<State>, _: Auth, Body(ids): Body<Vec<String>>) -> ApiResult<Json<Vec<Status>>> {
    if ids.is_empty() {
        return Err(AppError::invalid_param("user_ids"));
    }
    if ids.iter().any(|i| !is_valid_id(i)) {
        return Err(AppError::invalid_param("user_ids"));
    }
    Ok(Json(app.statuses(&ids).await?))
}

async fn get_status(St(app): St<State>, Auth(s): Auth, Path(user_id): Path<String>) -> ApiResult<Json<Status>> {
    let user_id = s.resolve(&user_id);
    if !is_valid_id(&user_id) {
        return Err(AppError::invalid_url_param("user_id"));
    }
    let st = app.statuses(std::slice::from_ref(&user_id)).await?;
    st.into_iter()
        .next()
        .map(Json)
        .ok_or_else(|| AppError::not_found("app.status.get.missing.app_error", "Unable to find the status entry."))
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct StatusBody {
    user_id: String,
    status: String,
    dnd_end_time: i64,
}

/// `updateUserStatus`.
async fn update_status(
    St(app): St<State>,
    Auth(s): Auth,
    Path(user_id): Path<String>,
    Body(b): Body<StatusBody>,
) -> ApiResult<Json<Value>> {
    let user_id = s.resolve(&user_id);
    if !is_valid_id(&user_id) {
        return Err(AppError::invalid_url_param("user_id"));
    }
    if b.user_id != user_id {
        return Err(AppError::invalid_param("user_id"));
    }
    app.require_user(&s, &user_id).await?;
    if !app.statuses_on() {
        return Err(AppError::not_implemented("Statuses"));
    }
    match b.status.as_str() {
        ONLINE => app.set_status_online(&user_id, true).await,
        OFFLINE => app.set_status_offline(&user_id, true, false).await,
        AWAY => app.set_status_away_if_needed(&user_id, true).await,
        DND => app.set_status_dnd(&user_id, (b.dnd_end_time > 0).then_some(b.dnd_end_time)).await,
        OUT_OF_OFFICE => app.set_status_ooo(&user_id).await,
        _ => return Err(AppError::invalid_param("status")),
    }
    let st = app.statuses(std::slice::from_ref(&user_id)).await?;
    Ok(Json(serde_json::to_value(st.into_iter().next().unwrap_or_default()).unwrap()))
}

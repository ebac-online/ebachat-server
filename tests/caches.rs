//! The caches Go reads only off EBAChat's paths (licensed terms of service, AI recaps' `GetPosts`, view
//! restrictions of users without `view_members`, schemes of the system console): called directly against the
//! stand's database, each must hit when read again and be fresh after the write that invalidates it.
//! `tools/diff/caches.py` covers the caches on EBAChat's paths through the API.
//!
//! `MM_CONFIG=dev/run/go/config.json cargo test --test caches -- --ignored --nocapture` (no server needs to run).

use ebachat_server::app::{App, State, new_id, now_ms};
use ebachat_server::config::Config;
use ebachat_server::model::{Post, Session};
use ebachat_server::posts::GetPostsOpts;

async fn app() -> State {
    let cfg = Config::load().await.expect("MM_CONFIG");
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://mmuser:mmpass@localhost:5433/mattermost?sslmode=disable".into());
    let db = sqlx::PgPool::connect(&url).await.unwrap();
    let app = App::new(db, cfg).unwrap();
    app.hub.set_app(&app);
    app
}

fn hits(app: &App, cache: &str) -> u64 {
    app.caches.stats()[cache].hits
}
fn invalidations(app: &App, cache: &str) -> u64 {
    app.caches.stats()[cache].invalidations
}

#[tokio::test]
#[ignore]
async fn last_posts_cache() {
    let app = app().await;
    let (channel_id, user_id): (String, String) = sqlx::query_as(
        "SELECT c.Id, cm.UserId FROM Channels c JOIN ChannelMembers cm ON cm.ChannelId = c.Id
         WHERE c.Name LIKE 'bench-%' AND c.DeleteAt = 0 LIMIT 1",
    )
    .fetch_one(&app.db)
    .await
    .unwrap();
    let o = GetPostsOpts { channel_id: channel_id.clone(), per_page: 60, ..Default::default() };
    app.get_posts_cached(&o).await.unwrap();
    let h = hits(&app, "last_posts");
    app.get_posts_cached(&o).await.unwrap();
    assert_eq!(hits(&app, "last_posts"), h + 1, "the first page comes from the cache");
    let p = app
        .create_post(
            Post { channel_id: channel_id.clone(), user_id, message: "last posts cache".into(), ..Default::default() },
            false,
        )
        .await
        .unwrap();
    let l = app.get_posts_cached(&o).await.unwrap();
    assert_eq!(l.order.first(), Some(&p.id), "a new post drops the cached page");
    println!("LastPost: hit on the repeated page, fresh after a post");
}

#[tokio::test]
#[ignore]
async fn terms_of_service_cache() {
    let app = app().await;
    let id = new_id();
    sqlx::query("INSERT INTO TermsOfService (Id, CreateAt, UserId, Text) VALUES ($1, $2, '', 'test terms')")
        .bind(&id)
        .bind(now_ms() + 10_000_000)
        .execute(&app.db)
        .await
        .unwrap();
    let first = app.latest_terms_of_service_id().await.unwrap();
    let h = hits(&app, "terms_of_service");
    let again = app.latest_terms_of_service_id().await.unwrap();
    sqlx::query("DELETE FROM TermsOfService WHERE Id = $1").bind(&id).execute(&app.db).await.unwrap();
    assert_eq!(first.as_deref(), Some(id.as_str()), "the latest terms");
    assert_eq!(again, first);
    assert_eq!(hits(&app, "terms_of_service"), h + 1, "read again from the cache");
    println!("TermsOfService: hit on the repeated read");
}

#[tokio::test]
#[ignore]
async fn scheme_cache() {
    let app = app().await;
    let id = new_id();
    sqlx::query(
        "INSERT INTO Schemes (Id, Name, DisplayName, Description, CreateAt, UpdateAt, DeleteAt, Scope,
            DefaultTeamAdminRole, DefaultTeamUserRole, DefaultChannelAdminRole, DefaultChannelUserRole,
            DefaultTeamGuestRole, DefaultChannelGuestRole, DefaultPlaybookAdminRole, DefaultPlaybookMemberRole,
            DefaultRunAdminRole, DefaultRunMemberRole)
         VALUES ($1, $1, 'cache test', '', 0, 0, 0, 'team', 'team_admin', 'team_user', 'channel_admin',
            'channel_user', 'team_guest', 'channel_guest', '', '', '', '')",
    )
    .bind(&id)
    .execute(&app.db)
    .await
    .unwrap();
    let s = app.scheme(&id).await.unwrap();
    let h = hits(&app, "scheme");
    let again = app.scheme(&id).await.unwrap();
    sqlx::query("DELETE FROM Schemes WHERE Id = $1").bind(&id).execute(&app.db).await.unwrap();
    assert_eq!(s.default_channel_user_role, "channel_user");
    assert_eq!(again.id, id);
    assert_eq!(hits(&app, "scheme"), h + 1, "read again from the cache");
    println!("Scheme: hit on the repeated read");
}

#[tokio::test]
#[ignore]
async fn team_ids_cache_in_view_restrictions() {
    let app = app().await;
    let (user_id, roles): (String, String) =
        sqlx::query_as("SELECT Id, Roles FROM Users WHERE Username = 'bench0210'").fetch_one(&app.db).await.unwrap();
    // A user without view_members (system_guest has none): their views are restricted.
    sqlx::query("UPDATE Users SET Roles = 'system_guest' WHERE Id = $1").bind(&user_id).execute(&app.db).await.unwrap();
    app.invalidate_user(&user_id);
    let r = app.view_restrictions(&user_id).await;
    let h = hits(&app, "team_ids_for_user");
    let r2 = app.view_restrictions(&user_id).await;
    let hit = hits(&app, "team_ids_for_user") == h + 1;
    // Leaving the stand as it was.
    sqlx::query("UPDATE Users SET Roles = $2 WHERE Id = $1")
        .bind(&user_id)
        .bind(&roles)
        .execute(&app.db)
        .await
        .unwrap();
    app.invalidate_user(&user_id);
    let r = r.unwrap().expect("restricted");
    assert!(!r.channels.is_empty(), "the channels they are in");
    assert_eq!(r.teams, r2.unwrap().unwrap().teams);
    assert!(hit, "the team ids come from the cache the second time");
    let inv = invalidations(&app, "team_ids_for_user");
    app.invalidate_user_teams(&user_id).await;
    assert!(invalidations(&app, "team_ids_for_user") > inv, "a team change drops them");
    // Visibility goes through the same restrictions.
    let s = Session { user_id: user_id.clone(), roles: "system_guest".into(), ..Default::default() };
    assert!(app.can_see_user(&s, &user_id).await.unwrap());
    println!("Team (GetUserTeamIds): hit in the view restrictions, dropped on a team change");
}

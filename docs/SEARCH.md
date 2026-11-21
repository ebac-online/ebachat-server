# Post search

Database post search (`POST /teams/{id}/posts/search`, `POST /posts/search`), the way Go's SQL store does it
(`SearchPostsForUser`: full-text `to_tsvector`/`to_tsquery`, the term syntax of `search_params.rs`, channels the user
is a member of, permission filter after the query), with one deliberate difference: paging.

## Paging (unlike Go)

Go's database search answers at most the newest 100 matches on page 0, whatever `per_page` asks, and an empty list
for any later page, so older matches cannot be reached. Rust pages for real:

- `per_page`: default 60 as in Go; `<= 0` means 60; at most 1000.
- `page`: from 0 (negative means 0); posts `[page * per_page, page * per_page + per_page)` of the matches, newest
  first (ties by id, so pages never overlap or skip).
- Depth: the newest 10 000 matches; a page past that is empty.
- A page is cut in SQL (`ORDER BY CreateAt DESC, Id LIMIT per_page OFFSET page * per_page`): the sets of terms
  (words and hashtags) are OR'ed groups of one query, and what Go filters after its query is in the query too: the
  channels are the user's, less those `FilterPostsByChannelPermissions` drops (checked before the query), hashtags
  match case-insensitively (Postgres `upper()` on both sides, Go's `strings.ToUpper`), burn-on-read posts are left
  out. The reply count is computed only for the page's rows. `tests/search_paging.rs` checks the pages.
- The matches always come through the full-text indexes (Mattermost's `idx_posts_message_txt`,
  `idx_posts_hashtags_txt`) in a `MATERIALIZED` CTE, then are sorted: every page costs about the same, bound by the
  number of matches. The planner left to itself may walk `CreateAt` from the newest post instead, testing each post,
  which is quick for a word all over the recent posts and a scan of the whole table for a word only old posts have
  (bench: a word in 30 000 old posts, page 0 in 900 ms that way, 30 ms through the index).
- The answer keeps Go's shape (`order`, `posts`, `matches` when present; no `has_next`).

The clients page unchanged: the web app (`WEBAPP_SEARCH_PER_PAGE` = 20) asks `page + 1` while a page is not empty,
EBAChat while a page is as long as its `per_page`. With Go they stop after the first page; with Rust they go on to
older matches.

//! GitHub access in two phases:
//!
//! 1. REST search (`/search/issues`) finds candidate PRs. It is fast and
//!    already returns title, author, draft flag and timestamps.
//! 2. GraphQL `nodes(ids:)` fetches the expensive per-PR details in small
//!    chunks, with the fields split across parallel requests so no single
//!    request exceeds GitHub's ~10s execution limit (measured: `reviewDecision`
//!    alone costs ~100ms per PR, diff stats and check rollups similar).

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use jiff::Timestamp;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::task::JoinSet;

use crate::config::Config;
use crate::model::{CiState, Comment, Mergeable, Pr, Query, Review, ReviewDecision, ReviewState};

const API: &str = "https://api.github.com";
const USER_AGENT: &str = concat!("anvil/", env!("CARGO_PKG_VERSION"));

/// Keep the `repo:` list per query short enough for GitHub's search parser.
const REPOS_PER_QUERY: usize = 20;
/// One REST search page; we never paginate (see `SearchPage::truncated`).
const SEARCH_PAGE: usize = 100;
/// PR ids per GraphQL detail request.
const DETAIL_CHUNK: usize = 25;
/// Retries for 5xx responses (GitHub answers 502 when a query runs too long).
const RETRIES: usize = 2;

/// Field groups for the detail fetch. Each is cheap enough on its own for a
/// `DETAIL_CHUNK`-sized batch; together they are not.
const DETAIL_GROUPS: [&str; 5] = [
    "id additions deletions changedFiles mergeable",
    "id lastCommit: commits(last: 1) { nodes { commit { oid committedDate statusCheckRollup { state } } } }",
    "id reviewRequests(first: 20) { nodes { requestedReviewer { __typename ... on User { login } ... on Team { slug } } } } \
     latestReviews(first: 30) { nodes { author { login } state submittedAt } } \
     authors: commits(first: 50) { nodes { commit { author { user { login } } } } }",
    "id comments(last: 30) { nodes { author { __typename login } body createdAt } } \
     reviewBodies: reviews(last: 20) { nodes { author { __typename login } body state createdAt: submittedAt } }",
    "id reviewThreads(last: 20) { nodes { comments(last: 5) { nodes { author { __typename login } body createdAt } } } }",
];

const VIEWER_QUERY: &str = "query { viewer { login } }";

// ---------------------------------------------------------------------------
// Raw response shapes. Lenient (`default`) so nulls and partial objects
// deserialize instead of failing the whole run.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SearchPage {
    pub total_count: u64,
    pub items: Vec<SearchItem>,
}

impl SearchPage {
    /// How many matching PRs were not fetched because only one page is read.
    pub fn truncated(&self) -> u64 {
        self.total_count.saturating_sub(self.items.len() as u64)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SearchItem {
    pub node_id: String,
    pub number: u64,
    pub title: String,
    pub html_url: String,
    pub draft: bool,
    pub created_at: Option<Timestamp>,
    pub updated_at: Option<Timestamp>,
    pub user: Option<SearchUser>,
    /// `https://api.github.com/repos/{owner}/{name}`
    pub repository_url: String,
}

impl SearchItem {
    pub fn repo(&self) -> String {
        self.repository_url
            .rsplit('/')
            .take(2)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("/")
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SearchUser {
    pub login: String,
    #[serde(rename = "type")]
    pub kind: String,
}

/// Union of all detail groups; each group fills in its own fields.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RawDetail {
    pub id: String,
    pub additions: u32,
    pub deletions: u32,
    pub changed_files: u32,
    pub mergeable: Option<String>,
    pub last_commit: RawNodes<RawCommitNode>,
    pub review_requests: RawNodes<RawReviewRequest>,
    pub latest_reviews: RawNodes<RawReview>,
    pub authors: RawNodes<RawCommitNode>,
    pub comments: RawNodes<RawComment>,
    pub review_bodies: RawNodes<RawComment>,
    pub review_threads: RawNodes<RawThread>,
}

impl RawDetail {
    fn absorb(&mut self, other: RawDetail) {
        if self.id.is_empty() {
            self.id = other.id.clone();
        }
        if other.additions > 0 || other.deletions > 0 || other.changed_files > 0 {
            self.additions = other.additions;
            self.deletions = other.deletions;
            self.changed_files = other.changed_files;
        }
        if other.mergeable.is_some() {
            self.mergeable = other.mergeable;
        }
        if !other.last_commit.nodes.is_empty() {
            self.last_commit = other.last_commit;
        }
        if !other.review_requests.nodes.is_empty() {
            self.review_requests = other.review_requests;
        }
        if !other.latest_reviews.nodes.is_empty() {
            self.latest_reviews = other.latest_reviews;
        }
        if !other.authors.nodes.is_empty() {
            self.authors = other.authors;
        }
        if !other.comments.nodes.is_empty() {
            self.comments = other.comments;
        }
        if !other.review_bodies.nodes.is_empty() {
            self.review_bodies = other.review_bodies;
        }
        if !other.review_threads.nodes.is_empty() {
            self.review_threads = other.review_threads;
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RawNodes<T> {
    pub nodes: Vec<T>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RawActor {
    /// Only present where the query asks for `__typename`.
    #[serde(rename = "__typename")]
    pub typename: String,
    pub login: String,
}

/// An issue comment, a review body or an inline review comment.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RawComment {
    pub author: Option<RawActor>,
    pub body: String,
    /// Reviews only; pending ones are not submitted yet.
    pub state: Option<String>,
    pub created_at: Option<Timestamp>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RawThread {
    pub comments: RawNodes<RawComment>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RawReviewRequest {
    pub requested_reviewer: Option<RawReviewer>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RawReviewer {
    #[serde(rename = "__typename")]
    pub typename: String,
    pub login: Option<String>,
    pub slug: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RawReview {
    pub author: Option<RawActor>,
    pub state: String,
    pub submitted_at: Option<Timestamp>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RawCommitNode {
    pub commit: RawCommit,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RawCommit {
    pub oid: String,
    pub committed_date: Option<Timestamp>,
    pub status_check_rollup: Option<RawRollup>,
    pub author: Option<RawCommitAuthor>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RawRollup {
    pub state: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RawCommitAuthor {
    pub user: Option<RawActor>,
}

#[derive(Debug, Deserialize)]
struct GraphqlResponse<T> {
    data: Option<T>,
    #[serde(default)]
    errors: Vec<GraphqlError>,
}

#[derive(Debug, Deserialize)]
struct GraphqlError {
    message: String,
}

#[derive(Debug, Deserialize)]
struct ViewerData {
    viewer: RawActor,
}

#[derive(Debug, Deserialize)]
struct NodesData {
    nodes: Vec<Option<RawDetail>>,
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

pub fn discover_token() -> Result<String> {
    for var in ["GITHUB_TOKEN", "GH_TOKEN"] {
        if let Ok(t) = std::env::var(var) {
            let t = t.trim().to_string();
            if !t.is_empty() {
                return Ok(t);
            }
        }
    }
    bail!("set GITHUB_TOKEN or GH_TOKEN to a GitHub token")
}

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
}

fn is_retryable(status: reqwest::StatusCode) -> bool {
    status.is_server_error()
}

impl Client {
    pub fn new(token: &str) -> Result<Self> {
        use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue};
        let mut headers = HeaderMap::new();
        let mut auth = HeaderValue::from_str(&format!("Bearer {token}"))?;
        auth.set_sensitive(true);
        headers.insert(AUTHORIZATION, auth);
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/vnd.github+json"),
        );
        headers.insert(
            "X-GitHub-Api-Version",
            HeaderValue::from_static("2022-11-28"),
        );
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .default_headers(headers)
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self { http })
    }

    async fn send_with_retry(&self, build: impl Fn() -> reqwest::RequestBuilder) -> Result<String> {
        let mut attempt = 0;
        loop {
            let resp = build().send().await.context("sending request")?;
            let status = resp.status();
            let body = resp.text().await.context("reading response")?;
            if status.is_success() {
                return Ok(body);
            }
            if is_retryable(status) && attempt < RETRIES {
                attempt += 1;
                tokio::time::sleep(Duration::from_millis(500 * attempt as u64)).await;
                continue;
            }
            bail!(
                "GitHub returned {status}: {}",
                body.chars().take(300).collect::<String>()
            );
        }
    }

    async fn graphql<T: DeserializeOwned>(&self, query: &str, variables: Value) -> Result<T> {
        let payload = json!({ "query": query, "variables": variables });
        let body = self
            .send_with_retry(|| self.http.post(format!("{API}/graphql")).json(&payload))
            .await?;
        let parsed: GraphqlResponse<T> =
            serde_json::from_str(&body).context("decoding GraphQL response")?;
        if !parsed.errors.is_empty() {
            let msgs: Vec<_> = parsed.errors.iter().map(|e| e.message.as_str()).collect();
            if parsed.data.is_none() {
                bail!("GraphQL errors: {}", msgs.join("; "));
            }
            eprintln!("warning: partial GraphQL errors: {}", msgs.join("; "));
        }
        parsed
            .data
            .ok_or_else(|| anyhow!("GraphQL response had no data"))
    }

    pub async fn viewer(&self) -> Result<String> {
        let data: ViewerData = self.graphql(VIEWER_QUERY, json!({})).await?;
        Ok(data.viewer.login)
    }

    pub async fn search(&self, q: &str) -> Result<SearchPage> {
        let body = self
            .send_with_retry(|| {
                self.http.get(format!("{API}/search/issues")).query(&[
                    ("q", q),
                    ("per_page", &SEARCH_PAGE.to_string()),
                    ("sort", "updated"),
                    ("order", "desc"),
                ])
            })
            .await
            .with_context(|| format!("search `{q}`"))?;
        serde_json::from_str(&body).with_context(|| format!("decoding search `{q}`"))
    }

    async fn details(&self, ids: &[String], fields: &str) -> Result<Vec<RawDetail>> {
        let query = format!(
            "query($ids: [ID!]!) {{ nodes(ids: $ids) {{ ... on PullRequest {{ {fields} }} }} }}"
        );
        let data: NodesData = self
            .graphql(&query, json!({ "ids": ids }))
            .await
            .context("fetching PR details")?;
        Ok(data
            .nodes
            .into_iter()
            .flatten()
            .filter(|d| !d.id.is_empty())
            .collect())
    }
}

/// The searches to run for this config. Each returns PRs tagged with its `Query`.
pub fn search_queries(cfg: &Config) -> Vec<(Query, String)> {
    let mut base = String::from("is:open is:pr draft:false archived:false");
    for org in &cfg.orgs {
        base.push_str(&format!(" org:{org}"));
    }
    #[allow(non_snake_case)]
    let BASE = base.as_str();
    let mut out = Vec::new();

    // Review requests. `review-requested:@me` covers me and every team I'm in;
    // when specific teams are configured, ask for direct requests and each
    // team separately instead (search has no OR across qualifiers).
    let rr = &cfg.review_requests;
    let mut exclude = String::new();
    for team in &rr.exclude_teams {
        exclude.push_str(&format!(" -team-review-requested:{team}"));
    }
    if rr.teams.is_empty() {
        out.push((
            Query::ReviewRequested,
            format!("{BASE} review-requested:@me{exclude}"),
        ));
    } else {
        out.push((
            Query::ReviewRequested,
            format!("{BASE} user-review-requested:@me{exclude}"),
        ));
        for team in &rr.teams {
            out.push((
                Query::ReviewRequested,
                format!("{BASE} team-review-requested:{team}{exclude}"),
            ));
        }
    }

    out.push((Query::Authored, format!("{BASE} author:@me")));
    out.push((Query::Involves, format!("{BASE} involves:@me -author:@me")));
    for chunk in cfg.team.repos.chunks(REPOS_PER_QUERY) {
        let repos: Vec<String> = chunk.iter().map(|r| format!("repo:{r}")).collect();
        out.push((Query::TeamRepos, format!("{BASE} {}", repos.join(" "))));
    }
    out
}

pub async fn search_all(
    client: &Client,
    queries: Vec<(Query, String)>,
) -> Result<Vec<(Query, String, SearchPage)>> {
    let mut set = JoinSet::new();
    for (i, (query, q)) in queries.into_iter().enumerate() {
        let client = client.clone();
        set.spawn(async move {
            let page = client.search(&q).await;
            (i, query, q, page)
        });
    }
    let mut results = Vec::new();
    while let Some(joined) = set.join_next().await {
        let (i, query, q, page) = joined.context("search task panicked")?;
        results.push((i, query, q, page?));
    }
    results.sort_by_key(|(i, ..)| *i);
    Ok(results.into_iter().map(|(_, q, s, p)| (q, s, p)).collect())
}

/// Fetch details for `ids`, chunked and with field groups in parallel.
/// Returns one merged `RawDetail` per id that GitHub knew about.
pub async fn fetch_details(client: &Client, ids: &[String]) -> Result<Vec<RawDetail>> {
    let mut set = JoinSet::new();
    for chunk in ids.chunks(DETAIL_CHUNK) {
        for fields in DETAIL_GROUPS {
            let client = client.clone();
            let chunk = chunk.to_vec();
            set.spawn(async move { client.details(&chunk, fields).await });
        }
    }
    let mut merged: BTreeMap<String, RawDetail> = BTreeMap::new();
    while let Some(joined) = set.join_next().await {
        for d in joined.context("detail task panicked")?? {
            merged.entry(d.id.clone()).or_default().absorb(d);
        }
    }
    Ok(merged.into_values().collect())
}

// ---------------------------------------------------------------------------
// Assembly
// ---------------------------------------------------------------------------

fn parse_mergeable(s: Option<&str>) -> Mergeable {
    match s {
        Some("MERGEABLE") => Mergeable::Clean,
        Some("CONFLICTING") => Mergeable::Conflicting,
        _ => Mergeable::Unknown,
    }
}

fn parse_ci(s: Option<&str>) -> CiState {
    match s {
        Some("SUCCESS") => CiState::Success,
        Some("FAILURE" | "ERROR") => CiState::Failure,
        Some("PENDING" | "EXPECTED") => CiState::Pending,
        _ => CiState::None,
    }
}

fn parse_review_state(s: &str) -> Option<ReviewState> {
    Some(match s {
        "APPROVED" => ReviewState::Approved,
        "CHANGES_REQUESTED" => ReviewState::ChangesRequested,
        "COMMENTED" => ReviewState::Commented,
        "DISMISSED" => ReviewState::Dismissed,
        "PENDING" => ReviewState::Pending,
        _ => return None,
    })
}

/// `reviewDecision` is too expensive to fetch, so derive it from the latest
/// review of each reviewer: any outstanding change request wins, otherwise
/// any approval counts.
fn derive_decision(reviews: &[Review]) -> ReviewDecision {
    if reviews
        .iter()
        .any(|r| r.state == ReviewState::ChangesRequested)
    {
        ReviewDecision::ChangesRequested
    } else if reviews.iter().any(|r| r.state == ReviewState::Approved) {
        ReviewDecision::Approved
    } else {
        ReviewDecision::ReviewRequired
    }
}

/// Logins `@mentioned` in `body`, in order, without duplicates. An `@` glued
/// to the previous word (as in an email address) does not count.
pub fn mentions(body: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut prev: Option<char> = None;
    for (i, c) in body.char_indices() {
        let starts_word = prev.is_none_or(|p| !p.is_alphanumeric() && p != '@');
        if c == '@' && starts_word {
            let login: String = body[i + 1..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
                .collect();
            if !login.is_empty() && !out.iter().any(|m| m.eq_ignore_ascii_case(&login)) {
                out.push(login);
            }
        }
        prev = Some(c);
    }
    out
}

/// Flatten every kind of comment into one list, oldest first. Review bodies
/// are only comments when they say something.
fn collect_comments(d: &RawDetail) -> Vec<Comment> {
    let bodies = d
        .review_bodies
        .nodes
        .iter()
        .filter(|r| r.state.as_deref() != Some("PENDING") && !r.body.trim().is_empty());
    let threads = d
        .review_threads
        .nodes
        .iter()
        .flat_map(|t| &t.comments.nodes);
    let mut comments: Vec<Comment> = d
        .comments
        .nodes
        .iter()
        .chain(bodies)
        .chain(threads)
        .filter_map(|c| {
            let author = c.author.as_ref()?;
            Some(Comment {
                author: author.login.clone(),
                author_is_bot: author.typename == "Bot",
                created_at: c.created_at?,
                mentions: mentions(&c.body),
            })
        })
        .filter(|c| !c.author.is_empty())
        .collect();
    comments.sort_by_key(|c| c.created_at);
    comments
}

/// Deduplicate search hits by node id, remembering which queries found each.
pub fn candidates(searches: Vec<(Query, SearchPage)>) -> Vec<(SearchItem, BTreeSet<Query>)> {
    let mut by_id: BTreeMap<String, (SearchItem, BTreeSet<Query>)> = BTreeMap::new();
    for (query, page) in searches {
        for item in page.items {
            if item.node_id.is_empty() || item.created_at.is_none() || item.updated_at.is_none() {
                continue;
            }
            by_id
                .entry(item.node_id.clone())
                .or_insert_with(|| (item, BTreeSet::new()))
                .1
                .insert(query);
        }
    }
    by_id.into_values().collect()
}

/// Join candidates with their details. Candidates without details get
/// neutral defaults rather than being dropped.
pub fn assemble(
    candidates: Vec<(SearchItem, BTreeSet<Query>)>,
    details: Vec<RawDetail>,
) -> Vec<Pr> {
    let mut details: BTreeMap<String, RawDetail> =
        details.into_iter().map(|d| (d.id.clone(), d)).collect();

    let mut prs: Vec<Pr> = candidates
        .into_iter()
        .map(|(item, found_by)| {
            let d = details.remove(&item.node_id).unwrap_or_default();
            let comments = collect_comments(&d);

            let mut requested_users = Vec::new();
            let mut requested_teams = Vec::new();
            for rr in d.review_requests.nodes {
                let Some(r) = rr.requested_reviewer else {
                    continue;
                };
                match (r.typename.as_str(), r.login, r.slug) {
                    ("User", Some(login), _) => requested_users.push(login),
                    ("Team", _, Some(slug)) => requested_teams.push(slug),
                    _ => {}
                }
            }

            let reviews: Vec<Review> = d
                .latest_reviews
                .nodes
                .into_iter()
                .filter_map(|r| {
                    Some(Review {
                        author: r.author?.login,
                        state: parse_review_state(&r.state)?,
                        submitted_at: r.submitted_at?,
                    })
                })
                .collect();

            let last = d.last_commit.nodes.into_iter().next().map(|n| n.commit);
            let ci = parse_ci(
                last.as_ref()
                    .and_then(|c| c.status_check_rollup.as_ref())
                    .map(|r| r.state.as_str()),
            );

            let commit_authors: BTreeSet<String> = d
                .authors
                .nodes
                .into_iter()
                .filter_map(|n| n.commit.author?.user.map(|u| u.login))
                .filter(|l| !l.is_empty())
                .collect();

            let (author, author_is_bot) = match &item.user {
                Some(u) => (u.login.clone(), u.kind == "Bot"),
                None => ("ghost".to_string(), false),
            };
            let repo = item.repo();

            Pr {
                id: item.node_id,
                number: item.number,
                repo,
                title: item.title,
                url: item.html_url,
                is_draft: item.draft,
                author,
                author_is_bot,
                created_at: item.created_at.expect("filtered in candidates()"),
                updated_at: item.updated_at.expect("filtered in candidates()"),
                additions: d.additions,
                deletions: d.deletions,
                changed_files: d.changed_files,
                mergeable: parse_mergeable(d.mergeable.as_deref()),
                review_decision: derive_decision(&reviews),
                ci,
                head_sha: last.as_ref().map(|c| c.oid.clone()).unwrap_or_default(),
                last_commit_at: last.as_ref().and_then(|c| c.committed_date),
                requested_users,
                requested_teams,
                reviews,
                commit_authors: commit_authors.into_iter().collect(),
                comments,
                found_by,
            }
        })
        .collect();
    prs.sort_by(|a, b| a.repo.cmp(&b.repo).then(a.number.cmp(&b.number)));
    prs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, number: u64) -> SearchItem {
        SearchItem {
            node_id: id.into(),
            number,
            title: "t".into(),
            html_url: "u".into(),
            draft: false,
            created_at: Some("2026-10-01T00:00:00Z".parse().unwrap()),
            updated_at: Some("2026-10-02T00:00:00Z".parse().unwrap()),
            user: Some(SearchUser {
                login: "alice".into(),
                kind: "User".into(),
            }),
            repository_url: "https://api.github.com/repos/acme/widgets".into(),
        }
    }

    #[test]
    fn repo_is_derived_from_repository_url() {
        assert_eq!(item("x", 1).repo(), "acme/widgets");
    }

    #[test]
    fn candidates_dedupe_and_union_found_by() {
        let a = SearchPage {
            total_count: 2,
            items: vec![item("x", 1), item("y", 2)],
        };
        let b = SearchPage {
            total_count: 1,
            items: vec![item("x", 1)],
        };
        let c = candidates(vec![(Query::Authored, a), (Query::Involves, b)]);
        assert_eq!(c.len(), 2);
        let x = c.iter().find(|(i, _)| i.node_id == "x").unwrap();
        assert_eq!(x.1.len(), 2);
    }

    #[test]
    fn assemble_tolerates_nulls_and_missing_details() {
        let detail: RawDetail = serde_json::from_value(json!({
            "id": "x", "additions": 1, "deletions": 2, "changedFiles": 1,
            "mergeable": "CONFLICTING",
            "reviewRequests": { "nodes": [
                { "requestedReviewer": { "__typename": "User", "login": "me" } },
                { "requestedReviewer": { "__typename": "Team", "slug": "payments" } },
                { "requestedReviewer": null }
            ] },
            "latestReviews": { "nodes": [
                { "author": { "login": "bob" }, "state": "APPROVED", "submittedAt": "2026-10-01T12:00:00Z" },
                { "author": null, "state": "COMMENTED", "submittedAt": "2026-10-01T12:00:00Z" }
            ] },
            "lastCommit": { "nodes": [ { "commit": { "oid": "abc", "committedDate": "2026-10-02T00:00:00Z", "statusCheckRollup": null } } ] },
            "authors": { "nodes": [
                { "commit": { "author": { "user": { "login": "carol" } } } },
                { "commit": { "author": { "user": null } } }
            ] },
            "comments": { "nodes": [
                { "author": { "__typename": "User", "login": "bob" }, "body": "hey @me look", "createdAt": "2026-10-03T00:00:00Z" },
                { "author": { "__typename": "Bot", "login": "ci[bot]" }, "body": "built", "createdAt": "2026-10-03T01:00:00Z" },
                { "author": null, "body": "ghost", "createdAt": "2026-10-03T02:00:00Z" }
            ] },
            "reviewBodies": { "nodes": [
                { "author": { "__typename": "User", "login": "bob" }, "body": "", "state": "APPROVED", "createdAt": "2026-10-01T12:00:00Z" },
                { "author": { "__typename": "User", "login": "me" }, "body": "draft", "state": "PENDING", "createdAt": "2026-10-04T00:00:00Z" },
                { "author": { "__typename": "User", "login": "dave" }, "body": "LGTM", "state": "COMMENTED", "createdAt": "2026-10-02T00:00:00Z" }
            ] },
            "reviewThreads": { "nodes": [
                { "comments": { "nodes": [
                    { "author": { "__typename": "User", "login": "erin" }, "body": "nit", "createdAt": "2026-10-01T18:00:00Z" }
                ] } }
            ] }
        }))
        .unwrap();
        let mut no_user = item("y", 2);
        no_user.user = None;
        let prs = assemble(
            vec![
                (item("x", 1), BTreeSet::from([Query::ReviewRequested])),
                (no_user, BTreeSet::from([Query::Involves])),
            ],
            vec![detail],
        );
        assert_eq!(prs.len(), 2);
        let x = &prs[0];
        assert_eq!(x.mergeable, Mergeable::Conflicting);
        assert_eq!(x.review_decision, ReviewDecision::Approved);
        assert_eq!(x.ci, CiState::None);
        assert_eq!(x.requested_users, vec!["me"]);
        assert_eq!(x.requested_teams, vec!["payments"]);
        assert_eq!(x.reviews.len(), 1);
        assert_eq!(x.commit_authors, vec!["carol"]);
        let who: Vec<(&str, bool)> = x
            .comments
            .iter()
            .map(|c| (c.author.as_str(), c.author_is_bot))
            .collect();
        assert_eq!(
            who,
            vec![
                ("erin", false),
                ("dave", false),
                ("bob", false),
                ("ci[bot]", true)
            ],
            "oldest first; empty and pending review bodies and ghost comments dropped"
        );
        assert_eq!(x.comments[2].mentions, vec!["me"]);
        let y = &prs[1];
        assert_eq!(y.author, "ghost");
        assert_eq!(y.mergeable, Mergeable::Unknown);
        assert_eq!(y.review_decision, ReviewDecision::ReviewRequired);
        assert_eq!(y.additions, 0);
    }

    #[test]
    fn mentions_are_whole_handles_at_word_starts() {
        assert_eq!(
            mentions("@alice please, cc @Bob-1 and @alice again (@acme/payments)"),
            vec!["alice", "Bob-1", "acme"]
        );
        assert!(mentions("mail me at felipe@example.com or @@ nothing").is_empty());
        assert_eq!(mentions("(@carol)"), vec!["carol"]);
    }

    #[test]
    fn change_request_beats_approval_in_derived_decision() {
        let r = |a: &str, s| Review {
            author: a.into(),
            state: s,
            submitted_at: "2026-10-01T00:00:00Z".parse().unwrap(),
        };
        assert_eq!(
            derive_decision(&[
                r("a", ReviewState::Approved),
                r("b", ReviewState::ChangesRequested)
            ]),
            ReviewDecision::ChangesRequested
        );
        assert_eq!(
            derive_decision(&[r("a", ReviewState::Commented)]),
            ReviewDecision::ReviewRequired
        );
    }

    #[test]
    fn detail_groups_merge_by_id() {
        let mut a = RawDetail::default();
        a.absorb(RawDetail {
            id: "x".into(),
            additions: 5,
            ..Default::default()
        });
        a.absorb(RawDetail {
            id: "x".into(),
            mergeable: Some("MERGEABLE".into()),
            ..Default::default()
        });
        assert_eq!(a.id, "x");
        assert_eq!(a.additions, 5);
        assert_eq!(a.mergeable.as_deref(), Some("MERGEABLE"));
    }

    #[test]
    fn orgs_scope_every_query() {
        let mut cfg = Config {
            orgs: vec!["acme".into(), "acme-labs".into()],
            ..Default::default()
        };
        cfg.team.repos = vec!["acme/widgets".into()];
        for (_, q) in search_queries(&cfg) {
            assert!(q.contains(" org:acme org:acme-labs "), "{q}");
        }
    }

    #[test]
    fn default_review_query_is_plain_review_requested() {
        let qs = search_queries(&Config::default());
        let review: Vec<&str> = qs
            .iter()
            .filter(|(q, _)| *q == Query::ReviewRequested)
            .map(|(_, s)| s.as_str())
            .collect();
        assert_eq!(
            review,
            vec!["is:open is:pr draft:false archived:false review-requested:@me"]
        );
    }

    #[test]
    fn excluded_teams_are_negated_on_review_queries() {
        let mut cfg = Config::default();
        cfg.review_requests.exclude_teams = vec!["acme/bot".into(), "acme/noise".into()];
        let qs = search_queries(&cfg);
        let review: Vec<&str> = qs
            .iter()
            .filter(|(q, _)| *q == Query::ReviewRequested)
            .map(|(_, s)| s.as_str())
            .collect();
        assert_eq!(
            review,
            vec![
                "is:open is:pr draft:false archived:false review-requested:@me \
                 -team-review-requested:acme/bot -team-review-requested:acme/noise"
            ]
        );
        for (query, q) in &qs {
            if *query != Query::ReviewRequested {
                assert!(!q.contains("team-review-requested"), "{q}");
            }
        }
    }

    #[test]
    fn included_teams_split_into_direct_plus_one_query_per_team() {
        let mut cfg = Config::default();
        cfg.review_requests.teams = vec!["acme/payments".into(), "acme/platform".into()];
        cfg.review_requests.exclude_teams = vec!["acme/bot".into()];
        let qs = search_queries(&cfg);
        let review: Vec<&str> = qs
            .iter()
            .filter(|(q, _)| *q == Query::ReviewRequested)
            .map(|(_, s)| s.as_str())
            .collect();
        assert_eq!(
            review,
            vec![
                "is:open is:pr draft:false archived:false user-review-requested:@me \
                 -team-review-requested:acme/bot",
                "is:open is:pr draft:false archived:false team-review-requested:acme/payments \
                 -team-review-requested:acme/bot",
                "is:open is:pr draft:false archived:false team-review-requested:acme/platform \
                 -team-review-requested:acme/bot",
            ]
        );
        assert!(!qs.iter().any(|(_, q)| q.contains(" review-requested:@me")));
    }

    #[test]
    fn team_repos_are_chunked() {
        let mut cfg = Config::default();
        cfg.team.repos = (0..45).map(|i| format!("acme/r{i}")).collect();
        let qs = search_queries(&cfg);
        assert_eq!(qs.iter().filter(|(q, _)| *q == Query::TeamRepos).count(), 3);
        assert_eq!(qs.len(), 6);
    }
}

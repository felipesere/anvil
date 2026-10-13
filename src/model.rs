//! Normalised PR data and the "facts" derived from it relative to me.

use std::collections::BTreeSet;

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::config::Config;

/// Which search query returned a PR. A PR can be found by several.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Query {
    ReviewRequested,
    Authored,
    Involves,
    TeamRepos,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Mergeable {
    Clean,
    Conflicting,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReviewDecision {
    Approved,
    ChangesRequested,
    ReviewRequired,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CiState {
    Success,
    Failure,
    Pending,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReviewState {
    Approved,
    ChangesRequested,
    Commented,
    Dismissed,
    Pending,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthorTier {
    Team,
    Other,
    Bot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RepoTier {
    Team,
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Review {
    pub author: String,
    pub state: ReviewState,
    pub submitted_at: Timestamp,
}

/// Anything someone wrote on the PR: an issue comment, a review body or an
/// inline review comment. The body itself is not kept, only who it mentions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Comment {
    pub author: String,
    /// GitHub types the author as `Bot`.
    pub author_is_bot: bool,
    pub created_at: Timestamp,
    /// Logins `@mentioned` in the body, in order of appearance.
    pub mentions: Vec<String>,
}

/// A pull request as fetched from GitHub, independent of who is looking at it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pr {
    pub id: String,
    pub number: u64,
    /// `owner/name`
    pub repo: String,
    pub title: String,
    pub url: String,
    pub is_draft: bool,
    pub author: String,
    /// GitHub types the author as `Bot`.
    pub author_is_bot: bool,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    pub additions: u32,
    pub deletions: u32,
    pub changed_files: u32,
    pub mergeable: Mergeable,
    pub review_decision: ReviewDecision,
    pub ci: CiState,
    pub head_sha: String,
    pub last_commit_at: Option<Timestamp>,
    pub requested_users: Vec<String>,
    pub requested_teams: Vec<String>,
    /// Latest review per reviewer.
    pub reviews: Vec<Review>,
    /// Logins of commit authors (deduplicated, unknown users dropped).
    pub commit_authors: Vec<String>,
    /// Most recent comments of every kind, oldest first.
    pub comments: Vec<Comment>,
    pub found_by: BTreeSet<Query>,
}

/// Everything scoring needs, computed once relative to `me`, the config and `now`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrFacts {
    pub is_draft: bool,
    pub is_mine: bool,
    pub i_contributed: bool,
    pub author_tier: AuthorTier,
    pub repo_tier: RepoTier,
    pub lines: u32,
    pub age_days: f64,
    pub stale_days: f64,
    pub ci: CiState,
    pub has_conflicts: bool,
    pub review_decision: ReviewDecision,
    pub requested_me: bool,
    pub requested_my_team: bool,
    pub my_last_review: Option<ReviewState>,
    pub updated_since_my_review: bool,
    pub approved_by_other: bool,
    pub changes_requested_by_other: bool,
    /// Comments by other humans within `thresholds.recent_comment_days`.
    pub recent_comments: u32,
    /// Someone else `@mentioned` me and I have not commented or reviewed since.
    pub mentioned_me: bool,
}

pub fn days_between(earlier: Timestamp, later: Timestamp) -> f64 {
    (later.as_second() - earlier.as_second()) as f64 / 86_400.0
}

fn same_login(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

impl PrFacts {
    pub fn compute(pr: &Pr, cfg: &Config, me: &str, now: Timestamp) -> PrFacts {
        let is_mine = same_login(&pr.author, me);
        let i_contributed = is_mine || pr.commit_authors.iter().any(|a| same_login(a, me));

        let author_tier = if pr.author_is_bot || cfg.is_listed_bot(&pr.author) {
            AuthorTier::Bot
        } else if cfg.is_team_member(&pr.author) {
            AuthorTier::Team
        } else {
            AuthorTier::Other
        };
        let repo_tier = if cfg.is_team_repo(&pr.repo) {
            RepoTier::Team
        } else {
            RepoTier::Other
        };

        let requested_me = pr.requested_users.iter().any(|u| same_login(u, me));
        let requested_my_team = pr.found_by.contains(&Query::ReviewRequested) && !requested_me;

        let my_review = pr
            .reviews
            .iter()
            .filter(|r| same_login(&r.author, me))
            .max_by_key(|r| r.submitted_at);
        let my_last_review = my_review.map(|r| r.state);
        let updated_since_my_review = match (my_review, pr.last_commit_at) {
            (Some(r), Some(commit_at)) => commit_at > r.submitted_at,
            _ => false,
        };

        let approved_by_other = pr
            .reviews
            .iter()
            .any(|r| r.state == ReviewState::Approved && !same_login(&r.author, me));
        let changes_requested_by_other = pr
            .reviews
            .iter()
            .any(|r| r.state == ReviewState::ChangesRequested && !same_login(&r.author, me));

        let my_last_activity = pr
            .comments
            .iter()
            .filter(|c| same_login(&c.author, me))
            .map(|c| c.created_at)
            .chain(my_review.map(|r| r.submitted_at))
            .max();
        let by_other_humans = pr.comments.iter().filter(|c| {
            !same_login(&c.author, me) && !c.author_is_bot && !cfg.is_listed_bot(&c.author)
        });
        let recent_comments = by_other_humans
            .clone()
            .filter(|c| days_between(c.created_at, now) <= cfg.thresholds.recent_comment_days)
            .count() as u32;
        let mentioned_me = by_other_humans
            .filter(|c| my_last_activity.is_none_or(|t| c.created_at > t))
            .any(|c| c.mentions.iter().any(|m| same_login(m, me)));

        PrFacts {
            is_draft: pr.is_draft,
            is_mine,
            i_contributed,
            author_tier,
            repo_tier,
            lines: pr.additions + pr.deletions,
            age_days: days_between(pr.created_at, now).max(0.0),
            stale_days: days_between(pr.updated_at, now).max(0.0),
            ci: pr.ci,
            has_conflicts: pr.mergeable == Mergeable::Conflicting,
            review_decision: pr.review_decision,
            requested_me,
            requested_my_team,
            my_last_review,
            updated_since_my_review,
            approved_by_other,
            changes_requested_by_other,
            recent_comments,
            mentioned_me,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn pr() -> Pr {
        Pr {
            id: "PR_1".into(),
            number: 1,
            repo: "acme/widgets".into(),
            title: "t".into(),
            url: "u".into(),
            is_draft: false,
            author: "alice".into(),
            author_is_bot: false,
            created_at: ts("2026-10-01T00:00:00Z"),
            updated_at: ts("2026-10-08T00:00:00Z"),
            additions: 10,
            deletions: 5,
            changed_files: 2,
            mergeable: Mergeable::Clean,
            review_decision: ReviewDecision::ReviewRequired,
            ci: CiState::Success,
            head_sha: "abc".into(),
            last_commit_at: Some(ts("2026-10-07T00:00:00Z")),
            requested_users: vec![],
            requested_teams: vec![],
            reviews: vec![],
            commit_authors: vec!["alice".into()],
            comments: vec![],
            found_by: BTreeSet::new(),
        }
    }

    fn comment(author: &str, at: &str, mentions: &[&str]) -> Comment {
        Comment {
            author: author.into(),
            author_is_bot: false,
            created_at: ts(at),
            mentions: mentions.iter().map(|m| m.to_string()).collect(),
        }
    }

    fn cfg() -> Config {
        Config::parse("team:\n  members: [alice]\n  repos: [acme/widgets]\n").unwrap()
    }

    #[test]
    fn basic_facts() {
        let now = ts("2026-10-09T00:00:00Z");
        let f = PrFacts::compute(&pr(), &cfg(), "me", now);
        assert!(!f.is_mine);
        assert_eq!(f.author_tier, AuthorTier::Team);
        assert_eq!(f.repo_tier, RepoTier::Team);
        assert_eq!(f.lines, 15);
        assert_eq!(f.age_days, 8.0);
        assert_eq!(f.stale_days, 1.0);
    }

    #[test]
    fn team_request_is_inferred_from_query_membership() {
        let mut p = pr();
        p.found_by.insert(Query::ReviewRequested);
        let f = PrFacts::compute(&p, &cfg(), "me", ts("2026-10-09T00:00:00Z"));
        assert!(!f.requested_me);
        assert!(f.requested_my_team);

        p.requested_users.push("ME".into());
        let f = PrFacts::compute(&p, &cfg(), "me", ts("2026-10-09T00:00:00Z"));
        assert!(f.requested_me);
        assert!(!f.requested_my_team);
    }

    #[test]
    fn rereview_when_commits_land_after_my_review() {
        let mut p = pr();
        p.reviews.push(Review {
            author: "me".into(),
            state: ReviewState::ChangesRequested,
            submitted_at: ts("2026-10-06T00:00:00Z"),
        });
        let f = PrFacts::compute(&p, &cfg(), "me", ts("2026-10-09T00:00:00Z"));
        assert_eq!(f.my_last_review, Some(ReviewState::ChangesRequested));
        assert!(f.updated_since_my_review);
        assert!(!f.changes_requested_by_other);
    }

    #[test]
    fn recent_comments_count_only_other_humans_inside_the_window() {
        let now = ts("2026-10-09T00:00:00Z");
        let mut p = pr();
        p.comments = vec![
            comment("bob", "2026-10-01T00:00:00Z", &[]), // too old
            comment("bob", "2026-10-08T00:00:00Z", &[]),
            comment("me", "2026-10-08T12:00:00Z", &[]), // mine
            Comment {
                author_is_bot: true,
                ..comment("ci[bot]", "2026-10-08T13:00:00Z", &[])
            },
            comment("renovate[bot]", "2026-10-08T14:00:00Z", &[]), // listed bot
        ];
        let mut cfg = cfg();
        cfg.team.bots = vec!["renovate[bot]".into()];
        let f = PrFacts::compute(&p, &cfg, "me", now);
        assert_eq!(f.recent_comments, 1);
        assert!(!f.mentioned_me);
    }

    #[test]
    fn mention_counts_until_i_respond() {
        let now = ts("2026-10-09T00:00:00Z");
        let mut p = pr();
        p.comments = vec![comment("bob", "2026-10-02T00:00:00Z", &["carol", "ME"])];
        let f = PrFacts::compute(&p, &cfg(), "me", now);
        assert!(f.mentioned_me, "login match is case-insensitive");
        assert_eq!(f.recent_comments, 0, "an old mention is still a mention");

        p.comments.push(comment("me", "2026-10-03T00:00:00Z", &[]));
        let f = PrFacts::compute(&p, &cfg(), "me", now);
        assert!(!f.mentioned_me, "a later comment of mine answers it");

        p.comments.pop();
        p.reviews.push(Review {
            author: "me".into(),
            state: ReviewState::Commented,
            submitted_at: ts("2026-10-03T00:00:00Z"),
        });
        let f = PrFacts::compute(&p, &cfg(), "me", now);
        assert!(!f.mentioned_me, "a later review of mine answers it too");

        p.comments
            .push(comment("bob", "2026-10-04T00:00:00Z", &["me"]));
        let f = PrFacts::compute(&p, &cfg(), "me", now);
        assert!(
            f.mentioned_me,
            "a fresh mention after my reply counts again"
        );

        p.comments = vec![comment("me", "2026-10-05T00:00:00Z", &["me"])];
        let f = PrFacts::compute(&p, &cfg(), "me", now);
        assert!(!f.mentioned_me, "mentioning myself is not a mention");
    }

    #[test]
    fn contributed_via_commit_author() {
        let mut p = pr();
        p.commit_authors.push("me".into());
        let f = PrFacts::compute(&p, &cfg(), "me", ts("2026-10-09T00:00:00Z"));
        assert!(!f.is_mine);
        assert!(f.i_contributed);
    }
}

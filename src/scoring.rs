//! Deterministic bucketing and scoring over `PrFacts`. No I/O here.

use serde::{Deserialize, Serialize};

use crate::config::{Config, Thresholds, Weights};
use crate::enrich::{ChangeKind, Enrichment};
use crate::model::{AuthorTier, CiState, PrFacts, RepoTier, ReviewDecision, ReviewState};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CloseReason {
    Stale,
    OldConflicts,
    StaleBot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActionReason {
    CiFailing,
    ReadyToMerge,
    ChangesRequested,
    Conflicts,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReviewReason {
    RequestedMe,
    Mentioned,
    RequestedTeam,
    ReReview,
    TeamPr,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Bucket {
    Skip,
    Close(CloseReason),
    Action(Vec<ActionReason>),
    Review(Vec<ReviewReason>),
    Fyi,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Explanation {
    pub factors: Vec<(&'static str, f64)>,
    pub bonuses: Vec<(&'static str, f64)>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Scored {
    pub bucket: Bucket,
    pub score: f64,
    pub explanation: Explanation,
}

/// First matching rule wins.
pub fn bucket(f: &PrFacts, t: &Thresholds) -> Bucket {
    if f.is_draft {
        return Bucket::Skip;
    }

    let old = f.age_days >= t.close_after_days;
    if old && f.stale_days >= t.close_stale_days {
        return Bucket::Close(CloseReason::Stale);
    }
    if old && f.has_conflicts {
        return Bucket::Close(CloseReason::OldConflicts);
    }
    if f.author_tier == AuthorTier::Bot && f.stale_days >= t.bot_stale_days {
        return Bucket::Close(CloseReason::StaleBot);
    }

    if f.i_contributed {
        let mut reasons = Vec::new();
        if f.ci == CiState::Failure {
            reasons.push(ActionReason::CiFailing);
        }
        if f.review_decision == ReviewDecision::Approved && f.ci == CiState::Success {
            reasons.push(ActionReason::ReadyToMerge);
        }
        if f.review_decision == ReviewDecision::ChangesRequested {
            reasons.push(ActionReason::ChangesRequested);
        }
        if f.has_conflicts {
            reasons.push(ActionReason::Conflicts);
        }
        if !reasons.is_empty() {
            return Bucket::Action(reasons);
        }
    }

    if !f.is_mine {
        let mut reasons = Vec::new();
        if f.requested_me {
            reasons.push(ReviewReason::RequestedMe);
        }
        if f.mentioned_me {
            reasons.push(ReviewReason::Mentioned);
        }
        if f.requested_my_team {
            reasons.push(ReviewReason::RequestedTeam);
        }
        if f.updated_since_my_review && f.my_last_review != Some(ReviewState::Approved) {
            reasons.push(ReviewReason::ReReview);
        }
        if f.repo_tier == RepoTier::Team
            && f.author_tier == AuthorTier::Team
            && f.review_decision != ReviewDecision::Approved
        {
            reasons.push(ReviewReason::TeamPr);
        }
        if !reasons.is_empty() {
            return Bucket::Review(reasons);
        }
    }

    Bucket::Fyi
}

fn w_author(f: &PrFacts, w: &Weights) -> f64 {
    match f.author_tier {
        AuthorTier::Team => w.author_team,
        AuthorTier::Other => w.author_other,
        AuthorTier::Bot => w.author_bot,
    }
}

fn w_repo(f: &PrFacts, w: &Weights) -> f64 {
    match f.repo_tier {
        RepoTier::Team => w.repo_team,
        RepoTier::Other => w.repo_other,
    }
}

fn w_size(f: &PrFacts, e: Option<&Enrichment>, w: &Weights) -> f64 {
    let base = w
        .size_bands
        .iter()
        .find(|b| f.lines <= b.max_lines)
        .map(|b| b.weight)
        .unwrap_or(w.size_overflow);
    match e.map(|e| e.change_kind) {
        // Docs and config changes read faster than their line count suggests.
        Some(ChangeKind::Docs | ChangeKind::Config) => (base * 1.3).min(1.0),
        _ => base,
    }
}

fn w_recency(f: &PrFacts, t: &Thresholds) -> f64 {
    0.5 + 0.5 * 0.5f64.powf(f.stale_days / t.recency_half_life_days)
}

fn w_discussion(f: &PrFacts, w: &Weights) -> f64 {
    if f.recent_comments > 0 {
        1.0
    } else {
        w.discussion_quiet
    }
}

fn w_state(f: &PrFacts, w: &Weights) -> f64 {
    let mut m = 1.0f64;
    if f.ci == CiState::Failure {
        m = m.min(w.state_ci_failing);
    }
    if f.has_conflicts {
        m = m.min(w.state_conflicts);
    }
    if f.approved_by_other {
        m = m.min(w.state_approved_by_other);
    }
    if f.changes_requested_by_other {
        m = m.min(w.state_changes_requested_by_other);
    }
    m
}

fn review_score(f: &PrFacts, e: Option<&Enrichment>, cfg: &Config) -> (f64, Explanation) {
    let w = &cfg.weights;
    let factors = vec![
        ("author", w_author(f, w)),
        ("repo", w_repo(f, w)),
        ("size", w_size(f, e, w)),
        ("recency", w_recency(f, &cfg.thresholds)),
        ("state", w_state(f, w)),
        ("discussion", w_discussion(f, w)),
    ];
    let mut bonuses = Vec::new();
    if f.requested_me {
        bonuses.push(("requested_me", w.bonus_requested_me));
    }
    if f.mentioned_me {
        bonuses.push(("mentioned_me", w.bonus_mentioned_me));
    }
    if f.updated_since_my_review && f.my_last_review != Some(ReviewState::Approved) {
        bonuses.push(("re_review", w.bonus_rereview));
    }
    if f.requested_my_team {
        bonuses.push(("requested_team", w.bonus_requested_team));
    }
    let product: f64 = factors.iter().map(|(_, v)| v).product();
    let bonus: f64 = bonuses.iter().map(|(_, v)| v).sum();
    (100.0 * product + bonus, Explanation { factors, bonuses })
}

fn action_score(f: &PrFacts, reasons: &[ActionReason], cfg: &Config) -> (f64, Explanation) {
    let w = &cfg.weights;
    let base = reasons
        .iter()
        .map(|r| match r {
            ActionReason::CiFailing => w.action_ci_failing,
            ActionReason::ReadyToMerge => w.action_ready_to_merge,
            ActionReason::ChangesRequested => w.action_changes_requested,
            ActionReason::Conflicts => w.action_conflicts,
        })
        .fold(0.0, f64::max);
    let factors = vec![
        ("reason", base),
        ("recency", w_recency(f, &cfg.thresholds)),
        ("repo", w_repo(f, w)),
    ];
    let score = factors.iter().map(|(_, v)| v).product();
    (
        score,
        Explanation {
            factors,
            bonuses: vec![],
        },
    )
}

pub fn triage(f: &PrFacts, e: Option<&Enrichment>, cfg: &Config) -> Scored {
    let bucket = bucket(f, &cfg.thresholds);
    let (score, explanation) = match &bucket {
        Bucket::Skip => (0.0, Explanation::default()),
        Bucket::Close(_) => (
            f.stale_days,
            Explanation {
                factors: vec![("stale_days", f.stale_days)],
                bonuses: vec![],
            },
        ),
        Bucket::Action(reasons) => action_score(f, reasons, cfg),
        Bucket::Review(_) | Bucket::Fyi => review_score(f, e, cfg),
    };
    Scored {
        bucket,
        score,
        explanation,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> PrFacts {
        PrFacts {
            is_draft: false,
            is_mine: false,
            i_contributed: false,
            author_tier: AuthorTier::Team,
            repo_tier: RepoTier::Team,
            lines: 40,
            age_days: 2.0,
            stale_days: 1.0,
            ci: CiState::Success,
            has_conflicts: false,
            review_decision: ReviewDecision::ReviewRequired,
            requested_me: false,
            requested_my_team: false,
            my_last_review: None,
            updated_since_my_review: false,
            approved_by_other: false,
            changes_requested_by_other: false,
            recent_comments: 0,
            mentioned_me: false,
        }
    }

    fn cfg() -> Config {
        Config::default()
    }

    fn t() -> Thresholds {
        Thresholds::default()
    }

    #[test]
    fn drafts_are_skipped() {
        let f = PrFacts {
            is_draft: true,
            requested_me: true,
            ..facts()
        };
        assert_eq!(bucket(&f, &t()), Bucket::Skip);
    }

    #[test]
    fn old_and_stale_lands_in_close_even_with_conflicts_on_my_pr() {
        let f = PrFacts {
            is_mine: true,
            i_contributed: true,
            has_conflicts: true,
            age_days: 90.0,
            stale_days: 30.0,
            ..facts()
        };
        assert_eq!(bucket(&f, &t()), Bucket::Close(CloseReason::Stale));
    }

    #[test]
    fn old_with_conflicts_lands_in_close_despite_recent_activity() {
        let f = PrFacts {
            is_mine: true,
            i_contributed: true,
            has_conflicts: true,
            age_days: 90.0,
            stale_days: 1.0,
            ..facts()
        };
        assert_eq!(bucket(&f, &t()), Bucket::Close(CloseReason::OldConflicts));
    }

    #[test]
    fn fresh_conflicts_on_my_pr_need_action() {
        let f = PrFacts {
            is_mine: true,
            i_contributed: true,
            has_conflicts: true,
            ..facts()
        };
        assert_eq!(
            bucket(&f, &t()),
            Bucket::Action(vec![ActionReason::Conflicts])
        );
    }

    #[test]
    fn approved_and_green_means_merge() {
        let f = PrFacts {
            is_mine: true,
            i_contributed: true,
            review_decision: ReviewDecision::Approved,
            ..facts()
        };
        assert_eq!(
            bucket(&f, &t()),
            Bucket::Action(vec![ActionReason::ReadyToMerge])
        );
    }

    #[test]
    fn failing_ci_on_a_pr_i_committed_to_needs_action() {
        let f = PrFacts {
            i_contributed: true,
            ci: CiState::Failure,
            ..facts()
        };
        assert_eq!(
            bucket(&f, &t()),
            Bucket::Action(vec![ActionReason::CiFailing])
        );
    }

    #[test]
    fn my_healthy_pr_is_fyi_not_review() {
        let f = PrFacts {
            is_mine: true,
            i_contributed: true,
            requested_me: true,
            ..facts()
        };
        assert_eq!(bucket(&f, &t()), Bucket::Fyi);
    }

    #[test]
    fn stale_bot_pr_is_close_candidate() {
        let f = PrFacts {
            author_tier: AuthorTier::Bot,
            age_days: 40.0,
            stale_days: 35.0,
            ..facts()
        };
        assert_eq!(bucket(&f, &t()), Bucket::Close(CloseReason::StaleBot));
    }

    #[test]
    fn team_pr_in_team_repo_needs_review_without_request() {
        assert_eq!(
            bucket(&facts(), &t()),
            Bucket::Review(vec![ReviewReason::TeamPr])
        );
        let approved = PrFacts {
            review_decision: ReviewDecision::Approved,
            ..facts()
        };
        assert_eq!(bucket(&approved, &t()), Bucket::Fyi);
    }

    #[test]
    fn rereview_only_when_my_last_review_was_not_approval() {
        let f = PrFacts {
            repo_tier: RepoTier::Other,
            updated_since_my_review: true,
            my_last_review: Some(ReviewState::ChangesRequested),
            ..facts()
        };
        assert_eq!(
            bucket(&f, &t()),
            Bucket::Review(vec![ReviewReason::ReReview])
        );
        let f = PrFacts {
            my_last_review: Some(ReviewState::Approved),
            ..f
        };
        assert_eq!(bucket(&f, &t()), Bucket::Fyi);
    }

    #[test]
    fn requested_outranks_unrequested_at_equal_everything_else() {
        let requested = triage(
            &PrFacts {
                requested_me: true,
                ..facts()
            },
            None,
            &cfg(),
        );
        let plain = triage(&facts(), None, &cfg());
        assert!(requested.score > plain.score);
        assert_eq!(requested.explanation.bonuses, vec![("requested_me", 25.0)]);
    }

    #[test]
    fn mention_puts_a_pr_in_review_and_adds_a_bonus() {
        let f = PrFacts {
            repo_tier: RepoTier::Other,
            author_tier: AuthorTier::Other,
            mentioned_me: true,
            ..facts()
        };
        assert_eq!(
            bucket(&f, &t()),
            Bucket::Review(vec![ReviewReason::Mentioned])
        );
        let scored = triage(&f, None, &cfg());
        assert_eq!(scored.explanation.bonuses, vec![("mentioned_me", 20.0)]);

        let mine = PrFacts {
            is_mine: true,
            i_contributed: true,
            ..f
        };
        assert_eq!(bucket(&mine, &t()), Bucket::Fyi);
    }

    #[test]
    fn quiet_prs_score_below_discussed_ones() {
        let quiet = triage(&facts(), None, &cfg());
        let discussed = triage(
            &PrFacts {
                recent_comments: 2,
                ..facts()
            },
            None,
            &cfg(),
        );
        assert!(discussed.score > quiet.score);
        let factor = |s: &Scored| {
            s.explanation
                .factors
                .iter()
                .find(|(k, _)| *k == "discussion")
                .map(|(_, v)| *v)
                .unwrap()
        };
        assert_eq!(factor(&discussed), 1.0);
        assert_eq!(factor(&quiet), 0.8);
    }

    #[test]
    fn bot_ranks_below_team_author_at_equal_size() {
        let bot = triage(
            &PrFacts {
                author_tier: AuthorTier::Bot,
                requested_me: true,
                ..facts()
            },
            None,
            &cfg(),
        );
        let team = triage(
            &PrFacts {
                requested_me: true,
                ..facts()
            },
            None,
            &cfg(),
        );
        assert!(team.score > bot.score);
    }

    #[test]
    fn smaller_and_fresher_scores_higher() {
        let small = triage(&facts(), None, &cfg());
        let big = triage(
            &PrFacts {
                lines: 2000,
                ..facts()
            },
            None,
            &cfg(),
        );
        let stale = triage(
            &PrFacts {
                stale_days: 21.0,
                ..facts()
            },
            None,
            &cfg(),
        );
        assert!(small.score > big.score);
        assert!(small.score > stale.score);
    }

    #[test]
    fn recency_has_a_floor() {
        let ancient = PrFacts {
            stale_days: 10_000.0,
            ..facts()
        };
        assert!((w_recency(&ancient, &t()) - 0.5).abs() < 1e-9);
        assert!(
            (w_recency(
                &PrFacts {
                    stale_days: 0.0,
                    ..facts()
                },
                &t()
            ) - 1.0)
                .abs()
                < 1e-9
        );
    }

    #[test]
    fn docs_enrichment_shrinks_perceived_size() {
        let e = Enrichment {
            change_kind: ChangeKind::Docs,
            description_clarity: crate::enrich::Clarity::Clear,
        };
        let f = PrFacts {
            lines: 400,
            ..facts()
        };
        assert!(w_size(&f, Some(&e), &cfg().weights) > w_size(&f, None, &cfg().weights));
    }

    #[test]
    fn ci_failing_action_outranks_conflicts() {
        let ci = triage(
            &PrFacts {
                i_contributed: true,
                ci: CiState::Failure,
                ..facts()
            },
            None,
            &cfg(),
        );
        let conflict = triage(
            &PrFacts {
                i_contributed: true,
                has_conflicts: true,
                ..facts()
            },
            None,
            &cfg(),
        );
        assert!(ci.score > conflict.score);
    }
}

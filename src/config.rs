//! Configuration: who "me" is, who my team is, and the tunable knobs for scoring.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    /// GitHub login. Resolved from the API when absent.
    pub me: Option<String>,
    /// Restrict every search to these organisations (empty = everywhere).
    pub orgs: Vec<String>,
    pub team: Team,
    pub review_requests: ReviewRequests,
    pub thresholds: Thresholds,
    pub weights: Weights,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct Team {
    /// GitHub logins of immediate team members.
    pub members: Vec<String>,
    /// Repositories as `owner/name`.
    pub repos: Vec<String>,
    /// Extra bot logins on top of accounts GitHub itself types as `Bot`.
    pub bots: Vec<String>,
}

/// Which review requests count as "requested from me".
///
/// GitHub's `review-requested:@me` matches requests to me directly and to any
/// team I belong to. These knobs narrow that down.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct ReviewRequests {
    /// Besides direct requests to me, only requests to these teams (`org/slug`)
    /// count. Empty = every team I belong to.
    pub teams: Vec<String>,
    /// Requests to these teams (`org/slug`) never count, even if I belong to them.
    pub exclude_teams: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct Thresholds {
    /// A PR at least this old is a closure candidate when also stale or conflicting.
    pub close_after_days: f64,
    /// Days without any update before an old PR counts as stale.
    pub close_stale_days: f64,
    /// Days without any update before a bot PR is a closure candidate.
    pub bot_stale_days: f64,
    /// Half-life of the recency factor, in days since last update.
    pub recency_half_life_days: f64,
    /// Review/FYI entries scoring below this are not shown at all.
    pub min_review_score: f64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            close_after_days: 60.0,
            close_stale_days: 21.0,
            bot_stale_days: 30.0,
            recency_half_life_days: 7.0,
            min_review_score: 0.0,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SizeBand {
    /// Inclusive upper bound on additions + deletions.
    pub max_lines: u32,
    pub weight: f64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct Weights {
    pub author_team: f64,
    pub author_other: f64,
    pub author_bot: f64,

    pub repo_team: f64,
    pub repo_other: f64,

    /// Ascending bands; the first band whose `max_lines` is >= the PR's lines wins.
    pub size_bands: Vec<SizeBand>,
    /// Weight for PRs larger than every band.
    pub size_overflow: f64,

    pub state_ci_failing: f64,
    pub state_conflicts: f64,
    pub state_approved_by_other: f64,
    pub state_changes_requested_by_other: f64,

    pub bonus_requested_me: f64,
    pub bonus_rereview: f64,
    pub bonus_requested_team: f64,

    pub action_ci_failing: f64,
    pub action_ready_to_merge: f64,
    pub action_changes_requested: f64,
    pub action_conflicts: f64,
}

impl Default for Weights {
    fn default() -> Self {
        Self {
            author_team: 1.0,
            author_other: 0.6,
            author_bot: 0.25,
            repo_team: 1.0,
            repo_other: 0.6,
            size_bands: vec![
                SizeBand {
                    max_lines: 50,
                    weight: 1.0,
                },
                SizeBand {
                    max_lines: 200,
                    weight: 0.85,
                },
                SizeBand {
                    max_lines: 600,
                    weight: 0.65,
                },
                SizeBand {
                    max_lines: 1500,
                    weight: 0.45,
                },
            ],
            size_overflow: 0.3,
            state_ci_failing: 0.5,
            state_conflicts: 0.6,
            state_approved_by_other: 0.8,
            state_changes_requested_by_other: 0.7,
            bonus_requested_me: 25.0,
            bonus_rereview: 15.0,
            bonus_requested_team: 10.0,
            action_ci_failing: 90.0,
            action_ready_to_merge: 85.0,
            action_changes_requested: 75.0,
            action_conflicts: 70.0,
        }
    }
}

const EXAMPLE: &str = r#"me: your-github-login        # optional, resolved from the API if absent
orgs: [acme]                 # only look at PRs in these orgs

team:
  members: [alice, bob]
  repos:
    - acme/widgets
    - acme/gadgets
  bots: ["dependabot[bot]", "renovate[bot]"]

review_requests:
  teams: [acme/payments]       # only these teams' requests count (plus direct ones)
  exclude_teams: [acme/bot]    # never count requests to these teams

thresholds:
  close_after_days: 60
  close_stale_days: 21
  recency_half_life_days: 7
  min_review_score: 20      # hide review candidates scoring below this
"#;

impl Config {
    pub fn default_path() -> Result<PathBuf> {
        if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
            return Ok(PathBuf::from(xdg).join("anvil").join("config.yaml"));
        }
        let base = directories::BaseDirs::new().context("could not determine home directory")?;
        Ok(base
            .home_dir()
            .join(".config")
            .join("anvil")
            .join("config.yaml"))
    }

    pub fn load(explicit: Option<&Path>) -> Result<Config> {
        let path = match explicit {
            Some(p) => p.to_path_buf(),
            None => Self::default_path()?,
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                bail!(
                    "no config file at {}\n\nCreate one like:\n\n{EXAMPLE}",
                    path.display()
                );
            }
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        Self::parse(&text).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Config> {
        let cfg: Config = serde_saphyr::from_str(text)?;
        if cfg
            .weights
            .size_bands
            .windows(2)
            .any(|w| w[0].max_lines >= w[1].max_lines)
        {
            bail!("weights.size_bands must be strictly ascending in max_lines");
        }
        let rr = &cfg.review_requests;
        for team in rr.teams.iter().chain(&rr.exclude_teams) {
            if !team.contains('/') {
                bail!("review_requests teams must be `org/slug`, got `{team}`");
            }
        }
        if let Some(both) = rr
            .teams
            .iter()
            .find(|t| rr.exclude_teams.iter().any(|e| e.eq_ignore_ascii_case(t)))
        {
            bail!("review_requests: `{both}` is both in teams and exclude_teams");
        }
        Ok(cfg)
    }

    pub fn is_team_member(&self, login: &str) -> bool {
        self.team
            .members
            .iter()
            .any(|m| m.eq_ignore_ascii_case(login))
    }

    pub fn is_team_repo(&self, name_with_owner: &str) -> bool {
        self.team
            .repos
            .iter()
            .any(|r| r.eq_ignore_ascii_case(name_with_owner))
    }

    pub fn is_listed_bot(&self, login: &str) -> bool {
        self.team.bots.iter().any(|b| b.eq_ignore_ascii_case(login))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_example_and_fills_defaults() {
        let cfg = Config::parse(EXAMPLE).unwrap();
        assert_eq!(cfg.me.as_deref(), Some("your-github-login"));
        assert_eq!(cfg.orgs, vec!["acme"]);
        assert_eq!(cfg.team.members, vec!["alice", "bob"]);
        assert!(cfg.is_team_repo("ACME/widgets"));
        assert!(cfg.is_listed_bot("dependabot[bot]"));
        assert_eq!(cfg.review_requests.teams, vec!["acme/payments"]);
        assert_eq!(cfg.review_requests.exclude_teams, vec!["acme/bot"]);
        assert_eq!(cfg.thresholds.bot_stale_days, 30.0);
        assert_eq!(cfg.thresholds.min_review_score, 20.0);
        assert_eq!(cfg.weights.author_bot, 0.25);
    }

    #[test]
    fn empty_document_is_all_defaults() {
        let cfg = Config::parse("{}").unwrap();
        assert!(cfg.me.is_none());
        assert_eq!(cfg.weights.size_bands.len(), 4);
    }

    #[test]
    fn rejects_unknown_keys() {
        assert!(Config::parse("teem: {}").is_err());
    }

    #[test]
    fn review_request_teams_must_be_org_slash_slug() {
        assert!(Config::parse("review_requests: {teams: [payments]}").is_err());
        assert!(Config::parse("review_requests: {exclude_teams: [bot]}").is_err());
        assert!(Config::parse("review_requests: {teams: [acme/payments]}").is_ok());
    }

    #[test]
    fn review_request_team_cannot_be_included_and_excluded() {
        let err = Config::parse(
            "review_requests: {teams: [acme/payments], exclude_teams: [ACME/payments]}",
        )
        .unwrap_err();
        assert!(err.to_string().contains("both"), "{err}");
    }
}

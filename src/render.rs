//! Terminal and JSON output.

use std::cmp::Ordering;
use std::io::Write;

use anyhow::Result;
use owo_colors::{OwoColorize, Stream::Stdout};
use serde::Serialize;

use crate::model::{CiState, Pr, PrFacts};
use crate::scoring::{ActionReason, Bucket, CloseReason, ReviewReason, Scored};

#[derive(Debug, Clone, Serialize)]
pub struct Entry {
    pub pr: Pr,
    pub facts: PrFacts,
    pub scored: Scored,
}

#[derive(Debug, Default, Serialize)]
pub struct Report {
    pub review: Vec<Entry>,
    pub action: Vec<Entry>,
    pub close: Vec<Entry>,
    pub fyi: Vec<Entry>,
    /// Review/FYI entries dropped for scoring below `min_review_score`.
    pub hidden_below_threshold: usize,
    pub min_review_score: f64,
}

fn by_score_desc(a: &Entry, b: &Entry) -> Ordering {
    b.scored
        .score
        .partial_cmp(&a.scored.score)
        .unwrap_or(Ordering::Equal)
        .then_with(|| a.pr.repo.cmp(&b.pr.repo))
        .then_with(|| a.pr.number.cmp(&b.pr.number))
}

impl Report {
    pub fn build(entries: Vec<Entry>, min_review_score: f64) -> Report {
        let mut r = Report {
            min_review_score,
            ..Report::default()
        };
        for e in entries {
            match e.scored.bucket {
                Bucket::Skip => {}
                Bucket::Close(_) => r.close.push(e),
                Bucket::Action(_) => r.action.push(e),
                Bucket::Review(_) | Bucket::Fyi if e.scored.score < min_review_score => {
                    r.hidden_below_threshold += 1;
                }
                Bucket::Review(_) => r.review.push(e),
                Bucket::Fyi => r.fyi.push(e),
            }
        }
        r.review.sort_by(by_score_desc);
        r.action.sort_by(by_score_desc);
        r.close.sort_by(by_score_desc);
        r.fyi.sort_by(by_score_desc);
        r
    }

    /// Keep only the best `top` entries per section (`0` = unlimited).
    pub fn truncate(&mut self, top: usize) {
        if top == 0 {
            return;
        }
        for section in [
            &mut self.review,
            &mut self.action,
            &mut self.close,
            &mut self.fyi,
        ] {
            section.truncate(top);
        }
    }

    pub fn print_json(&self, out: &mut impl Write) -> Result<()> {
        serde_json::to_writer_pretty(&mut *out, self)?;
        writeln!(out)?;
        Ok(())
    }

    /// `top == 0` means unlimited.
    pub fn print_terminal(
        &self,
        out: &mut impl Write,
        explain: bool,
        show_fyi: bool,
        top: usize,
    ) -> Result<()> {
        section(
            out,
            "NEEDS MY REVIEW",
            &self.review,
            explain,
            top,
            review_line,
        )?;
        if self.hidden_below_threshold > 0 {
            let hidden = format!(
                "  {} hidden for scoring below {} (thresholds.min_review_score)\n",
                self.hidden_below_threshold, self.min_review_score
            );
            writeln!(out, "{}", hidden.if_supports_color(Stdout, |t| t.dimmed()))?;
        }
        section(
            out,
            "NEEDS MY ACTION",
            &self.action,
            explain,
            top,
            action_line,
        )?;
        section(
            out,
            "COULD BE CLOSED",
            &self.close,
            explain,
            top,
            close_line,
        )?;
        if show_fyi {
            section(out, "FYI", &self.fyi, explain, top, review_line)?;
        }
        Ok(())
    }
}

fn section(
    out: &mut impl Write,
    title: &str,
    entries: &[Entry],
    explain: bool,
    top: usize,
    line: fn(&Entry) -> String,
) -> Result<()> {
    writeln!(out, "{}", title.if_supports_color(Stdout, |t| t.bold()))?;
    if entries.is_empty() {
        writeln!(
            out,
            "  {}",
            "nothing".if_supports_color(Stdout, |t| t.dimmed())
        )?;
    }
    let shown = if top == 0 {
        entries.len()
    } else {
        top.min(entries.len())
    };
    for e in &entries[..shown] {
        writeln!(
            out,
            "  {}  {}",
            line(e),
            e.pr.url.if_supports_color(Stdout, |t| t.dimmed())
        )?;
        if explain {
            writeln!(
                out,
                "      {}",
                explanation(e).if_supports_color(Stdout, |t| t.dimmed())
            )?;
        }
    }
    if shown < entries.len() {
        let more = format!(
            "… and {} more (raise --top to see them)",
            entries.len() - shown
        );
        writeln!(out, "  {}", more.if_supports_color(Stdout, |t| t.dimmed()))?;
    }
    writeln!(out)?;
    Ok(())
}

fn explanation(e: &Entry) -> String {
    let mut parts: Vec<String> = e
        .scored
        .explanation
        .factors
        .iter()
        .map(|(k, v)| format!("{k}={v:.2}"))
        .collect();
    for (k, v) in &e.scored.explanation.bonuses {
        parts.push(format!("+{v:.0} ({k})"));
    }
    parts.join(" ")
}

/// At most `max` characters, ending in an ellipsis when cut.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", head.trim_end())
}

fn ci_glyph(ci: CiState) -> String {
    match ci {
        CiState::Success => "✓".if_supports_color(Stdout, |t| t.green()).to_string(),
        CiState::Failure => "✗".if_supports_color(Stdout, |t| t.red()).to_string(),
        CiState::Pending => "●".if_supports_color(Stdout, |t| t.yellow()).to_string(),
        CiState::None => "-".to_string(),
    }
}

fn pr_ref(pr: &Pr) -> String {
    format!("{}#{}", pr.repo, pr.number)
}

fn review_line(e: &Entry) -> String {
    let reasons = match &e.scored.bucket {
        Bucket::Review(r) => r
            .iter()
            .map(|r| match r {
                ReviewReason::RequestedMe => "requested",
                ReviewReason::RequestedTeam => "team-requested",
                ReviewReason::ReReview => "re-review",
                ReviewReason::TeamPr => "team",
            })
            .collect::<Vec<_>>()
            .join(","),
        _ => String::new(),
    };
    let conflict = if e.facts.has_conflicts {
        " ⚠conflicts"
    } else {
        ""
    };
    format!(
        "{:>3}  {:<34} {:>12}  {:>3}d  {}  {:<14} {:<16} {}{}",
        e.scored.score.round() as i64,
        truncate(&pr_ref(&e.pr), 34),
        format!("+{}/-{}", e.pr.additions, e.pr.deletions),
        e.facts.stale_days.round() as i64,
        ci_glyph(e.facts.ci),
        truncate(&e.pr.author, 14),
        reasons,
        truncate(&e.pr.title, 60),
        conflict,
    )
}

fn action_line(e: &Entry) -> String {
    let reasons = match &e.scored.bucket {
        Bucket::Action(r) => r
            .iter()
            .map(|r| match r {
                ActionReason::CiFailing => "CI failing",
                ActionReason::ReadyToMerge => "approved & green: merge",
                ActionReason::ChangesRequested => "changes requested",
                ActionReason::Conflicts => "conflicts",
            })
            .collect::<Vec<_>>()
            .join(", "),
        _ => String::new(),
    };
    format!(
        "{:>3}  {:<34} {:<28} {}",
        e.scored.score.round() as i64,
        truncate(&pr_ref(&e.pr), 34),
        reasons,
        truncate(&e.pr.title, 60),
    )
}

fn close_line(e: &Entry) -> String {
    let reason = match &e.scored.bucket {
        Bucket::Close(CloseReason::Stale) => "stale",
        Bucket::Close(CloseReason::OldConflicts) => "old+conflicts",
        Bucket::Close(CloseReason::StaleBot) => "stale bot",
        _ => "",
    };
    let who = if e.facts.is_mine {
        "mine".to_string()
    } else {
        format!("nudge {}", e.pr.author)
    };
    format!(
        "{:>4}d stale  {:<34} {:<14} {:<22} {}",
        e.facts.stale_days.round() as i64,
        truncate(&pr_ref(&e.pr), 34),
        reason,
        format!("({})", truncate(&who, 20)),
        truncate(&e.pr.title, 60),
    )
}

#[cfg(test)]
mod tests {
    use super::truncate;

    #[test]
    fn truncate_never_exceeds_max_including_ellipsis() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("exactly-ten", 11), "exactly-ten");
        let cut = truncate("TrueLayer/homebrew-truelayer-internal#12", 34);
        assert_eq!(cut.chars().count(), 34);
        assert!(cut.ends_with('…'));
        assert_eq!(truncate("ab", 1), "…");
    }
}

//! Terminal and JSON output.

use std::cmp::Ordering;
use std::io::{IsTerminal, Write};

use anyhow::Result;
use comfy_table::{Attribute, Cell, CellAlignment, ColumnConstraint, ContentArrangement, Table};
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
            REVIEW_HEADER,
            review_row,
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
            ACTION_HEADER,
            action_row,
        )?;
        section(
            out,
            "COULD BE CLOSED",
            &self.close,
            explain,
            top,
            CLOSE_HEADER,
            close_row,
        )?;
        if show_fyi {
            section(
                out,
                "FYI",
                &self.fyi,
                explain,
                top,
                REVIEW_HEADER,
                review_row,
            )?;
        }
        Ok(())
    }
}

/// Column of the PR reference (`owner/repo#number`), which is rendered as a
/// hyperlink. Every section keeps it in the same position.
const PR_REF_COLUMN: usize = 1;

const REVIEW_HEADER: &[&str] = &[
    "score", "pr", "diff", "stale", "ci", "author", "reason", "title",
];
const ACTION_HEADER: &[&str] = &["score", "pr", "reason", "title"];
const CLOSE_HEADER: &[&str] = &["stale", "pr", "reason", "who", "title"];

#[allow(clippy::too_many_arguments)]
fn section(
    out: &mut impl Write,
    title: &str,
    entries: &[Entry],
    explain: bool,
    top: usize,
    header: &[&str],
    row: fn(&Entry) -> Vec<Cell>,
) -> Result<()> {
    writeln!(out, "{}", title.if_supports_color(Stdout, |t| t.bold()))?;
    if entries.is_empty() {
        writeln!(
            out,
            "  {}",
            "nothing".if_supports_color(Stdout, |t| t.dimmed())
        )?;
        writeln!(out)?;
        return Ok(());
    }
    let shown = if top == 0 {
        entries.len()
    } else {
        top.min(entries.len())
    };

    let mut table = Table::new();
    table
        .load_style(comfy_table::presets::UTF8_FULL_CONDENSED)
        .set_content_arrangement(ContentArrangement::Dynamic);
    let mut header: Vec<Cell> = header
        .iter()
        .map(|h| Cell::new(h).add_attribute(Attribute::Bold))
        .collect();
    if explain {
        header.push(Cell::new("factors").add_attribute(Attribute::Bold));
    }
    table.set_header(header);
    for e in &entries[..shown] {
        let mut cells = row(e);
        if explain {
            cells.push(Cell::new(explanation(e)).add_attribute(Attribute::Dim));
        }
        table.add_row(cells);
    }
    // Never wrap the link column: a split hyperlink is not clickable.
    if let Some(col) = table.column_mut(PR_REF_COLUMN) {
        col.set_constraint(ColumnConstraint::ContentWidth);
    }
    writeln!(out, "{table}")?;

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

/// `text` wrapped in an OSC 8 terminal hyperlink to `url`.
fn hyperlink(text: &str, url: &str) -> String {
    format!("\x1b]8;;{url}\x1b\\{text}\x1b]8;;\x1b\\")
}

/// `owner/repo#number`, clickable when stdout is a terminal. Piped output
/// stays plain so it greps and diffs cleanly.
fn pr_ref(pr: &Pr) -> Cell {
    let text = truncate(&format!("{}#{}", pr.repo, pr.number), 34);
    if std::io::stdout().is_terminal() {
        Cell::new(hyperlink(&text, &pr.url))
    } else {
        Cell::new(text)
    }
}

fn right(text: impl ToString) -> Cell {
    Cell::new(text).set_alignment(CellAlignment::Right)
}

fn review_row(e: &Entry) -> Vec<Cell> {
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
    let title = if e.facts.has_conflicts {
        format!("{} ⚠conflicts", e.pr.title)
    } else {
        e.pr.title.clone()
    };
    vec![
        right(e.scored.score.round() as i64),
        pr_ref(&e.pr),
        right(format!("+{}/-{}", e.pr.additions, e.pr.deletions)),
        right(format!("{}d", e.facts.stale_days.round() as i64)),
        Cell::new(ci_glyph(e.facts.ci)).set_alignment(CellAlignment::Center),
        Cell::new(&e.pr.author),
        Cell::new(reasons),
        Cell::new(title),
    ]
}

fn action_row(e: &Entry) -> Vec<Cell> {
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
    vec![
        right(e.scored.score.round() as i64),
        pr_ref(&e.pr),
        Cell::new(reasons),
        Cell::new(&e.pr.title),
    ]
}

fn close_row(e: &Entry) -> Vec<Cell> {
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
    vec![
        right(format!("{}d", e.facts.stale_days.round() as i64)),
        pr_ref(&e.pr),
        Cell::new(reason),
        Cell::new(who),
        Cell::new(&e.pr.title),
    ]
}

#[cfg(test)]
mod tests {
    use super::{hyperlink, truncate};

    #[test]
    fn hyperlink_wraps_text_in_osc8() {
        let link = hyperlink("acme/widgets#1", "https://github.com/acme/widgets/pull/1");
        assert_eq!(
            link,
            "\x1b]8;;https://github.com/acme/widgets/pull/1\x1b\\acme/widgets#1\x1b]8;;\x1b\\"
        );
    }

    #[test]
    fn truncate_never_exceeds_max_including_ellipsis() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("exactly-ten", 11), "exactly-ten");
        let cut = truncate("SomeOrg/homebrew-some-long-word-this-internal#12", 34);
        assert_eq!(cut.chars().count(), 34);
        assert!(cut.ends_with('…'));
        assert_eq!(truncate("ab", 1), "…");
    }
}

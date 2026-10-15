//! `anvil calibrate`: learn the scoring weights from pairwise preferences.
//!
//! Each answer to "which of these two PRs matters more?" is stored as a
//! judgment together with a snapshot of the facts both PRs had at the time.
//! Facts drift (staleness, CI, reviews), so the snapshot is what the answer
//! was given against, not the PR itself.
//!
//! Fitting always starts from the shipped defaults and refits every stored
//! judgment, so the weights are a pure function of the judgments file. Only
//! `weights` are learned; bucket rules and thresholds are not.

use std::collections::HashMap;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use comfy_table::{Attribute, Cell, ContentArrangement, Table};
use jiff::Timestamp;
use owo_colors::{OwoColorize, Stream::Stdout};
use serde::{Deserialize, Serialize};

use crate::config::{Config, Weights};
use crate::model::{CiState, PrFacts, ReviewDecision, ReviewState};
use crate::render::Entry;
use crate::scoring::{self, Bucket};

/// Score differences are divided by this before the logistic. A gap of one
/// temperature is a mild preference; three is near-certain.
const TEMPERATURE: f64 = 10.0;
/// Penalty for moving a parameter one "unit" away from its default. One unit
/// is 0.25 for a multiplier and 10 points for a bonus or action weight.
const REGULARIZATION: f64 = 1.0;
/// Shown PRs are deprioritized by this many score points per showing.
const SHOWN_PENALTY: f64 = 15.0;

#[derive(Debug, clap::Args)]
pub struct CalibrateArgs {
    /// Which section to draw pairs from.
    #[arg(long, value_enum, default_value_t = Kind::Review)]
    pub kind: Kind,

    /// How many pairs to ask about (0 = only refit the stored judgments).
    #[arg(long, default_value_t = 10, value_name = "N")]
    pub rounds: usize,

    /// Write the fitted weights to the config without asking.
    #[arg(long)]
    pub write: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Review,
    Action,
}

impl Kind {
    pub fn of(b: &Bucket) -> Option<Kind> {
        match b {
            Bucket::Review(_) => Some(Kind::Review),
            Bucket::Action(_) => Some(Kind::Action),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Judgments on disk
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Side {
    pub repo: String,
    pub number: u64,
    pub title: String,
    pub url: String,
    pub facts: PrFacts,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Winner {
    A,
    B,
    Tie,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Judgment {
    pub at: Timestamp,
    pub kind: Kind,
    pub a: Side,
    pub b: Side,
    pub winner: Winner,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Store {
    pub judgments: Vec<Judgment>,
}

impl Store {
    /// Always `~/.local/share/anvil/judgments.yaml`.
    pub fn path() -> Result<PathBuf> {
        let base = directories::BaseDirs::new().context("could not determine home directory")?;
        Ok(base
            .home_dir()
            .join(".local")
            .join("share")
            .join("anvil")
            .join("judgments.yaml"))
    }

    pub fn load(path: &Path) -> Result<Store> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                serde_saphyr::from_str(&text).with_context(|| format!("parsing {}", path.display()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Store::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let text = serde_saphyr::to_string(self)?;
        std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))
    }

    fn judged(&self, a: &str, b: &str) -> bool {
        self.judgments
            .iter()
            .any(|j| (j.a.url == a && j.b.url == b) || (j.a.url == b && j.b.url == a))
    }
}

// ---------------------------------------------------------------------------
// Parameters: a flat view over the learnable fields of `Weights`
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParamKind {
    /// A factor in (0, 1]; `1.0` is the anchor.
    Multiplier,
    /// Additive points (bonuses) or an action base score.
    Points,
}

impl ParamKind {
    fn bounds(self) -> (f64, f64) {
        match self {
            ParamKind::Multiplier => (0.05, 1.0),
            ParamKind::Points => (0.0, 150.0),
        }
    }

    /// Size of one regularization unit, also the coarsest search step.
    fn unit(self) -> f64 {
        match self {
            ParamKind::Multiplier => 0.25,
            ParamKind::Points => 10.0,
        }
    }
}

#[derive(Debug, Clone)]
struct Param {
    name: String,
    kind: ParamKind,
}

/// Every learnable field, by mutable reference. `author_team` and
/// `repo_team` stay at their configured value: they anchor the scale.
fn slots(w: &mut Weights) -> Vec<(String, &mut f64, ParamKind)> {
    use ParamKind::*;
    let mut v: Vec<(String, &mut f64, ParamKind)> = vec![
        ("author_other".into(), &mut w.author_other, Multiplier),
        ("author_bot".into(), &mut w.author_bot, Multiplier),
        ("repo_other".into(), &mut w.repo_other, Multiplier),
    ];
    for (i, b) in w.size_bands.iter_mut().enumerate() {
        let name = format!("size_bands[{i}] (≤{} lines)", b.max_lines);
        v.push((name, &mut b.weight, Multiplier));
    }
    v.extend([
        ("size_overflow".into(), &mut w.size_overflow, Multiplier),
        (
            "state_ci_failing".into(),
            &mut w.state_ci_failing,
            Multiplier,
        ),
        ("state_conflicts".into(), &mut w.state_conflicts, Multiplier),
        (
            "state_approved_by_other".into(),
            &mut w.state_approved_by_other,
            Multiplier,
        ),
        (
            "state_changes_requested_by_other".into(),
            &mut w.state_changes_requested_by_other,
            Multiplier,
        ),
        (
            "discussion_quiet".into(),
            &mut w.discussion_quiet,
            Multiplier,
        ),
        (
            "bonus_requested_me".into(),
            &mut w.bonus_requested_me,
            Points,
        ),
        (
            "bonus_mentioned_me".into(),
            &mut w.bonus_mentioned_me,
            Points,
        ),
        ("bonus_rereview".into(), &mut w.bonus_rereview, Points),
        (
            "bonus_requested_team".into(),
            &mut w.bonus_requested_team,
            Points,
        ),
        ("action_ci_failing".into(), &mut w.action_ci_failing, Points),
        (
            "action_ready_to_merge".into(),
            &mut w.action_ready_to_merge,
            Points,
        ),
        (
            "action_changes_requested".into(),
            &mut w.action_changes_requested,
            Points,
        ),
        ("action_conflicts".into(), &mut w.action_conflicts, Points),
    ]);
    v
}

fn params(w: &Weights) -> Vec<Param> {
    let mut w = w.clone();
    slots(&mut w)
        .into_iter()
        .map(|(name, _, kind)| Param { name, kind })
        .collect()
}

fn read(w: &Weights) -> Vec<f64> {
    let mut w = w.clone();
    slots(&mut w).into_iter().map(|(_, v, _)| *v).collect()
}

fn write(template: &Weights, values: &[f64]) -> Weights {
    let mut w = template.clone();
    for ((_, slot, _), v) in slots(&mut w).into_iter().zip(values) {
        *slot = *v;
    }
    w
}

/// Bigger PRs never score higher than smaller ones.
fn valid(w: &Weights) -> bool {
    let bands = w.size_bands.iter().map(|b| b.weight);
    let chain: Vec<f64> = bands.chain([w.size_overflow]).collect();
    chain.windows(2).all(|p| p[0] >= p[1])
}

/// Where fitting starts and what it is pulled toward: the shipped defaults,
/// with the size band structure (`max_lines`) taken from the config since it
/// is not learned.
fn prior(cfg: &Config) -> Weights {
    let mut p = Weights::default();
    if cfg.weights.size_bands.len() == p.size_bands.len() {
        for (pb, cb) in p.size_bands.iter_mut().zip(&cfg.weights.size_bands) {
            pb.max_lines = cb.max_lines;
        }
    } else {
        p.size_bands = cfg.weights.size_bands.clone();
    }
    p.author_team = cfg.weights.author_team;
    p.repo_team = cfg.weights.repo_team;
    p
}

// ---------------------------------------------------------------------------
// Loss and fit
// ---------------------------------------------------------------------------

fn softplus(x: f64) -> f64 {
    if x > 30.0 { x } else { (1.0 + x.exp()).ln() }
}

/// Scores of both sides under `cfg`, or `None` when a side no longer lands in
/// the section it was judged in (only possible after editing thresholds).
fn scores(j: &Judgment, cfg: &Config) -> Option<(f64, f64)> {
    let a = scoring::triage(&j.a.facts, None, cfg);
    let b = scoring::triage(&j.b.facts, None, cfg);
    if Kind::of(&a.bucket) != Some(j.kind) || Kind::of(&b.bucket) != Some(j.kind) {
        return None;
    }
    Some((a.score, b.score))
}

/// Negative log-likelihood of one answer under a Bradley–Terry model. A tie
/// is scored as "both orders equally likely", which is zero at equal scores.
fn judgment_loss(j: &Judgment, cfg: &Config) -> Option<f64> {
    let (a, b) = scores(j, cfg)?;
    let d = (a - b) / TEMPERATURE;
    Some(match j.winner {
        Winner::A => softplus(-d),
        Winner::B => softplus(d),
        Winner::Tie => softplus(d) + softplus(-d) - 4f64.ln(),
    })
}

fn loss(js: &[Judgment], cfg: &Config, params: &[Param], base: &[f64], cur: &[f64]) -> f64 {
    let data: f64 = js.iter().filter_map(|j| judgment_loss(j, cfg)).sum();
    let reg: f64 = params
        .iter()
        .zip(base.iter().zip(cur))
        .map(|(p, (b, c))| ((c - b) / p.kind.unit()).powi(2))
        .sum();
    data + REGULARIZATION * reg
}

/// Coordinate search with shrinking steps. The loss is cheap and the space
/// is ~20 bounded dimensions, so this converges in well under a second.
pub fn fit(js: &[Judgment], cfg: &Config) -> Weights {
    let prior = prior(cfg);
    let params = params(&prior);
    let base = read(&prior);
    let mut cur = base.clone();
    let mut work = cfg.clone();

    let mut evaluate = |v: &[f64]| {
        work.weights = write(&prior, v);
        loss(js, &work, &params, &base, v)
    };
    let mut best = evaluate(&cur);

    for scale in [1.0, 0.5, 0.25, 0.1, 0.04] {
        for _pass in 0..200 {
            let mut improved = false;
            for i in 0..cur.len() {
                let (lo, hi) = params[i].kind.bounds();
                let step = params[i].kind.unit() * scale;
                for dir in [1.0, -1.0] {
                    let mut trial = cur.clone();
                    trial[i] = (cur[i] + dir * step).clamp(lo, hi);
                    if trial[i] == cur[i] || !valid(&write(&prior, &trial)) {
                        continue;
                    }
                    let l = evaluate(&trial);
                    if l < best - 1e-9 {
                        best = l;
                        cur = trial;
                        improved = true;
                    }
                }
            }
            if !improved {
                break;
            }
        }
    }
    for v in &mut cur {
        *v = (*v * 100.0).round() / 100.0;
    }
    write(&prior, &cur)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Agreement {
    pub agree: usize,
    pub total: usize,
}

/// How many non-tie judgments the ranking under `cfg` reproduces.
pub fn agreement(js: &[Judgment], cfg: &Config) -> Agreement {
    let mut agg = Agreement::default();
    for j in js {
        let Some((a, b)) = scores(j, cfg) else {
            continue;
        };
        let expect = match j.winner {
            Winner::A => a > b,
            Winner::B => b > a,
            Winner::Tie => continue,
        };
        agg.total += 1;
        agg.agree += usize::from(expect);
    }
    agg
}

/// `(name, current, fitted)` for every parameter that changed.
pub fn changes(current: &Weights, fitted: &Weights) -> Vec<(String, f64, f64)> {
    params(fitted)
        .into_iter()
        .zip(read(current).into_iter().zip(read(fitted)))
        .filter(|(_, (c, f))| (c - f).abs() > 1e-9)
        .map(|(p, (c, f))| (p.name, c, f))
        .collect()
}

// ---------------------------------------------------------------------------
// Pair selection
// ---------------------------------------------------------------------------

/// Number of learnable factors on which two entries differ. Recency is
/// continuous and not learned, so it never counts.
fn distance(a: &Entry, b: &Entry) -> usize {
    let ea = &a.scored.explanation;
    let eb = &b.scored.explanation;
    let factors = ea
        .factors
        .iter()
        .zip(&eb.factors)
        .filter(|((ka, va), (kb, vb))| ka == kb && *ka != "recency" && (va - vb).abs() > 1e-9)
        .count();
    let bonus = |e: &crate::scoring::Explanation, k: &str| {
        e.bonuses.iter().find(|(n, _)| *n == k).map(|(_, v)| *v)
    };
    let bonuses = ea
        .bonuses
        .iter()
        .chain(&eb.bonuses)
        .map(|(k, _)| *k)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .filter(|k| bonus(ea, k) != bonus(eb, k))
        .count();
    factors + bonuses
}

/// The most informative unjudged pair: closest in score among pairs that
/// differ in at least one learnable factor, with already-shown PRs pushed
/// back so one PR does not dominate a session.
pub fn pick_pair(
    pool: &[&Entry],
    store: &Store,
    shown: &HashMap<usize, usize>,
) -> Option<(usize, usize)> {
    let mut best: Option<((usize, usize), f64)> = None;
    for i in 0..pool.len() {
        for j in i + 1..pool.len() {
            let (a, b) = (pool[i], pool[j]);
            if distance(a, b) == 0 || store.judged(&a.pr.url, &b.pr.url) {
                continue;
            }
            let seen = shown.get(&i).unwrap_or(&0) + shown.get(&j).unwrap_or(&0);
            let cost = (a.scored.score - b.scored.score).abs() + SHOWN_PENALTY * seen as f64;
            if best.is_none_or(|(_, c)| cost < c) {
                best = Some(((i, j), cost));
            }
        }
    }
    best.map(|(pair, _)| pair)
}

// ---------------------------------------------------------------------------
// Interactive session
// ---------------------------------------------------------------------------

enum Answer {
    Pick(Winner),
    Skip,
    Quit,
}

fn ask(out: &mut impl Write) -> Result<Answer> {
    loop {
        write!(
            out,
            "which matters more?  [a] left  [b] right  [t] tie  [s] skip  [q] quit > "
        )?;
        out.flush()?;
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line)? == 0 {
            writeln!(out)?;
            return Ok(Answer::Quit);
        }
        match line.trim().to_ascii_lowercase().as_str() {
            "a" => return Ok(Answer::Pick(Winner::A)),
            "b" => return Ok(Answer::Pick(Winner::B)),
            "t" | "=" => return Ok(Answer::Pick(Winner::Tie)),
            "s" => return Ok(Answer::Skip),
            "q" => return Ok(Answer::Quit),
            _ => writeln!(out, "  please answer a, b, t, s or q")?,
        }
    }
}

fn confirm(out: &mut impl Write, question: &str) -> Result<bool> {
    write!(out, "{question} [y/N] > ")?;
    out.flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(matches!(
        line.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn describe(e: &Entry) -> Vec<String> {
    let f = &e.facts;
    let pr = &e.pr;
    let ci = match f.ci {
        CiState::Success => "passing",
        CiState::Failure => "failing",
        CiState::Pending => "pending",
        CiState::None => "none",
    };
    let decision = match f.review_decision {
        ReviewDecision::Approved => "approved",
        ReviewDecision::ChangesRequested => "changes requested",
        ReviewDecision::ReviewRequired => "review required",
        ReviewDecision::None => "no decision",
    };
    let mut state = vec![decision.to_string()];
    if f.approved_by_other {
        state.push("approved by someone".into());
    }
    if f.changes_requested_by_other {
        state.push("changes requested by someone".into());
    }
    if f.has_conflicts {
        state.push("conflicts".into());
    }
    let mut asks = Vec::new();
    if f.requested_me {
        asks.push("review requested from me");
    }
    if f.requested_my_team {
        asks.push("review requested from my team");
    }
    if f.mentioned_me {
        asks.push("mentions me");
    }
    if f.updated_since_my_review && f.my_last_review != Some(ReviewState::Approved) {
        asks.push("updated since my review");
    }
    if asks.is_empty() {
        asks.push("-");
    }
    let link = if std::io::stdout().is_terminal() {
        format!(
            "\x1b]8;;{}\x1b\\{}#{}\x1b]8;;\x1b\\",
            pr.url, pr.repo, pr.number
        )
    } else {
        format!("{}#{}", pr.repo, pr.number)
    };
    vec![
        pr.title.clone(),
        link,
        format!("{} ({:?})", pr.author, f.author_tier).to_lowercase(),
        format!("{:?} repo", f.repo_tier).to_lowercase(),
        format!("+{}/-{} ({} lines)", pr.additions, pr.deletions, f.lines),
        format!(
            "{}d old, {}d since update",
            f.age_days.round() as i64,
            f.stale_days.round() as i64
        ),
        ci.to_string(),
        state.join(", "),
        asks.join(", "),
        format!("{} recent comments", f.recent_comments),
        format!("{:.0}", e.scored.score),
    ]
}

const ROWS: &[&str] = &[
    "title",
    "pr",
    "author",
    "repo",
    "size",
    "age",
    "ci",
    "state",
    "asks",
    "discussion",
    "score now",
];

fn show_pair(out: &mut impl Write, a: &Entry, b: &Entry, n: usize, of: usize) -> Result<()> {
    let heading = format!("PAIR {n} OF {of}");
    writeln!(out, "{}", heading.if_supports_color(Stdout, |t| t.bold()))?;
    let mut table = Table::new();
    table
        .load_style(comfy_table::presets::UTF8_FULL_CONDENSED)
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(vec![
            Cell::new(""),
            Cell::new("A (left)").add_attribute(Attribute::Bold),
            Cell::new("B (right)").add_attribute(Attribute::Bold),
        ]);
    let (da, db) = (describe(a), describe(b));
    for ((label, va), vb) in ROWS.iter().zip(da).zip(db) {
        let same = va == vb;
        let dim = |s: String| {
            if same {
                Cell::new(s).add_attribute(Attribute::Dim)
            } else {
                Cell::new(s)
            }
        };
        table.add_row(vec![Cell::new(label), dim(va), dim(vb)]);
    }
    writeln!(out, "{table}")?;
    Ok(())
}

fn side(e: &Entry) -> Side {
    Side {
        repo: e.pr.repo.clone(),
        number: e.pr.number,
        title: e.pr.title.clone(),
        url: e.pr.url.clone(),
        facts: e.facts.clone(),
    }
}

fn report(out: &mut impl Write, cfg: &Config, fitted: &Weights, store: &Store) -> Result<()> {
    let js = &store.judgments;
    let count = |k: Kind| js.iter().filter(|j| j.kind == k).count();
    let ties = js.iter().filter(|j| j.winner == Winner::Tie).count();
    writeln!(
        out,
        "{} judgments stored ({} review, {} action, {} ties)",
        js.len(),
        count(Kind::Review),
        count(Kind::Action),
        ties
    )?;

    let after = Config {
        weights: fitted.clone(),
        ..cfg.clone()
    };
    let (before, after) = (agreement(js, cfg), agreement(js, &after));
    writeln!(
        out,
        "agreement with your non-tie answers: current weights {}/{}, fitted weights {}/{}",
        before.agree, before.total, after.agree, after.total
    )?;
    writeln!(out)?;

    let changed = changes(&cfg.weights, fitted);
    if changed.is_empty() {
        writeln!(
            out,
            "the fitted weights match your config; nothing to change"
        )?;
        return Ok(());
    }
    let mut table = Table::new();
    table
        .load_style(comfy_table::presets::UTF8_FULL_CONDENSED)
        .set_header(vec![
            Cell::new("weight").add_attribute(Attribute::Bold),
            Cell::new("current").add_attribute(Attribute::Bold),
            Cell::new("fitted").add_attribute(Attribute::Bold),
        ]);
    for (name, c, f) in &changed {
        table.add_row(vec![
            Cell::new(name),
            Cell::new(format!("{c:.2}")),
            Cell::new(format!("{f:.2}")).add_attribute(Attribute::Bold),
        ]);
    }
    writeln!(out, "{table}")?;
    Ok(())
}

/// Replace (or append) the top-level `weights:` block of a config file,
/// leaving every other line untouched. Comments inside the block are lost.
pub fn patch_weights(text: &str, w: &Weights) -> Result<String> {
    #[derive(Serialize)]
    struct Block<'a> {
        weights: &'a Weights,
    }
    let mut block = serde_saphyr::to_string(&Block { weights: w })?;
    if !block.ends_with('\n') {
        block.push('\n');
    }
    let lines: Vec<&str> = text.lines().collect();
    let Some(start) = lines.iter().position(|l| l.starts_with("weights:")) else {
        let mut s = text.to_string();
        if !s.is_empty() && !s.ends_with('\n') {
            s.push('\n');
        }
        if !s.is_empty() {
            s.push('\n');
        }
        s.push_str(&block);
        return Ok(s);
    };
    let continues = |l: &str| {
        l.trim().is_empty() || l.starts_with(' ') || l.starts_with('\t') || l.starts_with("- ")
    };
    let mut end = start + 1;
    while end < lines.len() && continues(lines[end]) {
        end += 1;
    }
    while end > start + 1 && lines[end - 1].trim().is_empty() {
        end -= 1;
    }
    let mut s = String::new();
    for l in &lines[..start] {
        s.push_str(l);
        s.push('\n');
    }
    s.push_str(&block);
    for l in &lines[end..] {
        s.push_str(l);
        s.push('\n');
    }
    Ok(s)
}

pub fn run(cfg: &Config, entries: &[Entry], args: &CalibrateArgs) -> Result<()> {
    let path = Store::path()?;
    let mut store = Store::load(&path)?;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    if args.rounds > 0 {
        if !std::io::stdin().is_terminal() {
            bail!("calibrate needs an interactive terminal (use --rounds 0 to only refit)");
        }
        let pool: Vec<&Entry> = entries
            .iter()
            .filter(|e| Kind::of(&e.scored.bucket) == Some(args.kind))
            .collect();
        if pool.len() < 2 {
            bail!(
                "only {} PR(s) in the {:?} section right now; need at least two to compare",
                pool.len(),
                args.kind
            );
        }
        let mut shown: HashMap<usize, usize> = HashMap::new();
        let mut answered = 0;
        while answered < args.rounds {
            let Some((i, j)) = pick_pair(&pool, &store, &shown) else {
                writeln!(out, "no more informative pairs in this section today")?;
                break;
            };
            show_pair(&mut out, pool[i], pool[j], answered + 1, args.rounds)?;
            *shown.entry(i).or_default() += 1;
            *shown.entry(j).or_default() += 1;
            match ask(&mut out)? {
                Answer::Pick(winner) => {
                    store.judgments.push(Judgment {
                        at: Timestamp::now(),
                        kind: args.kind,
                        a: side(pool[i]),
                        b: side(pool[j]),
                        winner,
                    });
                    store.save(&path)?;
                    answered += 1;
                }
                Answer::Skip => {
                    *shown.entry(i).or_default() += 4;
                    *shown.entry(j).or_default() += 4;
                }
                Answer::Quit => break,
            }
            writeln!(out)?;
        }
    }

    if store.judgments.is_empty() {
        writeln!(out, "no judgments stored yet ({})", path.display())?;
        return Ok(());
    }
    let fitted = fit(&store.judgments, cfg);
    report(&mut out, cfg, &fitted, &store)?;
    if changes(&cfg.weights, &fitted).is_empty() {
        return Ok(());
    }

    let config_path = Config::path()?;
    let question = format!("write these weights to {}?", config_path.display());
    if args.write || confirm(&mut out, &question)? {
        let text = std::fs::read_to_string(&config_path)
            .with_context(|| format!("reading {}", config_path.display()))?;
        let patched = patch_weights(&text, &fitted)?;
        Config::parse(&patched).context("patched config does not parse; nothing written")?;
        std::fs::write(&config_path, patched)
            .with_context(|| format!("writing {}", config_path.display()))?;
        writeln!(out, "wrote weights to {}", config_path.display())?;
    } else {
        writeln!(out, "not written (rerun with --rounds 0 --write to apply)")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::model::{AuthorTier, Mergeable, Pr, RepoTier};

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

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
            requested_me: true,
            requested_my_team: false,
            my_last_review: None,
            updated_since_my_review: false,
            approved_by_other: false,
            changes_requested_by_other: false,
            recent_comments: 0,
            mentioned_me: false,
        }
    }

    fn pr(n: u64) -> Pr {
        Pr {
            id: format!("PR_{n}"),
            number: n,
            repo: "acme/widgets".into(),
            title: format!("pr {n}"),
            url: format!("https://github.com/acme/widgets/pull/{n}"),
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
            last_commit_at: None,
            requested_users: vec![],
            requested_teams: vec![],
            reviews: vec![],
            commit_authors: vec![],
            comments: vec![],
            found_by: BTreeSet::new(),
        }
    }

    fn entry(n: u64, facts: PrFacts, cfg: &Config) -> Entry {
        let scored = scoring::triage(&facts, None, cfg);
        Entry {
            pr: pr(n),
            facts,
            scored,
        }
    }

    fn side(n: u64, facts: PrFacts) -> Side {
        let p = pr(n);
        Side {
            repo: p.repo,
            number: p.number,
            title: p.title,
            url: p.url,
            facts,
        }
    }

    /// Every combination of a few facts, all landing in Review.
    fn variants() -> Vec<PrFacts> {
        let mut v = Vec::new();
        for author_tier in [AuthorTier::Team, AuthorTier::Other] {
            for lines in [40, 300, 1000] {
                for stale_days in [0.0, 10.0] {
                    for ci in [CiState::Success, CiState::Failure] {
                        for recent_comments in [0, 2] {
                            v.push(PrFacts {
                                author_tier,
                                lines,
                                stale_days,
                                ci,
                                recent_comments,
                                ..facts()
                            });
                        }
                    }
                }
            }
        }
        v
    }

    /// Judgments a user with `truth` weights would give.
    fn synthetic(truth: &Config) -> Vec<Judgment> {
        let v = variants();
        let mut js = Vec::new();
        let mut k = 0;
        for i in 0..v.len() {
            for j in i + 1..v.len() {
                k += 1;
                if k % 7 != 0 {
                    continue;
                }
                let a = scoring::triage(&v[i], None, truth).score;
                let b = scoring::triage(&v[j], None, truth).score;
                let winner = if (a - b).abs() < 1.0 {
                    Winner::Tie
                } else if a > b {
                    Winner::A
                } else {
                    Winner::B
                };
                js.push(Judgment {
                    at: ts("2026-10-14T00:00:00Z"),
                    kind: Kind::Review,
                    a: side(i as u64, v[i].clone()),
                    b: side(j as u64, v[j].clone()),
                    winner,
                });
            }
        }
        js
    }

    #[test]
    fn read_write_roundtrip_covers_every_learnable_field() {
        let w = Weights::default();
        let v = read(&w);
        assert_eq!(v.len(), params(&w).len());
        assert_eq!(write(&w, &v), w);
        let mut bumped = v.clone();
        bumped[0] = 0.33;
        assert_eq!(write(&w, &bumped).author_other, 0.33);
    }

    #[test]
    fn no_judgments_fits_the_prior() {
        let cfg = Config::default();
        assert_eq!(fit(&[], &cfg), Weights::default());
    }

    #[test]
    fn fit_moves_toward_the_judge_and_improves_agreement() {
        let mut truth = Config::default();
        truth.weights.author_other = 0.2;
        truth.weights.state_ci_failing = 0.95;
        let js = synthetic(&truth);
        assert!(js.len() > 100, "{} judgments", js.len());

        let start = Config::default();
        let fitted = fit(&js, &start);
        assert!(
            fitted.author_other < 0.45,
            "author_other should drop from 0.6, got {}",
            fitted.author_other
        );
        assert!(
            fitted.state_ci_failing > 0.7,
            "state_ci_failing should rise from 0.5, got {}",
            fitted.state_ci_failing
        );
        assert!(valid(&fitted));

        let after = Config {
            weights: fitted,
            ..start.clone()
        };
        let (before, after) = (agreement(&js, &start), agreement(&js, &after));
        assert_eq!(before.total, after.total);
        assert!(after.agree > before.agree, "{before:?} -> {after:?}");
    }

    #[test]
    fn judgments_from_a_different_section_are_ignored() {
        let cfg = Config::default();
        let mut j = synthetic(&cfg).remove(0);
        j.kind = Kind::Action;
        assert_eq!(judgment_loss(&j, &cfg), None);
        assert_eq!(agreement(&[j], &cfg), Agreement::default());
    }

    #[test]
    fn tie_loss_is_zero_at_equal_scores_and_grows_with_the_gap() {
        let cfg = Config::default();
        let close = Judgment {
            at: ts("2026-10-14T00:00:00Z"),
            kind: Kind::Review,
            a: side(1, facts()),
            b: side(2, facts()),
            winner: Winner::Tie,
        };
        assert!(judgment_loss(&close, &cfg).unwrap().abs() < 1e-9);
        let far = Judgment {
            b: side(
                2,
                PrFacts {
                    lines: 5000,
                    ..facts()
                },
            ),
            ..close
        };
        assert!(judgment_loss(&far, &cfg).unwrap() > 0.1);
    }

    #[test]
    fn pick_pair_prefers_close_scores_and_skips_identical_or_judged_pairs() {
        let cfg = Config::default();
        let mid = PrFacts {
            lines: 150,
            ..facts()
        }; // size 0.85 vs 1.0
        let other = PrFacts {
            author_tier: AuthorTier::Other,
            ..facts()
        }; // author 0.6
        let pool_owned = [
            entry(1, facts(), &cfg),
            entry(2, facts(), &cfg), // identical to 1: never paired with it
            entry(3, mid, &cfg),
            entry(4, other, &cfg),
        ];
        let pool: Vec<&Entry> = pool_owned.iter().collect();
        let mut store = Store::default();
        let shown = HashMap::new();

        let (i, j) = pick_pair(&pool, &store, &shown).unwrap();
        assert_eq!((i, j), (0, 2), "small vs mid-size is the closest pair");

        store.judgments.push(Judgment {
            at: ts("2026-10-14T00:00:00Z"),
            kind: Kind::Review,
            a: side(3, pool[2].facts.clone()),
            b: side(1, pool[0].facts.clone()),
            winner: Winner::A,
        });
        let (i, j) = pick_pair(&pool, &store, &shown).unwrap();
        assert_eq!(
            (i, j),
            (1, 2),
            "same pair in either order is not asked again"
        );

        let shown: HashMap<usize, usize> = [(2, 3)].into();
        let (i, j) = pick_pair(&pool, &store, &shown).unwrap();
        assert!(
            i != 2 && j != 2,
            "a PR shown three times is pushed back, got {i},{j}"
        );
    }

    #[test]
    fn store_roundtrips_through_yaml() {
        let store = Store {
            judgments: synthetic(&Config::default()).into_iter().take(2).collect(),
        };
        let text = serde_saphyr::to_string(&store).unwrap();
        let back: Store = serde_saphyr::from_str(&text).unwrap();
        assert_eq!(back.judgments.len(), 2);
        assert_eq!(back.judgments[0].a.url, store.judgments[0].a.url);
        assert_eq!(back.judgments[0].winner, store.judgments[0].winner);
        assert_eq!(
            back.judgments[0].a.facts.lines,
            store.judgments[0].a.facts.lines
        );
    }

    #[test]
    fn patch_replaces_the_weights_block_and_keeps_everything_else() {
        let text = "me: felipe\n\nweights:\n  author_other: 0.9   # hand tuned\n  size_bands:\n    - max_lines: 50\n      weight: 1.0\n\nthresholds:\n  close_after_days: 10\n";
        let w = Weights {
            author_other: 0.42,
            ..Weights::default()
        };
        let patched = patch_weights(text, &w).unwrap();
        assert!(patched.starts_with("me: felipe\n\nweights:\n"), "{patched}");
        assert!(
            patched.ends_with("\nthresholds:\n  close_after_days: 10\n"),
            "{patched}"
        );
        assert!(!patched.contains("hand tuned"));
        let cfg = Config::parse(&patched).unwrap();
        assert_eq!(cfg.weights, w);
        assert_eq!(cfg.me.as_deref(), Some("felipe"));
        assert_eq!(cfg.thresholds.close_after_days, 10.0);
    }

    #[test]
    fn patch_appends_when_there_is_no_weights_block() {
        let text = "me: felipe\nthresholds: {close_after_days: 10}";
        let w = Weights::default();
        let patched = patch_weights(text, &w).unwrap();
        assert!(
            patched.starts_with("me: felipe\nthresholds: {close_after_days: 10}\n\nweights:\n"),
            "{patched}"
        );
        assert_eq!(Config::parse(&patched).unwrap().weights, w);
    }

    #[test]
    fn patch_handles_a_flow_style_block_at_the_end() {
        let text = "weights: {author_other: 0.1}";
        let patched = patch_weights(text, &Weights::default()).unwrap();
        assert_eq!(Config::parse(&patched).unwrap().weights, Weights::default());
    }
}

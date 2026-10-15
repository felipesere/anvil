mod calibrate;
mod config;
mod enrich;
mod github;
mod model;
mod render;
mod scoring;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use jiff::Timestamp;

use crate::config::Config;
use crate::enrich::{Enricher, NoopEnricher};
use crate::github::SearchPage;
use crate::model::{PrFacts, Query};
use crate::render::{Entry, Report};

/// Morning triage of the GitHub pull requests that need your attention.
#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    #[command(flatten)]
    triage: TriageArgs,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, clap::Args)]
struct TriageArgs {
    /// Show the scoring factors under each PR.
    #[arg(long)]
    explain: bool,

    /// Emit the full report as JSON instead of text.
    #[arg(long)]
    json: bool,

    /// Also list PRs that matched nothing (FYI section).
    #[arg(long)]
    all: bool,

    /// Show at most this many PRs per section (0 = unlimited).
    #[arg(long, default_value_t = 10, value_name = "N")]
    top: usize,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Learn the scoring weights from your answers to "which PR matters more?".
    Calibrate(calibrate::CalibrateArgs),
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let cfg = Config::load()?;
    let entries = fetch_entries(&cfg).await?;

    match cli.command {
        Some(Command::Calibrate(args)) => calibrate::run(&cfg, &entries, &args),
        None => triage(&cfg, entries, &cli.triage),
    }
}

/// Search, fetch details and score every candidate PR.
async fn fetch_entries(cfg: &Config) -> Result<Vec<Entry>> {
    let client = github::Client::new(&github::discover_token()?)?;
    let (me, searches) = tokio::try_join!(
        async {
            match &cfg.me {
                Some(me) => Ok(me.clone()),
                None => client.viewer().await.context("resolving viewer login"),
            }
        },
        github::search_all(&client, github::search_queries(cfg)),
    )?;
    for (_, q, page) in &searches {
        let missed = page.truncated();
        if missed > 0 {
            eprintln!(
                "note: `{q}` matched {} PRs; only the 100 most recently updated were considered",
                page.total_count
            );
        }
    }
    let searches: Vec<(Query, SearchPage)> = searches
        .into_iter()
        .map(|(query, _, page)| (query, page))
        .collect();

    let ids: Vec<String> = github::candidates(searches.clone())
        .into_iter()
        .filter(|(item, _)| !item.draft)
        .map(|(item, _)| item.node_id)
        .collect();
    let details = github::fetch_details(&client, &ids).await?;

    let now = Timestamp::now();
    let enricher = NoopEnricher;
    Ok(github::assemble(github::candidates(searches), details)
        .into_iter()
        .map(|pr| {
            let facts = PrFacts::compute(&pr, cfg, &me, now);
            let enrichment = enricher.enrich(&pr);
            let scored = scoring::triage(&facts, enrichment.as_ref(), cfg);
            Entry { pr, facts, scored }
        })
        .collect())
}

fn triage(cfg: &Config, entries: Vec<Entry>, args: &TriageArgs) -> Result<()> {
    let mut report = Report::build(entries, cfg.thresholds.min_review_score, args.top);
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    if args.json {
        report.truncate(args.top);
        report.print_json(&mut out)?;
    } else {
        report.print_terminal(&mut out, args.explain, args.all, args.top)?;
    }
    Ok(())
}

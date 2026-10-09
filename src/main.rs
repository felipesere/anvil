mod config;
mod enrich;
mod github;
mod model;
mod render;
mod scoring;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use jiff::Timestamp;

use crate::config::Config;
use crate::enrich::{Enricher, NoopEnricher};
use crate::github::{Fixture, FixtureSearch, SearchPage};
use crate::model::{PrFacts, Query};
use crate::render::{Entry, Report};

/// Morning triage of the GitHub pull requests that need your attention.
#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    /// Config file (default: ~/.config/anvil/config.yaml).
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Show the scoring factors under each PR.
    #[arg(long)]
    explain: bool,

    /// Emit the full report as JSON instead of text.
    #[arg(long)]
    json: bool,

    /// Also list PRs that matched nothing (FYI section).
    #[arg(long)]
    all: bool,

    /// Replay a saved fixture instead of calling GitHub.
    #[arg(long, value_name = "FILE", conflicts_with = "dump_fixture")]
    fixture: Option<PathBuf>,

    /// Save the raw GitHub responses to FILE for later `--fixture` replay.
    #[arg(long, value_name = "FILE")]
    dump_fixture: Option<PathBuf>,

    /// Pretend it is this instant (RFC 3339). Mainly for deterministic fixture replays.
    #[arg(long, hide = true, value_name = "TIMESTAMP")]
    now: Option<Timestamp>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let cfg = Config::load(cli.config.as_deref())?;

    let (me, searches, details) = match &cli.fixture {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("reading fixture {}", path.display()))?;
            let fixture: Fixture = serde_json::from_str(&text)
                .with_context(|| format!("parsing fixture {}", path.display()))?;
            let searches = fixture
                .searches
                .into_iter()
                .map(|s| (s.query, s.page))
                .collect();
            (fixture.me, searches, fixture.details)
        }
        None => {
            let client = github::Client::new(&github::discover_token()?)?;
            let (me, searches) = tokio::try_join!(
                async {
                    match &cfg.me {
                        Some(me) => Ok(me.clone()),
                        None => client.viewer().await.context("resolving viewer login"),
                    }
                },
                github::search_all(&client, github::search_queries(&cfg)),
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

            if let Some(path) = &cli.dump_fixture {
                let fixture = Fixture {
                    me: me.clone(),
                    searches: searches
                        .iter()
                        .map(|(query, page)| FixtureSearch {
                            query: *query,
                            page: page.clone(),
                        })
                        .collect(),
                    details: details.clone(),
                };
                std::fs::write(path, serde_json::to_vec_pretty(&fixture)?)
                    .with_context(|| format!("writing fixture {}", path.display()))?;
            }
            (me, searches, details)
        }
    };

    let now = cli.now.unwrap_or_else(Timestamp::now);
    let enricher = NoopEnricher;
    let entries: Vec<Entry> = github::assemble(github::candidates(searches), details)
        .into_iter()
        .map(|pr| {
            let facts = PrFacts::compute(&pr, &cfg, &me, now);
            let enrichment = enricher.enrich(&pr);
            let scored = scoring::triage(&facts, enrichment.as_ref(), &cfg);
            Entry { pr, facts, scored }
        })
        .collect();

    let report = Report::build(entries);
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    if cli.json {
        report.print_json(&mut out)?;
    } else {
        report.print_terminal(&mut out, cli.explain, cli.all)?;
    }
    Ok(())
}

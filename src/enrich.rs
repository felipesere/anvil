//! Optional enrichment hook. v1 ships only the no-op implementation.
//!
//! Rule of thumb: an enricher is only justified for judgements static rules
//! cannot make. Anything derivable from paths, counts or GitHub metadata
//! belongs in `model.rs` / `scoring.rs` instead.

use serde::{Deserialize, Serialize};

use crate::model::Pr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChangeKind {
    Docs,
    Config,
    Tests,
    Logic,
    Mixed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Clarity {
    Clear,
    Vague,
    Missing,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Enrichment {
    pub change_kind: ChangeKind,
    pub description_clarity: Clarity,
}

pub trait Enricher {
    fn enrich(&self, pr: &Pr) -> Option<Enrichment>;
}

pub struct NoopEnricher;

impl Enricher for NoopEnricher {
    fn enrich(&self, _pr: &Pr) -> Option<Enrichment> {
        None
    }
}

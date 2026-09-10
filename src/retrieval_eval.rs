//! Metrics for externally labeled retrieval cases. Overlapping chunks cannot
//! inflate file ranks or line coverage.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Region {
    pub path: String,
    pub lines: [usize; 2],
}

#[derive(Debug, Serialize)]
pub struct Metrics {
    pub passage_reciprocal_rank: f64,
    pub file_recall: f64,
    pub reciprocal_rank: f64,
    pub file_ndcg: f64,
    pub line_recall: f64,
    pub returned_lines: usize,
    pub relevant_line_precision: f64,
}

fn ranges(regions: &[Region]) -> Result<BTreeMap<&str, Vec<(usize, usize)>>> {
    let mut result: BTreeMap<&str, Vec<(usize, usize)>> = BTreeMap::new();
    for region in regions {
        ensure!(
            !region.path.is_empty()
                && region.lines[0] > 0
                && region.lines[0] <= region.lines[1]
                && region.lines[1] < usize::MAX,
            "Invalid source region"
        );
        result
            .entry(&region.path)
            .or_default()
            .push((region.lines[0], region.lines[1] + 1));
    }
    for spans in result.values_mut() {
        spans.sort_unstable();
        let mut merged: Vec<(usize, usize)> = Vec::new();
        for &(start, end) in spans.iter() {
            if let Some(last) = merged.last_mut()
                && start <= last.1
            {
                last.1 = last.1.max(end);
            } else {
                merged.push((start, end));
            }
        }
        *spans = merged;
    }
    Ok(result)
}

pub fn score(relevant: &[Region], ranked: &[Region]) -> Result<Metrics> {
    ensure!(
        !relevant.is_empty(),
        "Positive retrieval cases require source labels"
    );
    let gold = ranges(relevant)?;
    let retrieved = ranges(ranked)?;
    let mut seen = BTreeSet::new();
    let mut hits = 0;
    let mut reciprocal_rank = 0.0;
    let mut dcg = 0.0;
    for region in ranked {
        if !seen.insert(region.path.as_str()) {
            continue;
        }
        if gold.contains_key(region.path.as_str()) {
            hits += 1;
            if reciprocal_rank == 0.0 {
                reciprocal_rank = 1.0 / seen.len() as f64;
            }
            dcg += 1.0 / ((seen.len() + 1) as f64).log2();
        }
    }
    let ideal: f64 = (0..gold.len().min(seen.len()))
        .map(|i| 1.0 / ((i + 2) as f64).log2())
        .sum();
    let count = |spans: &BTreeMap<&str, Vec<(usize, usize)>>| -> Result<usize> {
        spans.values().flatten().try_fold(0usize, |total, (s, e)| {
            total
                .checked_add(e - s)
                .ok_or_else(|| anyhow::anyhow!("Source coverage overflow"))
        })
    };
    let total = count(&gold)?;
    let returned_lines = count(&retrieved)?;
    let mut overlap = 0;
    for (path, spans) in &gold {
        if let Some(found) = retrieved.get(path) {
            for &(s, e) in spans {
                for &(a, b) in found {
                    overlap += e.min(b).saturating_sub(s.max(a));
                }
            }
        }
    }
    let passage_reciprocal_rank = ranked
        .iter()
        .position(|region| {
            relevant.iter().any(|label| {
                label.path == region.path
                    && label.lines[0] <= region.lines[1]
                    && region.lines[0] <= label.lines[1]
            })
        })
        .map_or(0.0, |rank| 1.0 / (rank + 1) as f64);
    Ok(Metrics {
        passage_reciprocal_rank,
        file_recall: hits as f64 / gold.len() as f64,
        reciprocal_rank,
        file_ndcg: if ideal > 0.0 { dcg / ideal } else { 0.0 },
        line_recall: overlap as f64 / total as f64,
        returned_lines,
        relevant_line_precision: if returned_lines > 0 {
            overlap as f64 / returned_lines as f64
        } else {
            0.0
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn region(path: &str, start: usize, end: usize) -> Region {
        Region {
            path: path.into(),
            lines: [start, end],
        }
    }
    #[test]
    fn duplicate_chunks_do_not_inflate_metrics() {
        let metrics = score(
            &[region("a", 10, 20)],
            &[
                region("wrong", 1, 5),
                region("a", 10, 15),
                region("a", 12, 18),
            ],
        )
        .unwrap();
        assert_eq!(metrics.reciprocal_rank, 0.5);
        assert_eq!(metrics.file_recall, 1.0);
        assert_eq!(metrics.returned_lines, 14);
        assert_eq!(metrics.line_recall, 9.0 / 11.0);
    }
    #[test]
    fn misses_and_invalid_labels_are_explicit() {
        assert_eq!(score(&[region("a", 1, 2)], &[]).unwrap().file_ndcg, 0.0);
        assert!(score(&[], &[]).is_err());
        assert!(score(&[region("a", 2, 1)], &[]).is_err());
    }
}

/// Per-case quality gates, independent of production retrieval configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Thresholds {
    pub min_file_recall: f64,
    pub min_line_recall: f64,
    pub min_passage_reciprocal_rank: f64,
    pub min_excerpt_line_recall: f64,
    pub max_results: usize,
}
impl Default for Thresholds {
    fn default() -> Self {
        Self {
            min_file_recall: 1.0,
            min_line_recall: 1.0,
            min_passage_reciprocal_rank: 0.0,
            min_excerpt_line_recall: 0.0,
            max_results: 20,
        }
    }
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(
    tag = "kind",
    content = "thresholds",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Expectation {
    Retrieve(Thresholds),
    Abstain,
}
impl Default for Expectation {
    fn default() -> Self {
        Self::Retrieve(Thresholds::default())
    }
}
impl Expectation {
    pub fn validate(&self, labels: &[Region]) -> Result<()> {
        match self {
            Self::Abstain => ensure!(
                labels.is_empty(),
                "Abstention cases cannot have positive labels"
            ),
            Self::Retrieve(gates) => {
                score(labels, &[])?;
                for value in [
                    gates.min_file_recall,
                    gates.min_line_recall,
                    gates.min_passage_reciprocal_rank,
                    gates.min_excerpt_line_recall,
                ] {
                    ensure!(
                        value.is_finite() && (0.0..=1.0).contains(&value),
                        "Quality gates must be in [0,1]"
                    );
                }
                ensure!(
                    (1..=20).contains(&gates.max_results),
                    "max_results must be 1–20"
                );
            }
        }
        Ok(())
    }
}

#[derive(Debug, Serialize)]
pub struct Assessment {
    pub passed: bool,
    pub failures: Vec<String>,
    pub selected: Option<Metrics>,
    pub candidates: Option<Metrics>,
    pub excerpts: Option<Metrics>,
    pub at_k: BTreeMap<usize, Metrics>,
}

/// Score selection separately from candidate generation and actually delivered
/// complete excerpt lines. A clipped partial line earns no coverage credit.
pub fn assess(
    labels: &[Region],
    expectation: &Expectation,
    response: &serde_json::Value,
    trace: &[serde_json::Value],
) -> Result<Assessment> {
    expectation.validate(labels)?;
    let results = response["results"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("Missing results"))?;
    let abstained = response["abstained"]
        .as_bool()
        .ok_or_else(|| anyhow::anyhow!("Missing abstention status"))?;
    ensure!(
        abstained == results.is_empty(),
        "Inconsistent abstention status"
    );
    let ranked: Vec<Region> = serde_json::from_value(response["results"].clone())?;
    let candidates: Vec<Region> = trace
        .iter()
        .cloned()
        .map(serde_json::from_value)
        .collect::<std::result::Result<_, _>>()?;
    let mut report = Assessment {
        passed: true,
        failures: Vec::new(),
        selected: None,
        candidates: None,
        excerpts: None,
        at_k: BTreeMap::new(),
    };
    match expectation {
        Expectation::Abstain => {
            if !abstained {
                report
                    .failures
                    .push("Expected abstention, received code results".into());
            }
        }
        Expectation::Retrieve(gates) => {
            let selected = score(labels, &ranked)?;
            let mut excerpts = Vec::new();
            for (result, region) in results.iter().zip(&ranked) {
                let text = result["excerpt"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("Missing excerpt"))?;
                let complete = text.strip_suffix("\n[excerpt limited]").map_or_else(
                    || text.lines().count(),
                    |prefix| prefix.bytes().filter(|byte| *byte == b'\n').count(),
                );
                if complete > 0 {
                    excerpts.push(Region {
                        path: region.path.clone(),
                        lines: [
                            region.lines[0],
                            region.lines[0]
                                .saturating_add(complete - 1)
                                .min(region.lines[1]),
                        ],
                    });
                }
            }
            let excerpt_metrics = score(labels, &excerpts)?;
            for (name, actual, minimum) in [
                ("file_recall", selected.file_recall, gates.min_file_recall),
                ("line_recall", selected.line_recall, gates.min_line_recall),
                (
                    "passage_reciprocal_rank",
                    selected.passage_reciprocal_rank,
                    gates.min_passage_reciprocal_rank,
                ),
                (
                    "excerpt_line_recall",
                    excerpt_metrics.line_recall,
                    gates.min_excerpt_line_recall,
                ),
            ] {
                if actual < minimum {
                    report
                        .failures
                        .push(format!("{name}: {actual:.4} < {minimum:.4}"));
                }
            }
            if results.len() > gates.max_results {
                report.failures.push(format!(
                    "Too many results: {} > {}",
                    results.len(),
                    gates.max_results
                ));
            }
            for k in [1, 3, 5, 10] {
                report
                    .at_k
                    .insert(k, score(labels, &ranked[..ranked.len().min(k)])?);
            }
            report.selected = Some(selected);
            report.candidates = Some(score(labels, &candidates)?);
            report.excerpts = Some(excerpt_metrics);
        }
    }
    report.passed = report.failures.is_empty();
    Ok(report)
}

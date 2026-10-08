//! Identity and freshness of durable research evidence.
use anyhow::{Context, Result};
use builder_core::research::{Observation, Record, Request};
use builder_tools::{Workspace, research as execution};
use std::collections::HashSet;

pub(super) fn identifier(record: &Record) -> String {
    format!("{}:{}", record.session, record.call_id)
}
pub(super) fn lookup<'a>(records: &'a [Record], session: &str, id: &str) -> Result<&'a Record> {
    records.iter().find(|r|identifier(r)==id || (r.session==session&&r.call_id==id)).context("Evidence missing, rewound, failed, or outside the configured retrieval window; run a fresh observation/check")
}
pub(super) fn observations(record: &Record) -> Result<Vec<Observation>> {
    Ok(serde_json::from_value(
        record.result["observations"].clone(),
    )?)
}
pub(super) fn verification_fresh(
    record: &Record,
    workspace: &Workspace,
    hash: Option<&str>,
    records: &[Record],
) -> bool {
    if records.iter().any(|newer| {
        newer.seq > record.seq
            && matches!(newer.request, Request::Verify { .. })
            && newer.result["command"] == record.result["command"]
            && newer.result["passed"] != true
    }) {
        return false;
    }
    matches!(record.request, Request::Verify { .. })
        && record.result["passed"] == true
        && hash.is_some_and(|h| record.result["snapshot"]["hash"].as_str() == Some(h))
        && observations(record).is_ok_and(|o| execution::fresh(workspace, &o))
}
pub(super) fn current_procedures(records: &[Record]) -> Vec<&Record> {
    let mut seen = HashSet::new();
    records
        .iter()
        .filter(|r| match &r.request {
            Request::Learn { key, .. } | Request::Retire { key, .. } => seen.insert(key.clone()),
            _ => false,
        })
        .collect()
}
pub(super) fn procedure_fresh(
    record: &Record,
    records: &[Record],
    workspace: &Workspace,
    hash: Option<&str>,
) -> bool {
    let Request::Learn {
        verification_ids, ..
    } = &record.request
    else {
        return false;
    };
    verification_ids.iter().all(|id| {
        lookup(records, &record.session, id)
            .is_ok_and(|r| verification_fresh(r, workspace, hash, records))
    })
}

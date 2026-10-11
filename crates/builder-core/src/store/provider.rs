//! Stored provider conformance reports.
use super::Store;
use anyhow::{Result, ensure};
use rusqlite::params;

impl Store {
    pub fn save_provider_conformance(
        &mut self,
        fingerprint: &str,
        profile: &str,
        endpoint: &str,
        model: &str,
        report: &serde_json::Value,
    ) -> Result<()> {
        ensure!(
            !fingerprint.is_empty()
                && fingerprint.len() <= 256
                && !profile.is_empty()
                && profile.len() <= 256
                && endpoint.len() <= 2048
                && !model.is_empty()
                && model.len() <= 1024,
            "Invalid provider conformance identity"
        );
        let report = serde_json::to_string(report)?;
        ensure!(
            report.len() <= 16 * 1024,
            "Provider conformance report exceeds storage bound"
        );
        self.conn.execute(
            "INSERT INTO provider_conformance(fingerprint,profile,endpoint,model,checked_at,report)
             VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(fingerprint) DO UPDATE SET
             profile=excluded.profile,endpoint=excluded.endpoint,model=excluded.model,
             checked_at=excluded.checked_at,report=excluded.report",
            params![
                fingerprint,
                profile,
                endpoint,
                model,
                chrono::Utc::now().to_rfc3339(),
                report
            ],
        )?;
        Ok(())
    }
}

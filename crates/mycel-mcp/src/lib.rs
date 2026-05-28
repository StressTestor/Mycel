use std::{io::Read, path::Path};

use chrono::{DateTime, Utc};
use mycel_core::{
    Antibody, AntibodyStore, Evaluation, HarnessMetrics, ProposedRun, Result,
    SentinelAntibodyCandidate,
};

pub fn tool_surface_name() -> &'static str {
    mycel_core::CORE_CRATE_NAME
}

pub struct McpTools {
    store: AntibodyStore,
}

impl McpTools {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Ok(Self {
            store: AntibodyStore::open(path)?,
        })
    }

    pub fn open_in_memory() -> Result<Self> {
        Ok(Self {
            store: AntibodyStore::open_in_memory()?,
        })
    }

    pub fn ingest_sentinel(
        &self,
        reader: impl Read,
        now: DateTime<Utc>,
    ) -> Result<Vec<SentinelAntibodyCandidate>> {
        self.store.ingest_sentinel_audit_jsonl(reader, now)
    }

    pub fn insert_antibodies(&self, antibodies: impl IntoIterator<Item = Antibody>) -> Result<()> {
        for antibody in antibodies {
            self.store.insert_antibody(&antibody)?;
        }
        Ok(())
    }

    pub fn evaluate(&self, run: &ProposedRun, now: DateTime<Utc>) -> Result<Evaluation> {
        self.store.evaluate_run(run, now)
    }

    pub fn list_antibodies(&self) -> Result<Vec<Antibody>> {
        self.store.list_antibodies()
    }

    pub fn run_harness(&self, now: DateTime<Utc>) -> Result<HarnessMetrics> {
        mycel_core::run_v0_1_harness(now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delegates_to_core_surface() {
        assert_eq!(tool_surface_name(), "mycel-core");
    }
}

use std::collections::BTreeSet;

use thiserror::Error;

use crate::{GateResult, GateStatus};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CertificationVerdict {
    Passed,
    Degraded,
    Blocked,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum PolicyError {
    #[error("suite must not be empty")]
    EmptySuite,
    #[error("check must not be empty")]
    EmptyCheck,
    #[error("missing required suite: {0}")]
    MissingRequiredSuite(String),
    #[error("duplicate check for suite {suite:?} and check {check:?}")]
    DuplicateCheck { suite: String, check: String },
}

pub fn evaluate(
    required_suites: &[&str],
    results: &[GateResult],
) -> Result<CertificationVerdict, PolicyError> {
    let mut observed_suites = BTreeSet::new();
    let mut checks = BTreeSet::new();
    let mut has_degraded = false;
    let mut has_blocked = false;

    for suite in required_suites {
        if suite.is_empty() {
            return Err(PolicyError::EmptySuite);
        }
    }

    for result in results {
        if result.suite.is_empty() {
            return Err(PolicyError::EmptySuite);
        }
        if result.check.is_empty() {
            return Err(PolicyError::EmptyCheck);
        }

        let key = (result.suite.clone(), result.check.clone());
        if !checks.insert(key.clone()) {
            return Err(PolicyError::DuplicateCheck {
                suite: key.0,
                check: key.1,
            });
        }

        observed_suites.insert(result.suite.as_str());
        match result.status {
            GateStatus::Passed => {}
            GateStatus::Degraded => has_degraded = true,
            GateStatus::Blocked => has_blocked = true,
        }
    }

    for suite in required_suites {
        if !observed_suites.contains(suite) {
            return Err(PolicyError::MissingRequiredSuite((*suite).to_owned()));
        }
    }

    if has_blocked {
        Ok(CertificationVerdict::Blocked)
    } else if has_degraded {
        Ok(CertificationVerdict::Degraded)
    } else {
        Ok(CertificationVerdict::Passed)
    }
}

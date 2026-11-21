//! credential diagnostics and probes.
use super::DoctorContext;
use crate::doctor::{
    bootstrap_local, resolve_bootstrap_path, DoctorCheck, DoctorReport, DoctorStatus, Result,
};

pub(super) fn credential(context: &mut DoctorContext, report: &mut DoctorReport) -> Result<()> {
    let bootstrap_cli = context.bootstrap_cli.clone();
    if let Ok(credential) = broker::StartupCredential::from_env() {
        report.ok("bootstrap", "credential from environment".to_string());
        let expiry = check_bootstrap_expiry(credential.expires_at());
        report.record(expiry.status, &expiry.name, expiry.detail);
    } else {
        match resolve_bootstrap_path(bootstrap_cli.clone()) {
            Ok(path) if path.exists() => {
                report.ok(
                    "bootstrap",
                    format!("credential file at {}", path.display()),
                );
                match bootstrap_local::load_startup_from_env_file(&path) {
                    Ok(credential) => {
                        let expiry = check_bootstrap_expiry(credential.expires_at());
                        report.record(expiry.status, &expiry.name, expiry.detail);
                    }
                    Err(error) => {
                        report.fail("bootstrap-expiry", format!("{error:#}"));
                    }
                }
            }
            Ok(path) => {
                report.warn(
                    "bootstrap",
                    format!(
                        "no credential yet; `bobby serve` will generate one at {}",
                        path.display()
                    ),
                );
            }
            Err(error) => {
                report.fail("bootstrap", format!("{error:#}"));
            }
        }
    }

    Ok(())
}

pub(crate) fn check_bootstrap_expiry(expires_at: types::WireTimestamp) -> DoctorCheck {
    let remaining = expires_at - types::WireTimestamp::now();
    if remaining <= jiff::SignedDuration::ZERO {
        DoctorCheck {
            status: DoctorStatus::Fail,
            name: "bootstrap-expiry".to_string(),
            detail: format!(
                "credential expired at {}; run `bobby init --force`",
                expires_at.rfc3339()
            ),
        }
    } else if remaining
        < jiff::SignedDuration::from_hours(
            (BOOTSTRAP_EXPIRY_WARN_DAYS)
                .checked_mul(24)
                .expect("duration overflow"),
        )
    {
        DoctorCheck {
            status: DoctorStatus::Warn,
            name: "bootstrap-expiry".to_string(),
            detail: format!(
                "credential expires in {} day(s) at {}; run `bobby init --force` before then",
                (remaining.as_secs() / 86_400),
                expires_at.rfc3339()
            ),
        }
    } else {
        DoctorCheck {
            status: DoctorStatus::Ok,
            name: "bootstrap-expiry".to_string(),
            detail: format!(
                "credential valid for {} more day(s)",
                (remaining.as_secs() / 86_400)
            ),
        }
    }
}

pub(crate) const BOOTSTRAP_EXPIRY_WARN_DAYS: i64 = 7;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn expiry_row_distinguishes_expired_expiring_and_valid_credentials() {
        let now = types::WireTimestamp::now();
        for (days, expected) in [
            (-1, DoctorStatus::Fail),
            (1, DoctorStatus::Warn),
            (8, DoctorStatus::Ok),
        ] {
            let check = check_bootstrap_expiry(
                now + jiff::SignedDuration::from_hours(
                    (days as i64).checked_mul(24).expect("duration overflow"),
                ),
            );
            assert_eq!(check.name, "bootstrap-expiry");
            assert_eq!(check.status, expected);
            if expected != DoctorStatus::Ok {
                assert!(check.detail.contains("bobby init --force"));
            }
        }
    }
}

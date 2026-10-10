//! sidecars diagnostics and probes.
use super::DoctorContext;
use crate::doctor::{
    onboarding, push_doctor_check, DoctorCheck, DoctorReport, DoctorStatus, Path, Result,
};

pub(super) fn sidecars(_context: &mut DoctorContext, report: &mut DoctorReport) -> Result<()> {
    // Sidecar gateways must sit beside bobby (or on PATH) for mcp-stdio /
    // acp-stdio. Missing binaries are a warning with an install hint.
    let mcp_bin = onboarding::find_sidecar_binary(onboarding::mcp_gateway_command());
    let acp_bin = onboarding::find_sidecar_binary(onboarding::acp_gateway_command());
    for (name, command, path) in [
        (
            "mcp-gateway",
            onboarding::mcp_gateway_command(),
            mcp_bin.as_deref(),
        ),
        (
            "acp-gateway",
            onboarding::acp_gateway_command(),
            acp_bin.as_deref(),
        ),
    ] {
        match path {
            Some(path) => report.ok(name, path.display().to_string()),
            None => report.warn(
                name,
                format!(
                    "{command} not found next to bobby or on PATH; install with `bobby install --cli`, re-run scripts/install.sh, or `cargo build -p {command} --release`"
                ),
            ),
        }
    }
    match sidecar_versions(mcp_bin.as_deref(), acp_bin.as_deref()) {
        Ok((mcp, acp)) => {
            if let Some(check) =
                sidecar_version_status(env!("CARGO_PKG_VERSION"), mcp.as_deref(), acp.as_deref())
            {
                push_doctor_check(report, check);
            }
        }
        Err(detail) => report.fail("sidecar-version", detail),
    }

    Ok(())
}

pub(crate) fn sidecar_version_status(
    bobby: &str,
    mcp: Option<&str>,
    acp: Option<&str>,
) -> Option<DoctorCheck> {
    if mcp.is_none() && acp.is_none() {
        return None;
    }
    if let Some((name, version)) = [("mcp-gateway", mcp), ("acp-gateway", acp)]
        .into_iter()
        .find_map(|(name, version)| {
            version.and_then(|version| (version != bobby).then_some((name, version)))
        })
    {
        return Some(DoctorCheck {
            status: DoctorStatus::Fail,
            name: "sidecar-version".to_string(),
            detail: format!("{name} {version} does not match bobby {bobby}"),
        });
    }
    Some(DoctorCheck {
        status: DoctorStatus::Ok,
        name: "sidecar-version".to_string(),
        detail: format!("matches bobby {bobby}"),
    })
}

pub(in crate::doctor) fn sidecar_versions(
    mcp: Option<&Path>,
    acp: Option<&Path>,
) -> Result<(Option<String>, Option<String>), String> {
    let read = |path: Option<&Path>| match path {
        None => Ok(None),
        Some(path) => onboarding::sidecar_version(path)
            .map(Some)
            .map_err(|error| format!("{}: {error:#}", path.display())),
    };
    Ok((read(mcp)?, read(acp)?))
}

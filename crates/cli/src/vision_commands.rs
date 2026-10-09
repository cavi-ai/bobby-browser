//! Vision subcommand orchestration over the existing feature entrypoints.
use crate::{
    prepare_jobs_client, run_configured_vision_service, vision_collect, vision_connect,
    vision_login, vision_solve, vision_status, VisionCommands,
};
use anyhow::Result;
pub(crate) async fn run(command: VisionCommands) -> Result<()> {
    match command {
        VisionCommands::Connect(args) => vision_connect::connect(args.into())?,
        VisionCommands::Login(args) => vision_login::login(args.config, &args.name).await?,
        VisionCommands::Status { config } => vision_status(config).await?,
        VisionCommands::Start { config } => run_configured_vision_service(config).await?,
        VisionCommands::Collect {
            output,
            examples,
            journey,
        } => {
            vision_collect::run_collect(output, examples, journey)?;
        }
        VisionCommands::Solve {
            purpose,
            url,
            session,
            page,
            node,
            timeout_ms,
            zigzagzig,
            common,
        } => {
            let (base_url, bearer) = prepare_jobs_client(&common)?;
            vision_solve::solve(vision_solve::VisionSolveOptions {
                purpose,
                url,
                session,
                page,
                node,
                timeout_ms,
                zigzagzig,
                base_url,
                bearer,
            })?;
        }
        VisionCommands::Detect {
            purpose,
            url,
            session,
            page,
            node,
            timeout_ms,
            common,
        } => {
            let (base_url, bearer) = prepare_jobs_client(&common)?;
            vision_solve::detect(vision_solve::VisionDetectOptions {
                purpose,
                url,
                session,
                page,
                node,
                timeout_ms,
                base_url,
                bearer,
            })?;
        }
    };
    Ok(())
}

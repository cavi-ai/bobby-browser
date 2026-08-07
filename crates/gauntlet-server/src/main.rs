//! Standalone Northstar gauntlet scenario server for out-of-process drivers.
//!
//!   cargo run -p gauntlet-server -- --seed demo
//!   cargo run -p gauntlet-server -- --seed demo --level 2 \
//!     --recaptcha-site-key … --recaptcha-secret …
//!
//! Prints the onboarding URL on stdout, then serves until Ctrl-C. Drivers
//! verify outcomes over `GET /__gauntlet/snapshot` and
//! `GET /__gauntlet/request-log`.

use gauntlet_server::{ScenarioConfig, ScenarioServer};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut seed: Option<String> = None;
    let mut level = 1u8;
    let mut site_key: Option<String> = None;
    let mut secret: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--seed" => seed = Some(args.next().ok_or("--seed needs a value")?),
            "--level" => {
                level = args
                    .next()
                    .ok_or("--level needs a value")?
                    .parse()
                    .map_err(|_| "--level must be 1 or 2")?
            }
            "--recaptcha-site-key" => {
                site_key = Some(args.next().ok_or("--recaptcha-site-key needs a value")?)
            }
            "--recaptcha-secret" => {
                secret = Some(args.next().ok_or("--recaptcha-secret needs a value")?)
            }
            other => return Err(format!("unknown argument {other}").into()),
        }
    }
    let seed = seed.ok_or("--seed is required")?;
    let config = if level == 2 {
        ScenarioConfig::level_two(
            seed,
            site_key.ok_or("--level 2 requires --recaptcha-site-key")?,
            secret.ok_or("--level 2 requires --recaptcha-secret")?,
        )?
    } else {
        ScenarioConfig::seeded(seed)
    };
    let server = ScenarioServer::start(config).await?;
    println!("{}", server.application_url("/onboarding"));
    tokio::signal::ctrl_c().await?;
    Ok(())
}

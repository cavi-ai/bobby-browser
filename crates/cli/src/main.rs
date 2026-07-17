use anyhow::Result;
use config::AppConfig;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("info")
        .json()
        .init();

    let cmd = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "serve".to_string());

    match cmd.as_str() {
        "serve" => {
            let startup = broker::StartupCredential::from_env()?;
            broker::serve(AppConfig::default(), startup).await?
        }
        "doctor" => println!("ok"),
        other => {
            eprintln!("unknown command: {other}");
            std::process::exit(2);
        }
    }

    Ok(())
}

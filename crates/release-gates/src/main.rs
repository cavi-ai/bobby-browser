use std::{env, process};

use release_gates::{
    cli::{exit_code, failure_exit_code, parse_args, run_security, summary_lines, Command},
    SecurityGate,
};

#[tokio::main]
async fn main() {
    process::exit(run().await);
}

async fn run() -> i32 {
    let cli = match parse_args(env::args().skip(1)) {
        Ok(cli) => cli,
        Err(error) => {
            eprintln!("release-gates: {error}");
            return failure_exit_code(&error);
        }
    };
    let repo_root = match env::current_dir() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("release-gates: failed to determine repository root: {error}");
            return 2;
        }
    };

    match cli.command {
        Command::Security => match run_security(&cli, &repo_root, &SecurityGate::default()).await {
            Ok(bundle) => {
                for line in summary_lines(&bundle) {
                    println!("{line}");
                }
                exit_code(bundle.verdict)
            }
            Err(error) => {
                eprintln!("release-gates: {error}");
                failure_exit_code(&error)
            }
        },
    }
}

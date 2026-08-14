use std::{env, fs, path::PathBuf};

use clap::Parser;
use eyre::{Result, WrapErr};
use pg_whitelist_proxy::{config::Config, provider::GrafanaProvider, proxy::PgProxy};
use tracing_subscriber::fmt;

struct EyreHandler;

#[derive(Parser)]
struct Args {
    #[arg(short, long, default_value = "config.toml")]
    config: PathBuf,
}

impl eyre::EyreHandler for EyreHandler {
    fn debug(
        &self, error: &(dyn std::error::Error + 'static), f: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        std::fmt::Debug::fmt(error, f)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    eyre::set_hook(Box::new(|_| Box::new(EyreHandler))).unwrap();

    let _ = dotenvy::dotenv();
    fmt().with_env_filter("pg_whitelist_proxy=debug").init();

    let args = Args::parse();

    let config_toml = fs::read_to_string(&args.config)
        .wrap_err_with(|| format!("failed to read configuration from {}", args.config.display()))?;
    let mut config: Config =
        toml::from_str(&config_toml).wrap_err("failed to parse configuration")?;

    if config.grafana.token.is_empty() {
        config.grafana.token = env::var("GRAFANA_TOKEN").wrap_err("GRAFANA_TOKEN is not set")?;
    }

    let provider = GrafanaProvider::new(&config.grafana)?;
    PgProxy::new(provider, config)?.run().await
}

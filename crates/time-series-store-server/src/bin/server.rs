use std::path::PathBuf;

use clap::Parser;
use time_series_store_server::auth::ApiKeyInterceptor;
use time_series_store_server::config::ServerConfig;
use time_series_store_server::service::TimeSeriesStoreService;
use tonic::service::InterceptorLayer;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(name = "time-series-store-server")]
#[command(about = "Read-only gRPC server for time-series-store")]
struct Cli {
    /// Path to a TOML config file. See examples/server.toml.
    #[arg(long)]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let cli = Cli::parse();
    let cfg = ServerConfig::load(&cli.config)?;
    cfg.authentication
        .validate()
        .map_err(|m| format!("config error: {m}"))?;
    let primary_file = cfg
        .data
        .files
        .first()
        .ok_or("config error: [data] files = [] must contain at least one path")?;

    let service = TimeSeriesStoreService::from_path(primary_file)?;
    let addr = format!("{}:{}", cfg.server.host, cfg.server.port).parse()?;
    tracing::info!(
        "serving {} on {} (auth: {})",
        primary_file.display(),
        addr,
        cfg.authentication.method,
    );

    let mut builder = tonic::transport::Server::builder();
    match cfg.authentication.method.as_str() {
        "none" => {
            builder
                .add_service(service.into_server())
                .serve(addr)
                .await?;
        }
        "api_key" => {
            let interceptor = ApiKeyInterceptor::new(cfg.authentication.keys.clone());
            builder
                .layer(InterceptorLayer::new(interceptor))
                .add_service(service.into_server())
                .serve(addr)
                .await?;
        }
        _ => unreachable!("validate() rejected unknown methods"),
    }
    Ok(())
}

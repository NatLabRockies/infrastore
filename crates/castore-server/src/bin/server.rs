use std::path::PathBuf;

use castore_server::auth::ApiKeyInterceptor;
use castore_server::config::ServerConfig;
use castore_server::service::CatalogStoreService;
use clap::Parser;
use tonic::service::InterceptorLayer;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(name = "castore-server")]
#[command(about = "Read-only gRPC server for castore")]
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

    let service = CatalogStoreService::from_path(primary_file)?;
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

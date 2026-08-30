use std::path::PathBuf;

use clap::Parser;
use infrastore_server::auth::ApiKeyInterceptor;
use infrastore_server::config::ServerConfig;
use infrastore_server::service::CatalogStoreService;
use tonic::service::InterceptorLayer;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(name = "infrastore-server")]
#[command(about = "Read-only gRPC server for infrastore")]
struct Cli {
    /// Path to a TOML config file. See examples/server.toml.
    #[arg(long)]
    config: PathBuf,
}

#[tokio::main]
async fn main() {
    // Returning the error from `main` would print its `Debug` form, which for a
    // startup failure is the difference between an operator reading
    // `MismatchedArtifact { h5: "..", sqlite: ".." }` and reading the sentence
    // that explains what to do about it.
    if let Err(e) = run().await {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
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

    let service =
        CatalogStoreService::from_path(primary_file)?.with_max_read_ids(cfg.server.max_read_ids);
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

//! The Gatel command line.
//!
//! Exit codes are a contract, because scripts read them:
//! 0 success, 1 the configuration was rejected, 2 the command was used wrongly,
//! 3 the gateway could not run.

#![forbid(unsafe_code)]

mod app;
mod explain;
mod trust;
mod watch;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use gatel_plan::Outcome;

use crate::app::App;

const CONFIG_REJECTED: u8 = 1;
// Exit code 2 is clap's, for a command used wrongly.
const RUNTIME_FAILURE: u8 = 3;

#[derive(Parser)]
#[command(
    name = "gatel",
    version,
    about = "A programmable gateway with transactional reloads"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Serve traffic, reloading when the configuration changes.
    Run {
        #[arg(default_value = "gatel.kdl")]
        config: PathBuf,
        /// Compile and bind, then exit without serving. For deployment checks.
        #[arg(long)]
        dry_run: bool,
    },
    /// Check a configuration without binding anything.
    Validate {
        #[arg(default_value = "gatel.kdl")]
        config: PathBuf,
    },
    /// Print what a configuration compiles to, in execution order.
    Explain {
        #[arg(default_value = "gatel.kdl")]
        config: PathBuf,
    },
    /// Show what changing from one configuration to another would do.
    Diff { before: PathBuf, after: PathBuf },
    /// List the directives this build contains.
    Capabilities,
    /// Trust the development certificate authority on this machine.
    ///
    /// Only needed for `tls internal`. It changes what every program here
    /// believes, which is why it is a separate step.
    Trust {
        /// Where the authority lives, if not the default location.
        #[arg(long)]
        directory: Option<PathBuf>,
    },
    /// Stop trusting the development certificate authority.
    Untrust {
        #[arg(long)]
        directory: Option<PathBuf>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_logging();

    match cli.command {
        Command::Run { config, dry_run } => run(config, dry_run),
        Command::Validate { config } => validate(config),
        Command::Explain { config } => explain(config),
        Command::Diff { before, after } => diff(before, after),
        Command::Capabilities => capabilities(),
        Command::Trust { directory } => trust(directory, true),
        Command::Untrust { directory } => trust(directory, false),
    }
}

fn trust(directory: Option<PathBuf>, install: bool) -> ExitCode {
    let outcome = if install {
        trust::install(directory.as_deref())
    } else {
        trust::remove(directory.as_deref())
    };
    match outcome {
        Ok(trust::Outcome::Installed { store, path }) => {
            println!("installed {path} into {store}");
            println!("browsers already running may need a restart to notice");
            ExitCode::SUCCESS
        }
        Ok(trust::Outcome::Removed { store }) => {
            println!("removed the development authority from {store}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("gatel: {error}");
            ExitCode::from(RUNTIME_FAILURE)
        }
    }
}

fn init_logging() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_env("GATEL_LOG").unwrap_or_else(|_| EnvFilter::new("gatel=info"));
    let json = std::env::var("GATEL_LOG_FORMAT")
        .map(|value| value == "json")
        .unwrap_or(false);
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr);
    // Diagnostics go to stdout so `gatel validate > report` works; the running
    // log goes to stderr so the two never interleave.
    if json {
        builder.json().init();
    } else {
        builder.init();
    }
}

fn build(config: PathBuf) -> Result<App, ExitCode> {
    App::new(config).map_err(|error| {
        eprintln!("gatel: {error}");
        ExitCode::from(RUNTIME_FAILURE)
    })
}

fn validate(config: PathBuf) -> ExitCode {
    let app = match build(config) {
        Ok(app) => app,
        Err(code) => return code,
    };
    match app.compile() {
        Ok(plan) => {
            println!(
                "ok: {} listener(s), {} route(s), {} upstream(s)",
                plan.listeners.len(),
                plan.route_count(),
                plan.upstreams.len()
            );
            ExitCode::SUCCESS
        }
        Err(diagnostics) => {
            println!("{diagnostics}");
            println!("\n{} problem(s)", diagnostics.error_count());
            ExitCode::from(CONFIG_REJECTED)
        }
    }
}

fn explain(config: PathBuf) -> ExitCode {
    let app = match build(config) {
        Ok(app) => app,
        Err(code) => return code,
    };
    match app.compile() {
        Ok(plan) => {
            print!("{}", explain::explain(&plan));
            ExitCode::SUCCESS
        }
        Err(diagnostics) => {
            println!("{diagnostics}");
            ExitCode::from(CONFIG_REJECTED)
        }
    }
}

fn diff(before: PathBuf, after: PathBuf) -> ExitCode {
    let compile = |path: PathBuf| -> Result<Arc<gatel_plan::Plan>, ExitCode> {
        let app = build(path.clone())?;
        app.compile().map_err(|diagnostics| {
            println!("{}:\n{diagnostics}", path.display());
            ExitCode::from(CONFIG_REJECTED)
        })
    };
    let before = match compile(before) {
        Ok(plan) => plan,
        Err(code) => return code,
    };
    let after = match compile(after) {
        Ok(plan) => plan,
        Err(code) => return code,
    };
    print!("{}", explain::diff(&before, &after));
    ExitCode::SUCCESS
}

fn capabilities() -> ExitCode {
    let registry = match gatel_plan::Registry::builder()
        .bundle(gatel_bundle_std::bundle())
        .build()
    {
        Ok(registry) => registry,
        Err(error) => {
            eprintln!("gatel: {error}");
            return ExitCode::from(RUNTIME_FAILURE);
        }
    };
    println!("build {}", registry.build_hash().short());
    println!("\nbundles");
    for (name, version) in registry.bundles() {
        println!("  {name} {version}");
    }
    println!("\ndirectives");
    for name in registry.names() {
        let meta = registry.get(name).expect("just listed").meta();
        let role = match meta.role.phase() {
            Some(phase) => format!("middleware/{phase}"),
            None => "terminal".to_string(),
        };
        println!("  {name:<14} {role:<20} {} scope", meta.scopes.describe());
    }
    // The proxy terminal is part of the kernel rather than a bundle, and would
    // otherwise be invisible here.
    println!("  {:<14} {:<20} route scope (kernel)", "proxy", "terminal");
    ExitCode::SUCCESS
}

fn run(config: PathBuf, dry_run: bool) -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("gatel: cannot start the async runtime: {error}");
            return ExitCode::from(RUNTIME_FAILURE);
        }
    };
    runtime.block_on(async move {
        let app = match build(config) {
            Ok(app) => Arc::new(app),
            Err(code) => return code,
        };

        tracing::debug!(
            target: "gatel::process",
            event = "process_started",
            boot_id = %app.store.boot_id(),
            config = %app.root.display(),
        );

        match app.attempt() {
            Outcome::Started { generation } => {
                app.activate().await;
                for address in app.server.active_addresses().await {
                    tracing::info!(target: "gatel::listener", event = "serving", %address);
                }
                tracing::info!(
                    target: "gatel::config",
                    event = "config_committed",
                    generation = generation.number(),
                );
            }
            Outcome::Rejected(diagnostics) => {
                println!("{diagnostics}");
                eprintln!("gatel: the configuration was rejected, nothing is serving");
                return ExitCode::from(CONFIG_REJECTED);
            }
            other => {
                eprintln!("gatel: could not start: {other:?}");
                return ExitCode::from(RUNTIME_FAILURE);
            }
        }

        if dry_run {
            app.shutdown().await;
            println!("ok: bound and ready, exiting because of --dry-run");
            return ExitCode::SUCCESS;
        }

        watch::reload_on_change(Arc::clone(&app)).await;

        tracing::info!(target: "gatel::process", event = "shutdown_started");
        app.shutdown().await;
        tracing::info!(target: "gatel::process", event = "shutdown_complete");
        ExitCode::SUCCESS
    })
}

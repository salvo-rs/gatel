//! The Gatel command line.
//!
//! Exit codes are a contract, because scripts read them:
//! 0 success, 1 configuration rejected or recovery needs coordination, 2 misuse,
//! 3 the gateway could not run.

#![forbid(unsafe_code)]

#[cfg(all(target_os = "linux", target_env = "musl"))]
#[global_allocator]
static GLOBAL_ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[cfg(feature = "admin-api")]
mod admin;
mod app;
#[cfg(feature = "admin-api")]
mod checkpoint_run;
mod cli_output;
mod explain;
mod file_sources;
mod passwd;
mod plugins;
mod precompress;
mod probe;
mod serve;
mod trust;
mod watch;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::{CommandFactory, Parser, Subcommand};
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
    /// Inspect existing recovery storage without applying or repairing it.
    #[cfg(feature = "admin-api")]
    JournalInspect(admin::journal_cli::Options),
    /// Export a fully covered checkpoint into a new directory with compacted history.
    #[cfg(feature = "admin-api")]
    JournalCompact(admin::journal_cli::compact::Options),
    /// Format one KDL file to stdout, preserving comments and import statements.
    Fmt {
        #[arg(default_value = "gatel.kdl")]
        config: PathBuf,
        /// Exit 1 if formatting differs; emit no formatted text and never write files.
        #[arg(long)]
        check: bool,
    },
    /// Pre-compress static assets into atomic .gz/.br/.zst sidecars.
    Precompress(precompress::Options),
    /// Serve traffic, reloading when the configuration changes.
    Run {
        #[cfg(feature = "admin-api")]
        #[command(flatten)]
        admin: admin::cli::Options,
        #[cfg(feature = "admin-api")]
        #[command(flatten)]
        bootstrap: admin::bootstrap::Options,
        #[cfg(feature = "admin-api")]
        #[command(flatten)]
        checkpoint: checkpoint_run::Options,
        #[arg(default_value = "gatel.kdl")]
        config: PathBuf,
        /// Compile and bind, then exit without serving. For deployment checks.
        #[arg(long)]
        dry_run: bool,
        /// Additional named KDL source, repeatable; paths are relative to the working directory.
        #[arg(long = "source", value_name = "NAME=PATH")]
        sources: Vec<file_sources::NamedFile>,
    },
    /// Serve a static directory without a configuration file (development helper).
    Serve {
        #[arg(default_value = ".")]
        root: PathBuf,
        #[arg(short, long, default_value = "8080", value_parser = clap::value_parser!(u16).range(1..))]
        port: u16,
        /// IP address to listen on; explicitly opt in to external access.
        #[arg(short, long, default_value = "127.0.0.1")]
        listen: std::net::IpAddr,
        /// List directories without an index file.
        #[arg(short, long)]
        browse: bool,
        /// Compile and bind, then shut down immediately.
        #[arg(long)]
        dry_run: bool,
    },
    /// Check a configuration without binding anything.
    Validate {
        #[arg(default_value = "gatel.kdl")]
        config: PathBuf,
        #[arg(long = "source", value_name = "NAME=PATH")]
        sources: Vec<file_sources::NamedFile>,
    },
    /// Query a running local health endpoint, without reading configuration.
    Probe(probe::Options),
    /// Print what a configuration compiles to, in execution order.
    Explain {
        #[arg(default_value = "gatel.kdl")]
        config: PathBuf,
        #[arg(long = "source", value_name = "NAME=PATH")]
        sources: Vec<file_sources::NamedFile>,
    },
    /// Show what changing from one configuration to another would do.
    Diff {
        before: PathBuf,
        after: PathBuf,
        /// Additional sources in the old configuration (repeatable).
        #[arg(long = "before-source", value_name = "NAME=PATH")]
        before_sources: Vec<file_sources::NamedFile>,
        /// Additional sources in the new configuration (repeatable).
        #[arg(long = "after-source", value_name = "NAME=PATH")]
        after_sources: Vec<file_sources::NamedFile>,
    },
    /// List the directives this build contains.
    Capabilities,
    /// Generate a password hash from bounded UTF-8 stdin, never a password argument.
    Passwd {
        /// Read a password from a pipe or redirected file (not an echoing terminal).
        #[arg(long, required = true)]
        stdin: bool,
        /// Bcrypt work factor. Default 12; lower values are for tests or explicit policy.
        #[arg(short, long, value_parser = clap::value_parser!(u32).range(4..=16))]
        cost: Option<u32>,
        #[arg(long, value_enum, default_value = "bcrypt")]
        algorithm: passwd::Algorithm,
    },
    /// Write shell completions to stdout, without loading configuration.
    Completions {
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
    /// Write a roff manual to stdout, optionally for a subcommand.
    ManPage {
        /// Subcommand path, for example: run.
        command: Vec<String>,
    },
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
        #[cfg(feature = "admin-api")]
        Command::JournalInspect(options) => admin::journal_cli::run(options),
        #[cfg(feature = "admin-api")]
        Command::JournalCompact(options) => admin::journal_cli::compact::run(options),
        Command::Fmt { config, check } => cli_output::format(&config, check),
        Command::Precompress(options) => precompress::run(options),
        Command::Run {
            config,
            dry_run,
            sources,
            #[cfg(feature = "admin-api")]
            admin,
            #[cfg(feature = "admin-api")]
            bootstrap,
            #[cfg(feature = "admin-api")]
            checkpoint,
        } => {
            #[cfg(feature = "admin-api")]
            if checkpoint.checkpoint_journal.is_some() {
                return checkpoint_run::run(App::new_with_admin(config, sources), checkpoint, admin, dry_run);
            }
            run(
                config,
                dry_run,
                sources,
                #[cfg(feature = "admin-api")]
                admin,
                #[cfg(feature = "admin-api")]
                bootstrap,
            )
        }
        Command::Serve {
            root,
            port,
            listen,
            browse,
            dry_run,
        } => run_app(
            serve::app(root, std::net::SocketAddr::new(listen, port), browse),
            dry_run,
        ),
        Command::Validate { config, sources } => validate(config, sources),
        Command::Probe(options) => probe::run(options),
        Command::Explain { config, sources } => explain(config, sources),
        Command::Diff {
            before,
            after,
            before_sources,
            after_sources,
        } => diff(before, after, before_sources, after_sources),
        Command::Capabilities => capabilities(),
        Command::Passwd {
            stdin: _,
            cost,
            algorithm,
        } => passwd::run(algorithm, cost),
        Command::Completions { shell } => cli_output::completions(Cli::command(), shell),
        Command::ManPage { command } => cli_output::man_page(Cli::command(), &command),
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

fn build_sources(config: PathBuf, sources: Vec<file_sources::NamedFile>) -> Result<App, ExitCode> {
    App::new_with_files(config, sources).map_err(|error| {
        eprintln!("gatel: {error}");
        ExitCode::from(RUNTIME_FAILURE)
    })
}

fn validate(config: PathBuf, sources: Vec<file_sources::NamedFile>) -> ExitCode {
    let app = match build_sources(config, sources) {
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

fn explain(config: PathBuf, sources: Vec<file_sources::NamedFile>) -> ExitCode {
    let app = match build_sources(config, sources) {
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

fn diff(
    before: PathBuf,
    after: PathBuf,
    before_sources: Vec<file_sources::NamedFile>,
    after_sources: Vec<file_sources::NamedFile>,
) -> ExitCode {
    let compile = |path: PathBuf, sources| -> Result<Arc<gatel_plan::Plan>, ExitCode> {
        let app = build_sources(path.clone(), sources)?;
        app.compile().map_err(|diagnostics| {
            println!("{}:\n{diagnostics}", path.display());
            ExitCode::from(CONFIG_REJECTED)
        })
    };
    let before = match compile(before, before_sources) {
        Ok(plan) => plan,
        Err(code) => return code,
    };
    let after = match compile(after, after_sources) {
        Ok(plan) => plan,
        Err(code) => return code,
    };
    print!("{}", explain::diff(&before, &after));
    ExitCode::SUCCESS
}

fn capabilities() -> ExitCode {
    println!("password algorithms");
    for algorithm in gatel_password::algorithms() {
        println!("  {algorithm}");
    }
    let registry = match plugins::registry() {
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

fn run(
    config: PathBuf,
    dry_run: bool,
    sources: Vec<file_sources::NamedFile>,
    #[cfg(feature = "admin-api")] admin: admin::cli::Options,
    #[cfg(feature = "admin-api")] bootstrap: admin::bootstrap::Options,
) -> ExitCode {
    #[cfg(feature = "admin-api")]
    if admin.admin_listen.is_some() || bootstrap.bootstrap_journal.is_some() {
        return run_app_inner(
            App::new_with_admin(config, sources),
            dry_run,
            Some(admin),
            Some(bootstrap),
        );
    }
    run_app(App::new_with_files(config, sources), dry_run)
}

fn run_app(app: Result<App, String>, dry_run: bool) -> ExitCode {
    run_app_inner(
        app,
        dry_run,
        #[cfg(feature = "admin-api")]
        None,
        #[cfg(feature = "admin-api")]
        None,
    )
}

fn run_app_inner(
    app: Result<App, String>,
    dry_run: bool,
    #[cfg(feature = "admin-api")] admin: Option<admin::cli::Options>,
    #[cfg(feature = "admin-api")] bootstrap: Option<admin::bootstrap::Options>,
) -> ExitCode {
    let app = match app {
        Ok(app) => Arc::new(app),
        Err(error) => {
            eprintln!("gatel: {error}");
            return ExitCode::from(RUNTIME_FAILURE);
        }
    };
    #[cfg(feature = "admin-api")]
    let mut bootstrap = match bootstrap.map(|options| options.load(&app.registry)).transpose() {
        Ok(loaded) => loaded.flatten(),
        Err(error) => {
            eprintln!("gatel: {error}");
            return ExitCode::from(RUNTIME_FAILURE);
        }
    };
    #[cfg(feature = "admin-api")]
    let admin = match admin.map(|options| options.prepare()).transpose() {
        Ok(prepared) => prepared.flatten(),
        Err(error) => {
            eprintln!("gatel: {error}");
            return ExitCode::from(RUNTIME_FAILURE);
        }
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("gatel: cannot start the async runtime: {error}");
            return ExitCode::from(RUNTIME_FAILURE);
        }
    };
    runtime.block_on(async move {
        tracing::debug!(
            target: "gatel::process",
            event = "process_started",
            boot_id = %app.configuration_status().boot_id,
            config = %app.root.display(),
        );

        #[cfg(feature = "admin-api")]
        let startup = if let Some(bootstrap) = &mut bootstrap {
            tracing::warn!(target: "gatel::config", event = "checkpoint_bootstrap", "read-only checkpoint bootstrap; subsequent reloads are not persisted");
            match bootstrap.restore(&app).await {
                Ok(outcome) => outcome,
                Err(error) => { eprintln!("gatel: {error}"); app.shutdown().await; return ExitCode::from(RUNTIME_FAILURE); }
            }
        } else { app.reload().await };
        #[cfg(not(feature = "admin-api"))]
        let startup = app.reload().await;
        match startup {
            Outcome::Started { generation } => {
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
                app.shutdown().await;
                return ExitCode::from(CONFIG_REJECTED);
            }
            other => {
                eprintln!("gatel: could not start: {other:?}");
                app.shutdown().await;
                return ExitCode::from(RUNTIME_FAILURE);
            }
        }

        #[cfg(feature = "admin-api")]
        let mut admin = match admin.map(|prepared| prepared.start(&app)).transpose() {
            Ok(admin) => admin,
            Err(error) => {
                eprintln!("gatel: {error}");
                app.shutdown().await;
                return ExitCode::from(RUNTIME_FAILURE);
            }
        };
        if dry_run {
            #[cfg(feature = "admin-api")]
            if let Some(admin) = admin.take()
                && let Err(error) = admin.shutdown().await
            {
                eprintln!("gatel: {error}");
                app.shutdown().await;
                return ExitCode::from(RUNTIME_FAILURE);
            }
            app.shutdown().await;
            println!("ok: bound and ready, exiting because of --dry-run");
            return ExitCode::SUCCESS;
        }

        let wait = async {
            if app.watches_configuration() {
                watch::reload_on_change(Arc::clone(&app)).await;
            } else {
                watch::shutdown_signal().await;
            }
        };
        #[cfg(feature = "admin-api")]
        let mut failed = tokio::select! {
            _ = wait => false,
            result = async {
                match admin.as_mut() {
                    Some(server) => server.terminated().await,
                    None => std::future::pending().await,
                }
            } => { eprintln!("gatel: admin listener stopped unexpectedly: {result:?}"); true }
        };
        #[cfg(not(feature = "admin-api"))]
        wait.await;

        tracing::info!(target: "gatel::process", event = "shutdown_started");
        #[cfg(feature = "admin-api")]
        if let Some(admin) = admin
            && let Err(error) = admin.shutdown().await
        {
            eprintln!("gatel: {error}");
            failed = true;
        }
        app.shutdown().await;
        tracing::info!(target: "gatel::process", event = "shutdown_complete");
        #[cfg(feature = "admin-api")]
        if failed {
            return ExitCode::from(RUNTIME_FAILURE);
        }
        ExitCode::SUCCESS
    })
}

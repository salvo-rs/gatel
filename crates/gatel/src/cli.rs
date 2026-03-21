use clap::{CommandFactory, Parser, Subcommand};

#[derive(Parser)]
#[command(name = "gatel", about = "A Caddy-like reverse proxy in Rust", version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Run the proxy server
    Run {
        /// Path to the KDL configuration file
        #[arg(short, long, default_value = "gatel.kdl")]
        config: String,
    },
    /// Validate a configuration file without starting the server
    Validate {
        /// Path to the KDL configuration file
        #[arg(short, long, default_value = "gatel.kdl")]
        config: String,
    },
    /// Reload the running server's configuration (sends SIGHUP)
    Reload,
    /// Quickly serve static files (development helper)
    Serve {
        /// Directory to serve
        #[arg(default_value = ".")]
        root: String,
        /// Listen port
        #[arg(short, long, default_value = "8080")]
        port: u16,
        /// Listen address
        #[arg(short, long, default_value = "0.0.0.0")]
        listen: String,
        /// Enable directory browsing
        #[arg(short, long)]
        browse: bool,
    },
    /// Generate shell completions
    Completions {
        /// Shell to generate completions for
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
    /// Generate man page
    ManPage,
}

/// Print shell completions to stdout.
pub fn generate_completions(shell: clap_complete::Shell) {
    clap_complete::generate(shell, &mut Cli::command(), "gatel", &mut std::io::stdout());
}

/// Print man page to stdout.
pub fn generate_man_page() -> std::io::Result<()> {
    let cmd = Cli::command();
    let man = clap_mangen::Man::new(cmd);
    man.render(&mut std::io::stdout())
}

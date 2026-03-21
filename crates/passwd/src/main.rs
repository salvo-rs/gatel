use clap::Parser;

#[derive(Parser)]
#[command(
    name = "gatel-passwd",
    about = "Generate password hashes for gatel basic-auth"
)]
struct Cli {
    /// Bcrypt cost factor (default: 12)
    #[arg(short, long, default_value = "12")]
    cost: u32,
    /// Password to hash (if not given, read from stdin)
    password: Option<String>,
}

fn main() {
    let cli = Cli::parse();
    let password = match cli.password {
        Some(p) => p,
        None => {
            eprint!("Enter password: ");
            let mut input = String::new();
            std::io::stdin()
                .read_line(&mut input)
                .expect("failed to read password");
            input.trim().to_string()
        }
    };

    let hash = bcrypt::hash(&password, cli.cost).expect("failed to hash password");
    println!("{hash}");
}

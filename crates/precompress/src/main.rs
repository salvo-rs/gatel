use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use clap::Parser;

#[derive(Parser)]
#[command(
    name = "gatel-precompress",
    about = "Pre-compress static assets for gatel file-server"
)]
struct Cli {
    /// Root directory to process
    root: PathBuf,
    /// Encodings to generate (comma-separated: gzip,brotli,zstd)
    #[arg(short, long, default_value = "gzip,brotli,zstd")]
    encodings: String,
    /// Only process files with these extensions (comma-separated)
    #[arg(short, long, default_value = "html,css,js,json,xml,svg,txt,wasm")]
    types: String,
}

fn main() {
    let cli = Cli::parse();
    let encodings: Vec<&str> = cli.encodings.split(',').map(|s| s.trim()).collect();
    let extensions: Vec<&str> = cli.types.split(',').map(|s| s.trim()).collect();

    process_dir(&cli.root, &encodings, &extensions);
}

fn process_dir(dir: &Path, encodings: &[&str], extensions: &[&str]) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("cannot read {}: {e}", dir.display());
            return;
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            process_dir(&path, encodings, extensions);
        } else if let Some(ext) = path.extension().and_then(|e| e.to_str())
            && extensions.iter().any(|&e| e.eq_ignore_ascii_case(ext))
        {
            compress_file(&path, encodings);
        }
    }
}

fn compress_file(path: &Path, encodings: &[&str]) {
    let data = match fs::read(path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("cannot read {}: {e}", path.display());
            return;
        }
    };

    for &enc in encodings {
        let (out_path, compressed) = match enc {
            "gzip" | "gz" => {
                let out = PathBuf::from(format!("{}.gz", path.display()));
                let mut encoder =
                    flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
                encoder.write_all(&data).ok();
                (out, encoder.finish().unwrap_or_default())
            }
            "brotli" | "br" => {
                let out = PathBuf::from(format!("{}.br", path.display()));
                let mut compressed = Vec::new();
                let mut encoder = brotli::CompressorWriter::new(&mut compressed, 4096, 11, 22);
                encoder.write_all(&data).ok();
                drop(encoder);
                (out, compressed)
            }
            "zstd" => {
                let out = PathBuf::from(format!("{}.zst", path.display()));
                let compressed = zstd::encode_all(&data[..], 19).unwrap_or_default();
                (out, compressed)
            }
            _ => {
                eprintln!("unknown encoding: {enc}");
                continue;
            }
        };

        if let Err(e) = fs::write(&out_path, &compressed) {
            eprintln!("failed to write {}: {e}", out_path.display());
        } else {
            let ratio = if data.is_empty() {
                0.0
            } else {
                compressed.len() as f64 / data.len() as f64 * 100.0
            };
            println!(
                "{} -> {} ({:.1}%)",
                path.display(),
                out_path.display(),
                ratio
            );
        }
    }
}

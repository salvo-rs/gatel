//! Installing the development authority into the OS trust store.
//!
//! Deliberately a separate command rather than something `run` does. Adding a
//! trust anchor changes what every program on the machine believes, and that
//! should be an explicit act with a visible result — not a side effect of
//! starting a server.

use std::path::Path;
use std::process::Command;

use gatel_tls::InternalCa;

/// What happened, so the caller can print it rather than this module doing so.
#[derive(Debug)]
pub enum Outcome {
    Installed { store: &'static str, path: String },
    Removed { store: &'static str },
}

pub fn install(directory: Option<&Path>) -> Result<Outcome, String> {
    let authority = load(directory)?;
    let path = authority.root_path();
    platform_install(&path).map(|store| Outcome::Installed {
        store,
        path: path.display().to_string(),
    })
}

pub fn remove(directory: Option<&Path>) -> Result<Outcome, String> {
    let authority = load(directory)?;
    platform_remove(&authority.root_path()).map(|store| Outcome::Removed { store })
}

fn load(directory: Option<&Path>) -> Result<InternalCa, String> {
    let directory = directory
        .map(Path::to_path_buf)
        .unwrap_or_else(InternalCa::default_directory);
    if !directory.join("root.pem").exists() {
        return Err(format!(
            "no development authority at `{}`\n  \
             it is created the first time a site uses `tls internal`",
            directory.display()
        ));
    }
    InternalCa::load_or_create(&directory)
}

#[cfg(windows)]
fn platform_install(path: &Path) -> Result<&'static str, String> {
    // The current user's Root store, not the machine's: this is a development
    // authority, and installing it machine-wide would need elevation and would
    // affect other users.
    run(
        "certutil",
        &["-user", "-addstore", "Root", &path.display().to_string()],
        "the current user's Root store",
    )
}

#[cfg(windows)]
fn platform_remove(_path: &Path) -> Result<&'static str, String> {
    run(
        "certutil",
        &["-user", "-delstore", "Root", "Gatel development CA"],
        "the current user's Root store",
    )
}

#[cfg(target_os = "macos")]
fn platform_install(path: &Path) -> Result<&'static str, String> {
    run(
        "security",
        &[
            "add-trusted-cert",
            "-d",
            "-r",
            "trustRoot",
            "-k",
            "/Library/Keychains/System.keychain",
            &path.display().to_string(),
        ],
        "the system keychain",
    )
}

#[cfg(target_os = "macos")]
fn platform_remove(path: &Path) -> Result<&'static str, String> {
    run(
        "security",
        &["remove-trusted-cert", "-d", &path.display().to_string()],
        "the system keychain",
    )
}

#[cfg(all(unix, not(target_os = "macos")))]
fn platform_install(path: &Path) -> Result<&'static str, String> {
    use std::fs;

    // Two families of distribution, two directories and two update commands.
    // Neither is guaranteed present, so tell the operator rather than guessing.
    let targets: [(&str, &str, &str); 2] = [
        (
            "/usr/local/share/ca-certificates",
            "gatel-development-ca.crt",
            "update-ca-certificates",
        ),
        (
            "/etc/pki/ca-trust/source/anchors",
            "gatel-development-ca.pem",
            "update-ca-trust",
        ),
    ];
    for (directory, name, command) in targets {
        if !Path::new(directory).is_dir() {
            continue;
        }
        let destination = Path::new(directory).join(name);
        fs::copy(path, &destination).map_err(|error| {
            format!(
                "cannot write `{}`: {error}\n  this usually needs sudo",
                destination.display()
            )
        })?;
        return run(command, &[], "the system trust store");
    }
    Err(format!(
        "no known trust store directory on this system\n  \
         install `{}` with whatever your distribution uses",
        path.display()
    ))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn platform_remove(_path: &Path) -> Result<&'static str, String> {
    for (directory, name, command) in [
        (
            "/usr/local/share/ca-certificates",
            "gatel-development-ca.crt",
            "update-ca-certificates",
        ),
        (
            "/etc/pki/ca-trust/source/anchors",
            "gatel-development-ca.pem",
            "update-ca-trust",
        ),
    ] {
        let path = Path::new(directory).join(name);
        if path.exists() {
            std::fs::remove_file(&path).map_err(|error| {
                format!(
                    "cannot remove `{}`: {error}\n  this usually needs sudo",
                    path.display()
                )
            })?;
            return run(command, &[], "the system trust store");
        }
    }
    Err("the development authority is not installed".to_string())
}

fn run(program: &str, arguments: &[&str], store: &'static str) -> Result<&'static str, String> {
    let output = Command::new(program)
        .args(arguments)
        .output()
        .map_err(|error| format!("cannot run `{program}`: {error}\n  it is needed to change {store}"))?;
    if output.status.success() {
        return Ok(store);
    }
    // The tool's own message is the useful one; ours would only paraphrase it.
    let detail = String::from_utf8_lossy(&output.stderr);
    let detail = if detail.trim().is_empty() {
        String::from_utf8_lossy(&output.stdout).into_owned()
    } else {
        detail.into_owned()
    };
    Err(format!("`{program}` failed: {}", detail.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_authority_explains_how_one_appears() {
        let directory = std::env::temp_dir().join("gatel-trust-absent-does-not-exist");
        let error = install(Some(&directory)).unwrap_err();
        assert!(error.contains("no development authority"), "{error}");
        assert!(
            error.contains("tls internal"),
            "the message should say how to create one"
        );
    }

    #[test]
    fn a_missing_tool_says_what_it_was_for() {
        let error = run("gatel-no-such-tool", &[], "the system trust store").unwrap_err();
        assert!(error.contains("cannot run"), "{error}");
        assert!(error.contains("trust store"), "{error}");
    }
}

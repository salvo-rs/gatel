//! Explicit command-line opt-in. Never accepts the credential as an argument.
use std::net::{SocketAddr, TcpListener};
use std::path::PathBuf;
use std::sync::Arc;

use super::AdminApi;
use super::server::AdminServer;
use crate::app::App;

#[derive(clap::Args, Default)]
pub(crate) struct Options {
    /// Enable a loopback HTTP/1 management endpoint (memory-only by default).
    #[arg(long, requires = "admin_token_file", value_name = "IP:PORT")]
    pub admin_listen: Option<SocketAddr>,
    /// Private file containing a random bearer token; relative to the working directory.
    #[arg(long, requires = "admin_listen", value_name = "PATH")]
    admin_token_file: Option<PathBuf>,
    /// Optional distinct private bearer token granting only version/status GET.
    #[arg(long, requires = "admin_listen", value_name = "PATH")]
    admin_read_token_file: Option<PathBuf>,
}

pub(crate) struct Prepared {
    listener: TcpListener,
    token: zeroize::Zeroizing<String>,
    read_token: Option<zeroize::Zeroizing<String>>,
}

impl Options {
    pub fn prepare(self) -> Result<Option<Prepared>, String> {
        let (Some(address), Some(path)) = (self.admin_listen, self.admin_token_file) else {
            return Ok(None);
        };
        if !address.ip().is_loopback() {
            return Err("admin listen address must be loopback".into());
        }
        let token = load_token(&path)?;
        let read_token = self
            .admin_read_token_file
            .as_deref()
            .map(load_token)
            .transpose()?;
        if read_token
            .as_ref()
            .is_some_and(|read| super::digest(read) == super::digest(&token))
        {
            return Err("admin read and write tokens must differ".into());
        }
        let listener =
            TcpListener::bind(address).map_err(|error| format!("cannot bind admin listener: {error}"))?;
        Ok(Some(Prepared {
            listener,
            token,
            read_token,
        }))
    }
}

fn load_token(path: &std::path::Path) -> Result<zeroize::Zeroizing<String>, String> {
    let name = path.file_name().ok_or("admin token file needs a filename")?;
    let root = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(std::path::Path::new("."));
    let secret = gatel_model::secret::resolve_secret(&gatel_model::SecretRef::File(name.into()), root)?;
    super::validate_token(secret.expose()).map_err(str::to_owned)?;
    Ok(zeroize::Zeroizing::new(secret.expose().to_owned()))
}

impl Prepared {
    pub(crate) fn start_checkpoint(
        self,
        session: &crate::app::CheckpointSession,
    ) -> Result<AdminServer, String> {
        let api = Arc::new(
            session
                .admin_api(self.token)
                .and_then(|api| api.with_read_token(self.read_token))
                .map_err(str::to_owned)?,
        );
        let server = AdminServer::start(self.listener, api)
            .map_err(|error| format!("cannot start admin listener: {error}"))?;
        tracing::info!(target: "gatel::admin", event = "admin_serving", address = %server.address(), "checkpoint loopback administration enabled");
        Ok(server)
    }

    pub fn start(self, app: &Arc<App>) -> Result<AdminServer, String> {
        let api = Arc::new(
            AdminApi::new(app, self.token)
                .and_then(|api| api.with_read_token(self.read_token))
                .map_err(str::to_owned)?,
        );
        let server = AdminServer::start(self.listener, api)
            .map_err(|error| format!("cannot start admin listener: {error}"))?;
        tracing::info!(target: "gatel::admin", event = "admin_serving", address = %server.address(), "in-memory loopback administration enabled");
        Ok(server)
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[test]
    fn cli_requires_both_options_and_never_takes_an_inline_token() {
        assert!(crate::Cli::try_parse_from(["gatel", "run", "--admin-listen", "127.0.0.1:0"]).is_err());
        assert!(crate::Cli::try_parse_from(["gatel", "run", "--admin-token-file", "token"]).is_err());
        assert!(crate::Cli::try_parse_from(["gatel", "run", "--admin-read-token-file", "token"]).is_err());
        assert!(crate::Cli::try_parse_from(["gatel", "run", "--admin-token", "secret"]).is_err());
        assert!(
            crate::Cli::try_parse_from([
                "gatel",
                "run",
                "--admin-listen",
                "127.0.0.1:0",
                "--admin-token-file",
                "token"
            ])
            .is_ok()
        );
    }

    #[test]
    fn preparation_rejects_nonloopback_before_reading_credentials() {
        let options = Options {
            admin_listen: Some("0.0.0.0:0".parse().unwrap()),
            admin_token_file: Some("never-read.token".into()),
            admin_read_token_file: None,
        };
        assert!(matches!(options.prepare(), Err(error) if error == "admin listen address must be loopback"));
    }

    #[test]
    fn private_token_file_supports_editor_newline_but_not_invalid_secrets() {
        struct Directory(PathBuf);
        impl Drop for Directory {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let directory = Directory(
            std::env::temp_dir().join(format!("gatel-admin-token-{}", gatel_model::BootId::generate())),
        );
        std::fs::create_dir(&directory.0).unwrap();
        let path = directory.0.join("admin.token");
        let options = || Options {
            admin_listen: Some("127.0.0.1:0".parse().unwrap()),
            admin_token_file: Some(path.clone()),
            admin_read_token_file: None,
        };
        std::fs::write(&path, "0123456789abcdef0123456789abcdef\r\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(options().prepare().is_err());
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        assert!(options().prepare().unwrap().is_some());
        let mut same = options();
        same.admin_read_token_file = Some(path.clone());
        assert!(matches!(same.prepare(), Err(error) if error == "admin read and write tokens must differ"));
        let mut missing = options();
        missing.admin_read_token_file = Some(directory.0.join("missing-read-token"));
        assert!(missing.prepare().is_err());
        for bad in [
            "short".to_string(),
            " ".repeat(32),
            "x".repeat(257),
            "x".repeat(65_537),
        ] {
            std::fs::write(&path, bad).unwrap();
            assert!(options().prepare().is_err());
        }
    }
}

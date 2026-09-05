//! Secret references.
//!
//! Configuration may only ever name a secret, never contain one. The resolved
//! value lives in [`SecretString`], whose `Debug` and `Display` are redacted so
//! it cannot reach a log or a diagnostic by accident.

use std::fmt;
use std::path::{Path, PathBuf};

use crate::hash::Hasher;

/// A reference to secret material: `env:NAME` or `file:relative/path`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretRef {
    Env(String),
    File(PathBuf),
}

impl SecretRef {
    /// Parse a reference. A bare path is accepted as `file:` shorthand so
    /// certificate paths stay readable, but inline material never is.
    pub fn parse(text: &str) -> Result<Self, String> {
        if let Some(name) = text.strip_prefix("env:") {
            if name.is_empty() {
                return Err("`env:` needs a variable name".to_string());
            }
            if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                return Err(format!(
                    "`{name}` is not a valid environment variable name (A-Z, 0-9 and _)"
                ));
            }
            return Ok(SecretRef::Env(name.to_string()));
        }
        let path = text.strip_prefix("file:").unwrap_or(text);
        if path.is_empty() {
            return Err("`file:` needs a path".to_string());
        }
        if path.contains("-----BEGIN") {
            return Err("inline PEM material is not accepted, use `file:` or `env:`".to_string());
        }
        Ok(SecretRef::File(PathBuf::from(path)))
    }

    /// Resolve a `file:` reference against the configuration root, rejecting
    /// anything that escapes it.
    pub fn resolve_path(&self, root: &Path) -> Result<PathBuf, String> {
        let SecretRef::File(relative) = self else {
            return Err("not a file reference".to_string());
        };
        if relative.is_absolute() {
            return Err(format!(
                "`{}` must be relative to the configuration file",
                relative.display()
            ));
        }
        let mut resolved = root.to_path_buf();
        for component in relative.components() {
            match component {
                std::path::Component::Normal(part) => resolved.push(part),
                std::path::Component::CurDir => {}
                std::path::Component::ParentDir => {
                    return Err(format!("`{}` may not contain `..`", relative.display()));
                }
                _ => {
                    return Err(format!("`{}` is not a plain relative path", relative.display()));
                }
            }
        }
        Ok(resolved)
    }

    /// The reference itself is safe to show: it names a location, not a value.
    pub fn display(&self) -> String {
        match self {
            SecretRef::Env(name) => format!("env:{name}"),
            SecretRef::File(path) => format!("file:{}", path.display()),
        }
    }

    pub fn hash_into(&self, hasher: &mut Hasher) {
        hasher.update_str(&self.display());
    }
}

impl fmt::Display for SecretRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.display())
    }
}

/// Read the material a reference names.
///
/// The same bounds as any other secret file: it must be inside the
/// configuration directory, small, and not readable by anyone else on the
/// machine.
pub fn resolve_secret(reference: &SecretRef, root: &Path) -> Result<SecretString, String> {
    match reference {
        SecretRef::Env(name) => std::env::var(name)
            .map(SecretString::new)
            .map_err(|_| format!("the environment variable `{name}` is not set")),
        SecretRef::File(_) => {
            let path = reference.resolve_path(root)?;
            let metadata =
                std::fs::metadata(&path).map_err(|error| format!("cannot read `{reference}`: {error}"))?;
            if !metadata.is_file() {
                return Err(format!("`{reference}` is not a regular file"));
            }
            if metadata.len() > MAX_SECRET_BYTES {
                return Err(format!("`{reference}` is larger than {MAX_SECRET_BYTES} bytes"));
            }
            check_permissions(reference, &metadata)?;
            let contents = std::fs::read_to_string(&path)
                .map_err(|error| format!("cannot read `{reference}`: {error}"))?;
            // A trailing newline is what an editor adds, not part of the secret.
            Ok(SecretString::new(contents.trim_end_matches(['\r', '\n'])))
        }
    }
}

/// A secret is a password, a token or a key. Anything much larger is a mistake.
const MAX_SECRET_BYTES: u64 = 64 * 1024;

#[cfg(unix)]
fn check_permissions(reference: &SecretRef, metadata: &std::fs::Metadata) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let mode = metadata.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(format!(
            "`{reference}` is readable by group or others (mode {mode:o}); use 0600"
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_permissions(_reference: &SecretRef, _metadata: &std::fs::Metadata) -> Result<(), String> {
    // Windows ACLs do not map onto a mode, and guessing would either reject
    // reasonable setups or give false assurance.
    Ok(())
}

/// Resolved secret material. Redacted in every rendering path.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretString(String);

impl SecretString {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The only way to read the value, named so a review notices it.
    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// A stable, non-reversible tag for change detection. Deliberately not a
    /// hash of the value alone, so it cannot be used to confirm a guess.
    pub fn observation(&self, salt: &str) -> String {
        let mut hasher = Hasher::new();
        hasher.update_str("gatel.secret.observation.v1");
        hasher.update_str(salt);
        hasher.update_bytes(self.0.as_bytes());
        hasher.finish().short()
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretString(<redacted>)")
    }
}

impl fmt::Display for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_and_file_forms_parse() {
        assert_eq!(
            SecretRef::parse("env:ADMIN_HASH").unwrap(),
            SecretRef::Env("ADMIN_HASH".into())
        );
        assert_eq!(
            SecretRef::parse("file:certs/key.pem").unwrap(),
            SecretRef::File(PathBuf::from("certs/key.pem"))
        );
        assert_eq!(
            SecretRef::parse("certs/key.pem").unwrap(),
            SecretRef::File(PathBuf::from("certs/key.pem"))
        );
    }

    #[test]
    fn inline_material_and_bad_env_names_are_rejected() {
        assert!(SecretRef::parse("-----BEGIN PRIVATE KEY-----").is_err());
        assert!(SecretRef::parse("env:not a name").is_err());
        assert!(SecretRef::parse("env:").is_err());
    }

    #[test]
    fn paths_may_not_escape_the_configuration_root() {
        let root = Path::new("/etc/gatel");
        assert!(
            SecretRef::parse("../../etc/shadow")
                .unwrap()
                .resolve_path(root)
                .is_err()
        );
        assert!(
            SecretRef::parse("/etc/shadow")
                .unwrap()
                .resolve_path(root)
                .is_err()
        );
        let resolved = SecretRef::parse("certs/key.pem")
            .unwrap()
            .resolve_path(root)
            .unwrap();
        assert!(resolved.ends_with("certs/key.pem"));
        assert!(resolved.starts_with(root));
    }

    #[test]
    fn secret_values_never_render() {
        let secret = SecretString::new("hunter2");
        assert_eq!(secret.to_string(), "<redacted>");
        assert_eq!(format!("{secret:?}"), "SecretString(<redacted>)");
        assert_eq!(secret.expose(), "hunter2");
    }

    #[test]
    fn a_file_secret_is_read_without_its_trailing_newline() {
        let directory = std::env::temp_dir().join(format!(
            "gatel-secret-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let token = directory.join("token");
        std::fs::write(&token, "s3cret\n").unwrap();
        // Readable by nobody else, because that is what resolving requires —
        // and a test that wrote it any other way would only pass on a platform
        // where the requirement does nothing.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();
        }

        let resolved = resolve_secret(&SecretRef::parse("token").unwrap(), &directory).unwrap();
        assert_eq!(resolved.expose(), "s3cret");

        let missing = resolve_secret(&SecretRef::parse("absent").unwrap(), &directory).unwrap_err();
        assert!(missing.contains("absent"), "{missing}");
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn an_unset_environment_variable_says_which_one() {
        let error = resolve_secret(
            &SecretRef::parse("env:GATEL_DEFINITELY_UNSET").unwrap(),
            Path::new("."),
        )
        .unwrap_err();
        assert!(error.contains("GATEL_DEFINITELY_UNSET"), "{error}");
    }

    #[test]
    fn observations_change_with_the_value_and_the_salt() {
        let a = SecretString::new("one");
        let b = SecretString::new("two");
        assert_ne!(a.observation("tls"), b.observation("tls"));
        assert_ne!(a.observation("tls"), a.observation("admin"));
        assert_eq!(a.observation("tls"), a.observation("tls"));
    }
}

use std::path::{Component, Path, PathBuf};

use gatel_cgi_protocol::environment::Environment;
use http::{HeaderMap, Uri};

#[cfg(test)]
#[path = "mapping_resolution_tests.rs"]
mod resolution_tests;

pub struct Script {
    filename: PathBuf,
    root: PathBuf,
    pub name: String,
}

pub struct RequestInfo<'a> {
    pub uri: &'a Uri,
    pub headers: &'a HeaderMap,
    pub method: &'a str,
    pub length: u64,
    pub remote: &'a str,
    pub server: &'a str,
    pub port: u16,
    pub protocol: &'a str,
    pub tls: bool,
    pub pass_authorization: bool,
}

impl Script {
    pub fn resolve(
        base: &Path,
        root: &str,
        script: &str,
        name: &str,
        remote: Option<&str>,
    ) -> Result<Self, String> {
        Self::validate(script, name)?;
        if let Some(remote) = remote {
            Self::validate_remote(remote)?;
        }
        let root = base
            .join(root)
            .canonicalize()
            .map_err(|_| "CGI root cannot be resolved")?;
        let filename = root
            .join(script)
            .canonicalize()
            .map_err(|_| "CGI script cannot be resolved")?;
        if !root.is_dir() || !filename.is_file() || !filename.starts_with(&root) {
            return Err("CGI script must be a file within root".into());
        }
        if root.to_str().is_none() || filename.to_str().is_none() {
            return Err("CGI script paths must be UTF-8".into());
        }
        let (filename, root) = if let Some(remote) = remote {
            (
                PathBuf::from(format!("{}/{}", remote.trim_end_matches('/'), script)),
                PathBuf::from(remote),
            )
        } else {
            (filename, root)
        };
        Ok(Self {
            filename,
            root,
            name: name.into(),
        })
    }

    pub fn validate(script: &str, name: &str) -> Result<(), String> {
        if script.is_empty()
            || script.contains(['\\', ':'])
            || script.chars().any(char::is_control)
            || script.split('/').any(|part| matches!(part, "" | "." | ".."))
            || !Path::new(script)
                .components()
                .all(|part| matches!(part, Component::Normal(_)))
        {
            return Err("script must be a relative path without traversal".into());
        }
        if !name.starts_with('/')
            || name.contains(['%', '?', '#', '\\'])
            || name.chars().any(char::is_control)
            || name.split('/').any(|part| matches!(part, "." | ".."))
        {
            return Err("script-name must be an absolute URL path without escapes or traversal".into());
        }
        Ok(())
    }

    pub fn validate_remote(remote: &str) -> Result<(), String> {
        if !remote.starts_with('/')
            || remote.contains(['\\', ':'])
            || remote.chars().any(char::is_control)
            || remote.split('/').any(|part| matches!(part, "." | ".."))
        {
            return Err("remote-root must be an absolute POSIX directory without traversal".into());
        }
        Ok(())
    }

    pub fn environment(&self, info: RequestInfo<'_>) -> Result<Environment, String> {
        let RequestInfo {
            uri,
            headers,
            method,
            length,
            remote,
            server,
            port,
            protocol,
            tls,
            pass_authorization,
        } = info;
        let path = decode_path(uri.path())?;
        let info = if path == self.name {
            ""
        } else {
            path.strip_prefix(&self.name)
                .filter(|tail| tail.starts_with('/'))
                .unwrap_or(&path)
        };
        let mut env = Environment::new(64 * 1024);
        let mut set = |name, value: &str| env.insert(name, value.as_bytes()).map_err(|e| e.to_string());
        for (name, value) in [
            ("GATEWAY_INTERFACE", "CGI/1.1"),
            ("SERVER_SOFTWARE", "gatel/2"),
            ("SERVER_PROTOCOL", protocol),
            ("REQUEST_METHOD", method),
            ("SCRIPT_FILENAME", self.filename.to_str().unwrap()),
            ("DOCUMENT_ROOT", self.root.to_str().unwrap()),
            ("SCRIPT_NAME", &self.name),
            ("PATH_INFO", info),
            ("QUERY_STRING", uri.query().unwrap_or("")),
            (
                "REQUEST_URI",
                uri.path_and_query().map(|p| p.as_str()).unwrap_or("/"),
            ),
            ("REMOTE_ADDR", remote),
            ("SERVER_NAME", server),
            ("HTTPS", if tls { "on" } else { "off" }),
        ] {
            set(name, value)?;
        }
        set("CONTENT_LENGTH", &length.to_string())?;
        set("SERVER_PORT", &port.to_string())?;
        let mut nominated = Vec::new();
        for field in headers.get_all("connection") {
            for name in field
                .to_str()
                .map_err(|_| "non-text Connection header")?
                .split(',')
            {
                let name = name.trim();
                if name.is_empty() {
                    continue;
                }
                let name =
                    http::HeaderName::from_bytes(name.as_bytes()).map_err(|_| "invalid Connection token")?;
                nominated.push(name.as_str().to_owned());
            }
        }
        // CONTENT_TYPE is a CGI special variable, but still originates from a
        // request header and must obey Connection's hop-by-hop exclusions.
        if !nominated.iter().any(|name| name == "content-type") {
            let mut values = headers.get_all("content-type").iter();
            if let Some(value) = values.next() {
                if values.next().is_some() {
                    return Err("duplicate Content-Type".into());
                }
                set(
                    "CONTENT_TYPE",
                    value.to_str().map_err(|_| "invalid Content-Type")?,
                )?;
            }
        }
        for name in headers.keys() {
            let key = name.as_str();
            if key.contains('_')
                || matches!(
                    key,
                    "proxy"
                        | "content-length"
                        | "content-type"
                        | "connection"
                        | "transfer-encoding"
                        | "te"
                        | "trailer"
                        | "upgrade"
                        | "keep-alive"
                        | "proxy-connection"
                        | "proxy-authorization"
                        | "proxy-authenticate"
                )
                || nominated.iter().any(|n| n == key)
                || (!pass_authorization && key == "authorization")
            {
                continue;
            }
            let values = headers
                .get_all(name)
                .iter()
                .map(|v| v.to_str().map_err(|_| "non-text CGI request header"))
                .collect::<Result<Vec<_>, _>>()?;
            if matches!(key, "authorization" | "host") && values.len() != 1 {
                return Err("duplicate single-valued CGI request header".into());
            }
            let value = values.join(if key == "cookie" { "; " } else { ", " });
            env.insert(
                &format!("HTTP_{}", key.replace('-', "_").to_ascii_uppercase()),
                value.as_bytes(),
            )
            .map_err(|e| e.to_string())?;
        }
        Ok(env)
    }
}

fn decode_path(path: &str) -> Result<String, String> {
    let mut decoded = Vec::with_capacity(path.len());
    let mut bytes = path.as_bytes().iter().copied();
    while let Some(byte) = bytes.next() {
        let byte = if byte == b'%' {
            let hi = bytes
                .next()
                .and_then(|v| (v as char).to_digit(16))
                .ok_or("invalid URL escape")?;
            let lo = bytes
                .next()
                .and_then(|v| (v as char).to_digit(16))
                .ok_or("invalid URL escape")?;
            let value = (hi * 16 + lo) as u8;
            if matches!(value, b'/' | b'\\' | b'%') {
                return Err("encoded path separator or nested escape".into());
            }
            value
        } else {
            byte
        };
        if byte.is_ascii_control() || byte == b'\\' {
            return Err("invalid path character".into());
        }
        decoded.push(byte);
    }
    let decoded = String::from_utf8(decoded).map_err(|_| "non UTF-8 path")?;
    if decoded.split('/').any(|part| matches!(part, "." | "..")) {
        return Err("path traversal".into());
    }
    Ok(decoded)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mapped(
        fields: &[(&str, &str)],
        credentials: bool,
    ) -> Result<std::collections::BTreeMap<String, Vec<u8>>, String> {
        let script = Script {
            filename: "/srv/app".into(),
            root: "/srv".into(),
            name: "/app".into(),
        };
        let uri = "/app/info?x=%2F".parse().unwrap();
        let mut headers = HeaderMap::new();
        for (name, value) in fields {
            headers.append(
                http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                value.parse().unwrap(),
            );
        }
        let env = script.environment(RequestInfo {
            uri: &uri,
            headers: &headers,
            method: "POST",
            length: 5,
            remote: "127.0.0.1",
            server: "configured.test",
            port: 80,
            protocol: "HTTP/2.0",
            tls: false,
            pass_authorization: credentials,
        })?;
        Ok(env
            .iter()
            .map(|(name, value)| (name.to_owned(), value.to_vec()))
            .collect())
    }

    #[test]
    fn special_content_type_obeys_connection_exclusions_but_length_is_derived() {
        let env = mapped(
            &[
                ("content-type", "text/plain"),
                ("content-length", "900"),
                ("connection", "Content-Type, Content-Length, X-Internal"),
                ("x-internal", "secret"),
            ],
            false,
        )
        .unwrap();
        assert!(!env.contains_key("CONTENT_TYPE") && !env.contains_key("HTTP_X_INTERNAL"));
        assert_eq!(env["CONTENT_LENGTH"], b"5");
        assert_eq!(env["SERVER_NAME"], b"configured.test");
    }

    #[test]
    fn singleton_duplicates_are_rejected_without_exposing_values() {
        for name in ["content-type", "host", "authorization"] {
            let error = mapped(&[(name, "sensitive-first"), (name, "sensitive-second")], true).unwrap_err();
            assert!(!error.contains("sensitive"));
        }
        let env = mapped(&[("authorization", "one"), ("authorization", "two")], false).unwrap();
        assert!(!env.contains_key("HTTP_AUTHORIZATION"));
        assert!(mapped(&[("connection", "bad token")], false).is_err());
    }

    #[test]
    fn repeatable_headers_keep_order_and_cookie_uses_its_own_separator() {
        let env = mapped(
            &[
                ("cookie", "a=1"),
                ("cookie", "b=2"),
                ("x-list", "first"),
                ("x-list", "second"),
            ],
            false,
        )
        .unwrap();
        assert_eq!(env["HTTP_COOKIE"], b"a=1; b=2");
        assert_eq!(env["HTTP_X_LIST"], b"first, second");
    }
    #[test]
    fn paths_are_decoded_once_without_filesystem_traversal() {
        for path in [
            "/../secret",
            "/%2e%2e/secret",
            "/a%2fb",
            "/a%5cb",
            "/%252e",
            "/%00",
            "/%ff",
            "/%",
            "/%gg",
        ] {
            assert!(decode_path(path).is_err(), "{path}");
        }
        assert_eq!(decode_path("/caf%C3%A9").unwrap(), "/café");
        for script in [
            "../index.php",
            "/index.php",
            "C:/index.php",
            "a\\b.php",
            "",
            "a/./index.php",
            "a//index.php",
            "index.php/",
            "bad\0.php",
            "bad\n.php",
        ] {
            assert!(Script::validate(script, "/index.php").is_err());
        }
        assert!(Script::validate("dir/index.php", "/index.php").is_ok());
    }

    #[test]
    fn environment_header_collisions_and_credentials_are_controlled() {
        let script = Script {
            filename: "/srv/index.php".into(),
            root: "/srv".into(),
            name: "/index.php".into(),
        };
        let uri = "/products?order=1".parse().unwrap();
        let mut headers = HeaderMap::new();
        for (key, value) in [
            ("proxy", "bad"),
            ("authorization", "secret"),
            ("x-good", "real"),
            ("x_good", "fake"),
        ] {
            headers.insert(http::HeaderName::from_static(key), value.parse().unwrap());
        }
        let env = |pass_authorization| {
            script
                .environment(RequestInfo {
                    uri: &uri,
                    headers: &headers,
                    method: "GET",
                    length: 0,
                    remote: "127.0.0.1",
                    server: "site.test",
                    port: 8080,
                    protocol: "HTTP/1.1",
                    tls: false,
                    pass_authorization,
                })
                .unwrap()
        };
        let collect = |env: Environment| {
            env.iter()
                .map(|(k, v)| (k.to_owned(), v.to_vec()))
                .collect::<std::collections::BTreeMap<_, _>>()
        };
        let values = collect(env(false));
        assert_eq!(values["HTTP_X_GOOD"], b"real");
        assert_eq!(values["PATH_INFO"], b"/products");
        assert!(!values.contains_key("HTTP_PROXY"));
        assert!(!values.contains_key("HTTP_AUTHORIZATION"));
        assert_eq!(collect(env(true))["HTTP_AUTHORIZATION"], b"secret");
    }
}

use std::fs;

use super::*;

struct Tree(PathBuf);
impl Tree {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "gatel-cgi-script-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path.canonicalize().unwrap())
    }
}
impl Drop for Tree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn environment(script: &Script, path: &str) -> std::collections::BTreeMap<String, Vec<u8>> {
    script
        .environment(RequestInfo {
            uri: &path.parse().unwrap(),
            headers: &HeaderMap::new(),
            method: "GET",
            length: 0,
            remote: "127.0.0.1",
            server: "configured.test",
            port: 8080,
            protocol: "HTTP/1.1",
            tls: false,
            pass_authorization: false,
        })
        .unwrap()
        .iter()
        .map(|(key, value)| (key.to_owned(), value.to_vec()))
        .collect()
}

#[test]
fn explicit_hidden_script_is_fixed_independently_of_request_path() {
    let tree = Tree::new();
    fs::create_dir_all(tree.0.join("root/.app")).unwrap();
    let target = tree.0.join("root/.app/front.php");
    fs::write(&target, b"configured script").unwrap();
    for remote in [None, Some("/srv/app")] {
        let script = Script::resolve(&tree.0, "root", ".app/front.php", "/entry.php", remote).unwrap();
        let expected = match remote {
            Some(root) => format!("{root}/.app/front.php"),
            None => target.canonicalize().unwrap().to_str().unwrap().to_owned(),
        };
        for (uri, info) in [
            ("/entry.php", ""),
            ("/entry.php/orders?sort=1", "/orders"),
            ("/entry.php-other", "/entry.php-other"),
            ("/unconfigured.php", "/unconfigured.php"),
            ("/.app/another.php", "/.app/another.php"),
            ("/entry.php/caf%C3%A9", "/café"),
        ] {
            let env = environment(&script, uri);
            assert_eq!(env["SCRIPT_FILENAME"], expected.as_bytes(), "{uri}");
            assert_eq!(env["SCRIPT_NAME"], b"/entry.php");
            assert_eq!(env["PATH_INFO"], info.as_bytes(), "{uri}");
            assert_eq!(env["REQUEST_URI"], uri.as_bytes());
        }
    }
}

#[test]
fn remote_mapping_still_requires_a_local_regular_script() {
    let tree = Tree::new();
    fs::create_dir(tree.0.join("root")).unwrap();
    fs::create_dir(tree.0.join("root/directory.php")).unwrap();
    for remote in [None, Some("/srv/app")] {
        for name in ["missing.php", "directory.php", "../outside.php"] {
            assert!(Script::resolve(&tree.0, "root", name, "/entry.php", remote).is_err());
        }
    }
}

#[cfg(unix)]
#[test]
fn script_symlinks_must_remain_within_root_even_with_remote_mapping() {
    use std::os::unix::fs::symlink;
    let tree = Tree::new();
    fs::create_dir(tree.0.join("root")).unwrap();
    fs::create_dir(tree.0.join("root-other")).unwrap();
    fs::write(tree.0.join("root-other/secret.php"), b"outside").unwrap();
    fs::write(tree.0.join("root/.fixed.php"), b"explicit internal script").unwrap();
    symlink(
        tree.0.join("root-other/secret.php"),
        tree.0.join("root/escape.php"),
    )
    .unwrap();
    symlink(tree.0.join("root/.fixed.php"), tree.0.join("root/internal.php")).unwrap();
    symlink(tree.0.join("absent"), tree.0.join("root/broken.php")).unwrap();
    for remote in [None, Some("/srv/app")] {
        for name in ["escape.php", "broken.php"] {
            assert!(Script::resolve(&tree.0, "root", name, "/entry.php", remote).is_err());
        }
        let script = Script::resolve(&tree.0, "root", "internal.php", "/entry.php", remote).unwrap();
        let expected = match remote {
            Some(root) => format!("{root}/internal.php"),
            None => tree.0.join("root/.fixed.php").to_str().unwrap().to_owned(),
        };
        assert_eq!(
            environment(&script, "/different.php")["SCRIPT_FILENAME"],
            expected.as_bytes()
        );
    }
}

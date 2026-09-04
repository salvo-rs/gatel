//! Reading a configuration tree from disk.
//!
//! An `import` is expanded in place, in declaration order. Loading is atomic:
//! either every file in the tree was read and parsed, or nothing is produced.
//! A missing import is an error unless it is marked `optional=#true`, because
//! "warn and carry on" silently changes what a gateway serves.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use gatel_model::{Code, Diagnostic, Diagnostics, Fingerprint, Hasher, Node, Origin, Span};

use crate::lower::lower;

/// The result of reading one configuration tree.
#[derive(Debug)]
pub struct Loaded {
    /// Top-level nodes with every import expanded in place.
    pub nodes: Vec<Node>,
    /// Fingerprint of the raw bytes of every file read, in visit order.
    /// Identical fingerprints cannot differ semantically, which is what lets
    /// an unchanged file skip compilation entirely.
    pub content: Fingerprint,
    /// Every file that was read, so a watcher knows what to watch.
    pub files: Vec<PathBuf>,
}

/// Bounds that stop a pathological or hostile configuration tree.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_files: usize,
    pub max_bytes_per_file: u64,
    pub max_import_depth: usize,
    pub max_nodes: usize,
    pub max_depth: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_files: 256,
            max_bytes_per_file: 4 * 1024 * 1024,
            max_import_depth: 8,
            max_nodes: 100_000,
            max_depth: 32,
        }
    }
}

pub fn load(root: &Path) -> Result<Loaded, Diagnostics> {
    load_with_limits(root, Limits::default())
}

pub fn load_with_limits(root: &Path, limits: Limits) -> Result<Loaded, Diagnostics> {
    let mut context = LoadContext {
        limits,
        hasher: Hasher::new(),
        files: Vec::new(),
        visiting: BTreeSet::new(),
        diagnostics: Diagnostics::new(),
    };
    let base = root.parent().unwrap_or(Path::new(".")).to_path_buf();
    let nodes = context.load_file(root, &base, 0, None);

    if !context.diagnostics.is_empty() && context.diagnostics.has_errors() {
        return Err(std::mem::take(&mut context.diagnostics).sorted());
    }

    let total: usize = nodes.iter().map(Node::node_count).sum();
    if total > limits.max_nodes {
        let mut diagnostics = Diagnostics::new();
        diagnostics.error(
            Code::ImportFailed,
            Origin::file(display(root), Span::default()),
            format!(
                "configuration has {total} nodes, the limit is {}",
                limits.max_nodes
            ),
        );
        return Err(diagnostics);
    }
    if let Some(deep) = nodes.iter().find(|node| node.depth() > limits.max_depth) {
        let mut diagnostics = Diagnostics::new();
        diagnostics.error(
            Code::ImportFailed,
            deep.origin.clone(),
            format!("node nesting exceeds the limit of {}", limits.max_depth),
        );
        return Err(diagnostics);
    }

    Ok(Loaded {
        nodes,
        content: context.hasher.finish(),
        files: context.files,
    })
}

struct LoadContext {
    limits: Limits,
    hasher: Hasher,
    files: Vec<PathBuf>,
    visiting: BTreeSet<PathBuf>,
    diagnostics: Diagnostics,
}

impl LoadContext {
    fn load_file(
        &mut self,
        path: &Path,
        base: &Path,
        depth: usize,
        imported_from: Option<&Origin>,
    ) -> Vec<Node> {
        let key = normalize(path);
        if !self.visiting.insert(key.clone()) {
            let origin = imported_from
                .cloned()
                .unwrap_or_else(|| Origin::file(display(path), Span::default()));
            self.diagnostics.push(Diagnostic::error(
                Code::ImportFailed,
                origin,
                format!("`{}` is already being imported, which would loop", display(path)),
            ));
            return Vec::new();
        }
        if self.files.len() >= self.limits.max_files {
            self.diagnostics.error(
                Code::ImportFailed,
                imported_from
                    .cloned()
                    .unwrap_or_else(|| Origin::file(display(path), Span::default())),
                format!("more than {} configuration files", self.limits.max_files),
            );
            self.visiting.remove(&key);
            return Vec::new();
        }

        let text = match read_bounded(path, self.limits.max_bytes_per_file) {
            Ok(text) => text,
            Err(message) => {
                let origin = imported_from
                    .cloned()
                    .unwrap_or_else(|| Origin::file(display(path), Span::default()));
                self.diagnostics.push(Diagnostic::error(
                    Code::ImportFailed,
                    origin,
                    format!("cannot read `{}`: {message}", display(path)),
                ));
                self.visiting.remove(&key);
                return Vec::new();
            }
        };

        self.hasher.update_str(&display(path));
        self.hasher.update_bytes(text.as_bytes());
        self.files.push(key.clone());

        let nodes = match lower(&text, &display(path)) {
            Ok(nodes) => nodes,
            Err(diagnostics) => {
                self.diagnostics.extend(diagnostics);
                self.visiting.remove(&key);
                return Vec::new();
            }
        };

        let mut expanded = Vec::with_capacity(nodes.len());
        for node in nodes {
            if node.name == "import" {
                expanded.extend(self.expand_import(&node, base, depth));
            } else {
                expanded.push(node);
            }
        }
        self.visiting.remove(&key);
        expanded
    }

    fn expand_import(&mut self, node: &Node, base: &Path, depth: usize) -> Vec<Node> {
        if depth >= self.limits.max_import_depth {
            self.diagnostics.error(
                Code::ImportFailed,
                node.origin.clone(),
                format!("imports nested more than {} deep", self.limits.max_import_depth),
            );
            return Vec::new();
        }

        let optional = match node.prop("optional") {
            Some(property) => match property.value.as_bool() {
                Some(flag) => flag,
                None => {
                    self.diagnostics.error(
                        Code::InvalidValue,
                        property.origin.clone(),
                        "`optional` must be `#true` or `#false`",
                    );
                    return Vec::new();
                }
            },
            None => false,
        };
        for property in &node.props {
            if property.name != "optional" {
                self.diagnostics.error(
                    Code::UnknownProperty,
                    property.origin.clone(),
                    format!("`import` has no property `{}`", property.name),
                );
            }
        }
        if !node.children.is_empty() {
            self.diagnostics.error(
                Code::UnknownNode,
                node.children[0].origin.clone(),
                "`import` does not take child nodes",
            );
        }
        if node.args.len() != 1 {
            self.diagnostics.error(
                Code::ArgumentCount,
                node.origin.clone(),
                "`import` takes exactly one path",
            );
            return Vec::new();
        }
        let Some(pattern) = node.args[0].as_str() else {
            self.diagnostics.error(
                Code::InvalidValue,
                node.args[0].origin.clone(),
                "`import` takes a path string",
            );
            return Vec::new();
        };

        if Path::new(pattern).is_absolute() || pattern.contains("..") || pattern.contains('\\') {
            self.diagnostics.push(
                Diagnostic::error(
                    Code::ImportFailed,
                    node.args[0].origin.clone(),
                    format!("`{pattern}` must be a relative path without `..`"),
                )
                .with_help("imports resolve against the directory of the importing file"),
            );
            return Vec::new();
        }

        let matches = match resolve(base, pattern) {
            Ok(matches) => matches,
            Err(message) => {
                self.diagnostics
                    .error(Code::ImportFailed, node.args[0].origin.clone(), message);
                return Vec::new();
            }
        };
        if matches.is_empty() {
            if !optional {
                self.diagnostics.push(
                    Diagnostic::error(
                        Code::ImportFailed,
                        node.args[0].origin.clone(),
                        format!("`{pattern}` matched no files"),
                    )
                    .with_help("add `optional=#true` if the file is not always present"),
                );
            }
            return Vec::new();
        }

        let mut expanded = Vec::new();
        for path in matches {
            let child_base = path.parent().unwrap_or(base).to_path_buf();
            expanded.extend(self.load_file(&path, &child_base, depth + 1, Some(&node.origin)));
        }
        expanded
    }
}

fn read_bounded(path: &Path, max_bytes: u64) -> Result<String, String> {
    let metadata = std::fs::metadata(path).map_err(|error| error.to_string())?;
    if !metadata.is_file() {
        return Err("not a regular file".to_string());
    }
    if metadata.len() > max_bytes {
        return Err(format!("file is larger than {max_bytes} bytes"));
    }
    std::fs::read_to_string(path).map_err(|error| error.to_string())
}

/// Resolve an import pattern. `*` and `?` are supported in the final component
/// only; matches are returned in sorted order so a directory listing cannot
/// change the meaning of a configuration between runs.
fn resolve(base: &Path, pattern: &str) -> Result<Vec<PathBuf>, String> {
    let (directory, last) = match pattern.rsplit_once('/') {
        Some((directory, last)) => (base.join(directory), last.to_string()),
        None => (base.to_path_buf(), pattern.to_string()),
    };
    if !last.contains('*') && !last.contains('?') {
        let path = directory.join(&last);
        return Ok(if path.exists() { vec![path] } else { Vec::new() });
    }
    if directory.to_string_lossy().contains('*') {
        return Err("wildcards are only allowed in the last path component".to_string());
    }
    let entries = match std::fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(_) => return Ok(Vec::new()),
    };
    let mut matches: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_file())
        .filter(|entry| glob_match(&last, &entry.file_name().to_string_lossy()))
        .map(|entry| entry.path())
        .collect();
    matches.sort();
    Ok(matches)
}

/// `*` matches any run of characters, `?` matches exactly one.
fn glob_match(pattern: &str, name: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let name: Vec<char> = name.chars().collect();
    let (mut p, mut n) = (0usize, 0usize);
    let (mut star, mut resume) = (usize::MAX, 0usize);
    while n < name.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == name[n]) {
            p += 1;
            n += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = p;
            resume = n;
            p += 1;
        } else if star != usize::MAX {
            p = star + 1;
            resume += 1;
            n = resume;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == '*' {
        p += 1;
    }
    p == pattern.len()
}

fn normalize(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn display(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "gatel-kdl-{tag}-{}-{:?}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn write(&self, name: &str, content: &str) -> PathBuf {
            let path = self.0.join(name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&path, content).unwrap();
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn imports_expand_in_place_in_declaration_order() {
        let dir = TempDir::new("order");
        dir.write("a.kdl", "site \"a\"\n");
        dir.write("b.kdl", "site \"b\"\n");
        let root = dir.write(
            "gatel.kdl",
            "import \"a.kdl\"\nsite \"middle\"\nimport \"b.kdl\"\n",
        );
        let loaded = load(&root).unwrap();
        let hosts: Vec<&str> = loaded
            .nodes
            .iter()
            .map(|node| node.args[0].as_str().unwrap())
            .collect();
        assert_eq!(hosts, ["a", "middle", "b"]);
        assert_eq!(loaded.files.len(), 3);
    }

    #[test]
    fn a_missing_import_fails_unless_it_is_optional() {
        let dir = TempDir::new("missing");
        let root = dir.write("gatel.kdl", "import \"absent.kdl\"\n");
        let error = load(&root).unwrap_err();
        assert_eq!(error.iter().next().unwrap().code, Code::ImportFailed);

        let root = dir.write(
            "optional.kdl",
            "import \"absent.kdl\" optional=#true\nsite \"a\"\n",
        );
        let loaded = load(&root).unwrap();
        assert_eq!(loaded.nodes.len(), 1);
    }

    #[test]
    fn globs_load_in_sorted_order_and_zero_matches_is_an_error() {
        let dir = TempDir::new("glob");
        dir.write("conf.d/20-b.kdl", "site \"b\"\n");
        dir.write("conf.d/10-a.kdl", "site \"a\"\n");
        dir.write("conf.d/ignored.txt", "not kdl\n");
        let root = dir.write("gatel.kdl", "import \"conf.d/*.kdl\"\n");
        let loaded = load(&root).unwrap();
        let hosts: Vec<&str> = loaded
            .nodes
            .iter()
            .map(|node| node.args[0].as_str().unwrap())
            .collect();
        assert_eq!(hosts, ["a", "b"]);

        let root = dir.write("empty.kdl", "import \"nothing.d/*.kdl\"\n");
        assert!(load(&root).is_err());
    }

    #[test]
    fn import_cycles_are_reported_rather_than_hanging() {
        let dir = TempDir::new("cycle");
        dir.write("b.kdl", "import \"a.kdl\"\n");
        let root = dir.write("a.kdl", "import \"b.kdl\"\n");
        let error = load(&root).unwrap_err();
        assert!(error.iter().any(|d| d.message.contains("would loop")));
    }

    #[test]
    fn escaping_the_configuration_directory_is_refused() {
        let dir = TempDir::new("escape");
        let root = dir.write("gatel.kdl", "import \"../outside.kdl\"\n");
        let error = load(&root).unwrap_err();
        assert!(error.iter().any(|d| d.message.contains("without `..`")));
    }

    #[test]
    fn identical_bytes_produce_an_identical_fingerprint() {
        let dir = TempDir::new("fingerprint");
        let root = dir.write("gatel.kdl", "site \"a\"\n");
        let first = load(&root).unwrap().content;
        let second = load(&root).unwrap().content;
        assert_eq!(first, second);
        dir.write("gatel.kdl", "site \"b\"\n");
        assert_ne!(load(&root).unwrap().content, first);
    }

    #[test]
    fn a_syntax_error_in_an_imported_file_names_that_file() {
        let dir = TempDir::new("syntax");
        dir.write("broken.kdl", "site \"a\" {\n");
        let root = dir.write("gatel.kdl", "import \"broken.kdl\"\n");
        let error = load(&root).unwrap_err();
        assert!(error.iter().any(|d| d.origin.file_name() == Some("broken.kdl")));
    }

    #[test]
    fn glob_patterns_match_the_way_a_shell_would() {
        assert!(glob_match("*.kdl", "site.kdl"));
        assert!(glob_match("*.kdl", ".kdl"));
        assert!(!glob_match("*.kdl", "site.txt"));
        assert!(glob_match("site-?.kdl", "site-a.kdl"));
        assert!(!glob_match("site-?.kdl", "site-ab.kdl"));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("a*b*c", "axxbyyc"));
        assert!(!glob_match("a*b*c", "axxbyy"));
    }
}

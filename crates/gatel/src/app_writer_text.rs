//! In-memory frontend for a bound source. Never expands file imports.
use gatel_model::{Code, Diagnostics, Fingerprint, Hasher, Node, Origin, SourceId};

const MAX_BYTES: usize = 1024 * 1024;
const MAX_NODES: usize = 10_000;

fn rejected(origin: Origin, message: &str) -> Diagnostics {
    let mut diagnostics = Diagnostics::new();
    diagnostics.error(Code::InvalidValue, origin, message);
    diagnostics
}

pub(super) fn lower(input: &str, source: &SourceId) -> Result<(Fingerprint, Vec<Node>), Diagnostics> {
    let label = format!("inline:{source}");
    if input.len() > MAX_BYTES {
        return Err(rejected(
            Origin::synthetic(label),
            "inline configuration exceeds 1 MiB",
        ));
    }
    // lower() guards recursive parser depth before parsing. The byte bound also
    // bounds the material parsed before we count the resulting nodes.
    let nodes = gatel_kdl::lower(input, &label)?;
    let mut pending: Vec<_> = nodes.iter().collect();
    let mut count = 0;
    while let Some(node) = pending.pop() {
        count += 1;
        if count > MAX_NODES {
            return Err(rejected(
                node.origin.clone(),
                "inline configuration exceeds 10000 nodes",
            ));
        }
        if node.name == "import" {
            return Err(rejected(
                node.origin.clone(),
                "imports are not supported in inline configuration",
            ));
        }
        pending.extend(&node.children);
    }
    let mut hash = Hasher::new();
    hash.update_str("gatel.inline-kdl.v1");
    hash.update_str(&label);
    hash.update_str(input);
    Ok((hash.finish(), nodes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds_are_inclusive_and_imports_never_expand() {
        let source = SourceId::admin();
        assert!(lower(&" ".repeat(MAX_BYTES), &source).is_ok());
        assert!(lower(&" ".repeat(MAX_BYTES + 1), &source).is_err());
        assert!(lower(&"node;".repeat(MAX_NODES), &source).is_ok());
        assert!(lower(&"node;".repeat(MAX_NODES + 1), &source).is_err());
        for input in [
            "import \"never-read.kdl\";",
            "parent { import \"never-read.kdl\"; }",
        ] {
            let error = lower(input, &source).unwrap_err();
            assert!(format!("{error:?}").contains("imports are not supported"));
        }
        let deep = format!("{}{}", "node {".repeat(1000), "}".repeat(1000));
        assert!(lower(&deep, &source).is_err());
    }

    #[test]
    fn identity_includes_exact_text_and_stable_source_label() {
        let source = SourceId::admin();
        let (first, nodes) = lower("node;", &source).unwrap();
        assert_eq!(first, lower("node;", &source).unwrap().0);
        assert_ne!(first, lower("node; ", &source).unwrap().0);
        assert_ne!(first, lower("node;", &SourceId::file("main")).unwrap().0);
        assert_eq!(nodes[0].origin, lower("node;", &source).unwrap().1[0].origin);
    }
}

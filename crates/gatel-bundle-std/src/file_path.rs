//! Public filename policy shared by URL resolution and directory browsing.
/// Both paths must already be canonical. Apply the public-name policy below
/// the configured root, not to the root itself or its ancestors.
pub(crate) fn public_resolved(root: &std::path::Path, resolved: &std::path::Path) -> bool {
    let Ok(relative) = resolved.strip_prefix(root) else {
        return false;
    };
    relative.components().all(|component| match component {
        std::path::Component::Normal(name) => name.to_str().is_some_and(public_component),
        _ => false,
    })
}

pub(crate) fn public_component(name: &str) -> bool {
    if name.is_empty() || name.starts_with('.') || name.chars().any(char::is_control) {
        return false;
    }
    #[cfg(windows)]
    if !windows_component(name) {
        return false;
    }
    true
}

// Reject alternate streams, device aliases and names normalized by Win32.
// The helper is tested on every platform even though only Windows applies it.
#[cfg(any(windows, test))]
fn windows_component(name: &str) -> bool {
    if name.contains([':', '<', '>', '"', '/', '\\', '|', '?', '*']) || name.ends_with(['.', ' ']) {
        return false;
    }
    let stem = name
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end()
        .to_ascii_uppercase();
    if matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) {
        return false;
    }
    for prefix in ["COM", "LPT"] {
        if let Some(suffix) = stem.strip_prefix(prefix)
            && matches!(
                suffix,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn resolved_policy_is_scoped_below_the_configured_root() {
        let root = std::path::Path::new(".deployment").join("www");
        assert!(public_resolved(&root, &root));
        assert!(public_resolved(&root, &root.join("assets/page.txt")));
        for path in [".env", ".private/page.txt", "assets/.secret", "bad\nname.txt"] {
            assert!(!public_resolved(&root, &root.join(path)), "{path}");
        }
        assert!(!public_resolved(
            &root,
            &root.with_file_name("www-other").join("page.txt")
        ));
    }
    #[test]
    fn windows_aliases_are_not_public_filenames() {
        for name in [
            "page.txt:secret",
            "page.txt::$DATA",
            "NUL",
            "con.txt",
            "CON .txt",
            "LPT1.log",
            "COM³.txt",
            "CONOUT$",
            "trailing.",
            "trailing ",
        ] {
            assert!(!windows_component(name), "{name}");
        }
        for name in ["normal.txt", "COM10.txt", "company.txt", "a #&%.txt", "中文.txt"] {
            assert!(windows_component(name), "{name}");
        }
    }
    #[test]
    fn hidden_and_control_names_are_not_advertised_or_resolved() {
        for name in ["", ".env", ".git", "a\n.txt", "a\0"] {
            assert!(!public_component(name));
        }
        assert!(public_component("safe.txt"));
    }
}

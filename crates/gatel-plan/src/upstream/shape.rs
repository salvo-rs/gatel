//! Structural checks shared by all upstream spellings and model producers.
use std::collections::BTreeMap;

use gatel_model::{Code, Diagnostic, Diagnostics, Node};

pub(super) fn validate(node: &Node, diagnostics: &mut Diagnostics) -> bool {
    let before = diagnostics.error_count();
    for property in &node.props {
        diagnostics.error(
            Code::UnknownProperty,
            property.origin.clone(),
            "upstream options must be child nodes, not properties",
        );
    }
    let mut seen = BTreeMap::new();
    for child in &node.children {
        if child.name == "endpoint" {
            continue;
        }
        if let Some(first) = seen.insert(child.name.as_str(), &child.origin) {
            diagnostics.push(
                Diagnostic::error(
                    Code::Duplicate,
                    child.origin.clone(),
                    format!("upstream `{}` is declared twice", child.name),
                )
                .with_related("first declared here", first.clone()),
            );
        }
        // Derived FromNode decoders validate their own shapes. These three
        // policies use manual decoders and must not silently ignore input.
        let args = match child.name.as_str() {
            "balance" | "protocol" => 1,
            "tls" => 0,
            _ => continue,
        };
        if child.args.len() != args {
            diagnostics.error(
                Code::ArgumentCount,
                child.origin.clone(),
                format!("upstream `{}` takes exactly {args} arguments", child.name),
            );
        }
        for nested in &child.children {
            diagnostics.error(
                Code::UnknownNode,
                nested.origin.clone(),
                format!("upstream `{}` takes no child nodes", child.name),
            );
        }
        let mut props = BTreeMap::new();
        for property in &child.props {
            if let Some(first) = props.insert(property.name.as_str(), &property.origin) {
                diagnostics.push(
                    Diagnostic::error(
                        Code::Duplicate,
                        property.origin.clone(),
                        "upstream policy property is declared twice",
                    )
                    .with_related("first declared here", first.clone()),
                );
            }
            if child.name == "protocol" {
                diagnostics.error(
                    Code::UnknownProperty,
                    property.origin.clone(),
                    "upstream protocol takes no properties",
                );
            }
            if child.name == "tls" && property.name == "server-name" && property.value.as_str().is_none() {
                diagnostics.error(
                    Code::InvalidValue,
                    property.origin.clone(),
                    "upstream TLS server-name must be a string",
                );
            }
        }
    }
    diagnostics.error_count() == before
}

//! Template placeholder expansion.

use std::collections::HashMap;

/// Replace all `{key}` placeholders in `template` with values from `values`.
pub fn expand_placeholders(template: &str, values: &HashMap<&str, String>) -> String {
    let mut result = template.to_string();
    for (key, value) in values {
        result = result.replace(&format!("{{{key}}}"), value);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_replacement() {
        let mut values = HashMap::new();
        values.insert("name", "Alice".to_string());
        let result = expand_placeholders("Hello {name}", &values);
        assert_eq!(result, "Hello Alice");
    }
}

//! Glob pattern matching.

/// Match a string against a glob pattern.
pub fn glob_matches(pattern: &str, text: &str) -> bool {
    glob_matches_impl(pattern.as_bytes(), text.as_bytes())
}

fn glob_matches_impl(pattern: &[u8], text: &[u8]) -> bool {
    let mut pattern_index = 0;
    let mut text_index = 0;
    let mut star_pattern_index = usize::MAX;
    let mut star_text_index = 0;

    while text_index < text.len() {
        if pattern_index < pattern.len() {
            if pattern_index + 1 < pattern.len()
                && pattern[pattern_index] == b'*'
                && pattern[pattern_index + 1] == b'*'
            {
                let rest = &pattern[pattern_index + 2..];
                if rest.is_empty() {
                    return true;
                }
                let rest = if rest.first() == Some(&b'/') {
                    &rest[1..]
                } else {
                    rest
                };
                for start in text_index..=text.len() {
                    let sub_text = &text[start..];
                    if glob_matches_impl(rest, sub_text) {
                        return true;
                    }
                    if !sub_text.is_empty()
                        && sub_text[0] == b'/'
                        && glob_matches_impl(rest, &sub_text[1..])
                    {
                        return true;
                    }
                }
                return false;
            }

            if pattern[pattern_index] == b'?' {
                pattern_index += 1;
                text_index += 1;
                continue;
            }

            if pattern[pattern_index] == b'*' {
                star_pattern_index = pattern_index;
                star_text_index = text_index;
                pattern_index += 1;
                continue;
            }

            if pattern[pattern_index] == text[text_index] {
                pattern_index += 1;
                text_index += 1;
                continue;
            }
        }

        if star_pattern_index != usize::MAX {
            pattern_index = star_pattern_index + 1;
            star_text_index += 1;
            text_index = star_text_index;
            if text_index <= text.len() && text_index > 0 && text[text_index - 1] == b'/' {
                return false;
            }
            continue;
        }

        return false;
    }

    while pattern_index < pattern.len() && pattern[pattern_index] == b'*' {
        pattern_index += 1;
    }

    pattern_index == pattern.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_star() {
        assert!(glob_matches("foo*", "foobar"));
        assert!(glob_matches("*bar", "foobar"));
        assert!(!glob_matches("foo*", "foo/bar"));
    }

    #[test]
    fn matches_double_star() {
        assert!(glob_matches("/api/**", "/api/v1/users"));
        assert!(glob_matches("foo/**/bar", "foo/a/b/c/bar"));
    }

    #[test]
    fn matches_question() {
        assert!(glob_matches("fo?", "foo"));
        assert!(!glob_matches("fo?", "fooo"));
    }
}

/// SQL execution guards: auto-LIMIT and read-only statement detection.

/// Checks if the SQL statement is a read-only query.
///
/// Returns true if the first significant keyword (after stripping whitespace and comments)
/// is one of: SELECT, WITH, EXPLAIN, SHOW, VALUES, PRAGMA.
pub fn is_read_statement(sql: &str) -> bool {
    let first_token = first_significant_token(sql);
    match first_token.to_uppercase().as_str() {
        "SELECT" | "WITH" | "EXPLAIN" | "SHOW" | "VALUES" | "PRAGMA" => true,
        _ => false,
    }
}

/// Applies an automatic LIMIT clause to SELECT statements if safe.
///
/// Returns a tuple of (possibly rewritten SQL, whether it changed).
///
/// This is a heuristic that:
/// - Only applies to statements starting with SELECT (not WITH)
/// - Does not apply if the statement already contains a top-level LIMIT, OFFSET, FETCH, or INTO token
/// - Does not apply if the statement ends in an open string literal or comment
/// - Appends " LIMIT {n}" before any trailing semicolon
pub fn apply_auto_limit(sql: &str, limit: u64) -> (String, bool) {
    let first_token = first_significant_token(sql);

    // Only apply to SELECT statements (not WITH)
    if first_token.to_uppercase() != "SELECT" {
        return (sql.to_string(), false);
    }

    // Check if statement already has a limiting clause
    if has_limiting_clause(sql) {
        return (sql.to_string(), false);
    }

    // Check if statement ends in an open string or comment
    if ends_in_open_construct(sql) {
        return (sql.to_string(), false);
    }

    // Apply the LIMIT clause
    let trimmed = sql.trim_end();
    let limit_str = format!(" LIMIT {}", limit);

    let result = if trimmed.ends_with(';') {
        let without_semicolon = &trimmed[..trimmed.len() - 1];
        format!("{}{};", without_semicolon, limit_str)
    } else {
        format!("{}{}", trimmed, limit_str)
    };

    (result, true)
}

/// Find the first significant token, skipping whitespace and comments.
fn first_significant_token(sql: &str) -> String {
    let mut chars = sql.chars().peekable();
    let mut in_line_comment = false;
    let mut in_block_comment = false;

    loop {
        match chars.peek() {
            None => return String::new(),
            Some(&c) => {
                // Handle line comment end
                if in_line_comment && c == '\n' {
                    in_line_comment = false;
                    chars.next();
                    continue;
                }

                if in_line_comment {
                    chars.next();
                    continue;
                }

                // Handle block comment
                if in_block_comment {
                    if c == '*' {
                        chars.next();
                        if chars.peek() == Some(&'/') {
                            chars.next();
                            in_block_comment = false;
                        }
                    } else {
                        chars.next();
                    }
                    continue;
                }

                // Check for comment start
                if c == '-' {
                    chars.next();
                    if chars.peek() == Some(&'-') {
                        chars.next();
                        in_line_comment = true;
                        continue;
                    }
                    // Not a comment, put the dash back (conceptually)
                    // Actually we already consumed it, so this is an error case
                    // But in practice, a single dash isn't a valid token starter
                    continue;
                }

                if c == '/' {
                    chars.next();
                    if chars.peek() == Some(&'*') {
                        chars.next();
                        in_block_comment = true;
                        continue;
                    }
                    // Not a comment start
                    continue;
                }

                // Skip whitespace
                if c.is_whitespace() {
                    chars.next();
                    continue;
                }

                // Found start of a token
                let mut token = String::new();
                while let Some(&ch) = chars.peek() {
                    if ch.is_alphanumeric() || ch == '_' {
                        token.push(ch);
                        chars.next();
                    } else {
                        break;
                    }
                }

                if !token.is_empty() {
                    return token;
                }

                // Skip non-alphanumeric character
                chars.next();
            }
        }
    }
}

/// Check if statement already has a limiting clause.
fn has_limiting_clause(sql: &str) -> bool {
    let mut chars = sql.chars().peekable();
    let mut in_single_string = false;
    let mut in_double_ident = false;
    let mut in_line_comment = false;
    let mut in_block_comment = false;

    while let Some(&c) = chars.peek() {
        // Handle single-quoted strings
        if in_single_string {
            if c == '\'' {
                chars.next();
                if chars.peek() == Some(&'\'') {
                    // Escaped quote ''
                    chars.next();
                } else {
                    in_single_string = false;
                }
            } else {
                chars.next();
            }
            continue;
        }

        // Handle double-quoted identifiers
        if in_double_ident {
            if c == '"' {
                chars.next();
                in_double_ident = false;
            } else {
                chars.next();
            }
            continue;
        }

        // Handle line comments
        if in_line_comment {
            if c == '\n' {
                in_line_comment = false;
            }
            chars.next();
            continue;
        }

        // Handle block comments
        if in_block_comment {
            if c == '*' {
                chars.next();
                if chars.peek() == Some(&'/') {
                    chars.next();
                    in_block_comment = false;
                }
            } else {
                chars.next();
            }
            continue;
        }

        // Check for comment starts
        if c == '-' {
            chars.next();
            if chars.peek() == Some(&'-') {
                chars.next();
                in_line_comment = true;
                continue;
            }
            continue;
        }

        if c == '/' {
            chars.next();
            if chars.peek() == Some(&'*') {
                chars.next();
                in_block_comment = true;
                continue;
            }
            continue;
        }

        // Start of single-quoted string
        if c == '\'' {
            in_single_string = true;
            chars.next();
            continue;
        }

        // Start of double-quoted identifier
        if c == '"' {
            in_double_ident = true;
            chars.next();
            continue;
        }

        // Collect token
        if c.is_alphanumeric() || c == '_' {
            let mut token = String::new();
            while let Some(&ch) = chars.peek() {
                if ch.is_alphanumeric() || ch == '_' {
                    token.push(ch);
                    chars.next();
                } else {
                    break;
                }
            }

            let upper_token = token.to_uppercase();
            match upper_token.as_str() {
                "LIMIT" | "OFFSET" | "FETCH" | "INTO" => return true,
                _ => {}
            }
        } else {
            chars.next();
        }
    }

    false
}

/// Check if statement ends in an open string literal or comment.
fn ends_in_open_construct(sql: &str) -> bool {
    let mut chars = sql.chars().peekable();
    let mut in_single_string = false;
    let mut in_double_ident = false;
    let mut in_line_comment = false;
    let mut in_block_comment = false;

    while let Some(c) = chars.next() {
        // Handle single-quoted strings
        if in_single_string {
            if c == '\'' {
                if chars.peek() == Some(&'\'') {
                    // Escaped quote ''
                    chars.next();
                } else {
                    in_single_string = false;
                }
            }
            continue;
        }

        // Handle double-quoted identifiers
        if in_double_ident {
            if c == '"' {
                in_double_ident = false;
            }
            continue;
        }

        // Handle line comments
        if in_line_comment {
            if c == '\n' {
                in_line_comment = false;
            }
            continue;
        }

        // Handle block comments
        if in_block_comment {
            if c == '*' && chars.peek() == Some(&'/') {
                chars.next();
                in_block_comment = false;
            }
            continue;
        }

        // Check for comment starts
        if c == '-' && chars.peek() == Some(&'-') {
            chars.next();
            in_line_comment = true;
            continue;
        }

        if c == '/' && chars.peek() == Some(&'*') {
            chars.next();
            in_block_comment = true;
            continue;
        }

        // Start of single-quoted string
        if c == '\'' {
            in_single_string = true;
            continue;
        }

        // Start of double-quoted identifier
        if c == '"' {
            in_double_ident = true;
            continue;
        }
    }

    // If we end in any open state, return true
    in_single_string || in_double_ident || in_line_comment || in_block_comment
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_allowlist() {
        assert!(is_read_statement("  SELECT 1"));
        assert!(is_read_statement("-- note\nwith x as (select 1) select * from x"));
        assert!(is_read_statement("EXPLAIN ANALYZE select 1"));
        assert!(!is_read_statement("UPDATE t SET a=1"));
        assert!(!is_read_statement("/* c */ delete from t"));
        assert!(!is_read_statement("insert into t values (1)"));
    }

    #[test]
    fn auto_limit_appends() {
        let (sql, changed) = apply_auto_limit("select * from big", 1000);
        assert!(changed);
        assert_eq!(sql, "select * from big LIMIT 1000");
        let (sql2, changed2) = apply_auto_limit("select * from big;", 1000);
        assert!(changed2);
        assert_eq!(sql2, "select * from big LIMIT 1000;");
    }

    #[test]
    fn auto_limit_leaves_limited_and_nonselect_alone() {
        assert!(!apply_auto_limit("select * from t limit 5", 1000).1);
        assert!(!apply_auto_limit("select * from t OFFSET 2", 1000).1);
        assert!(!apply_auto_limit("update t set a=1", 1000).1);
        assert!(!apply_auto_limit("with x as (select 1) select * from x", 1000).1);
        // LIMIT inside a string literal must not count as a LIMIT token:
        let (s, ch) = apply_auto_limit("select 'no limit here' from t", 1000);
        assert!(ch);
        assert_eq!(s, "select 'no limit here' from t LIMIT 1000");
    }
}

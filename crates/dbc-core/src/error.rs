use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryError {
    pub code: Option<String>,
    pub message: String,
    /// Byte offset into the SQL text, when the server reports one.
    pub position: Option<u32>,
}

impl QueryError {
    pub fn msg(m: impl Into<String>) -> Self {
        Self { code: None, message: m.into(), position: None }
    }
}

impl fmt::Display for QueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(c) = &self.code { write!(f, "[{c}] ")?; }
        write!(f, "{}", self.message)?;
        if let Some(p) = self.position { write!(f, " (at {p})")?; }
        Ok(())
    }
}

impl std::error::Error for QueryError {}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn display_includes_code_and_position() {
        let e = QueryError { code: Some("42601".into()), message: "syntax error".into(), position: Some(15) };
        assert_eq!(e.to_string(), "[42601] syntax error (at 15)");
        assert_eq!(QueryError::msg("boom").to_string(), "boom");
    }
}

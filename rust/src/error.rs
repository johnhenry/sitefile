//! Errors with stable codes and source positions, matching the TypeScript
//! implementation (`src/errors.ts`): same code names, same message format
//! `file:line:column: detail`, so the two CLIs print identical errors.

use std::fmt;

/// A position in a source file. Lines and columns are 1-based (columns count
/// UTF-16 code units, like the TypeScript version).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SourceSpan {
    pub file: String,
    pub line: u32,
    pub column: u32,
}

impl SourceSpan {
    pub fn new(file: impl Into<String>, line: u32, column: u32) -> Self {
        SourceSpan { file: file.into(), line, column }
    }
}

impl fmt::Display for SourceSpan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}:{}", self.file, self.line, self.column)
    }
}

macro_rules! codes {
    ($($v:ident),* $(,)?) => {
        /// Stable error codes. Add new ones; never rename (tests and users match on them).
        #[allow(non_camel_case_types)]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum ErrorCode { $($v),* }
        impl ErrorCode {
            pub fn as_str(self) -> &'static str {
                match self { $(ErrorCode::$v => stringify!($v)),* }
            }
        }
    };
}

codes!(
    E_SYNTAX, E_UNKNOWN_INSTRUCTION, E_UNSUPPORTED, E_VALIDATION, E_UNDEFINED, E_DUPLICATE,
    E_MISSING_ARG, E_TYPE,
    E_PATH, E_PATH_ESCAPE, E_NOT_FOUND, E_UNSUPPORTED_FILE, E_HARDLINK, E_PATH_COLLISION,
    E_CONFLICT,
    E_PROTOCOL, E_SSRF_BLOCKED, E_FETCH, E_FETCH_STATUS, E_FETCH_TIMEOUT, E_TOO_LARGE, E_REDIRECT,
    E_INTEGRITY, E_LOCK_MISMATCH, E_LOCK_MISSING, E_UNPINNED,
    E_ARCHIVE, E_ARCHIVE_UNSAFE, E_ARCHIVE_LIMIT,
    E_ENCODING, E_TEMPLATE, E_FRONTMATTER, E_DATA,
    E_PLUGIN,
    E_BROKEN_LINK,
    E_EXPORT, E_EXPORT_UNSAFE,
    E_STORE, E_SERVER, E_REGISTRY, E_AUTH,
    E_BAKE,
    E_USAGE,
);

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone)]
pub struct SitefileError {
    pub code: ErrorCode,
    /// The message without the location prefix.
    pub detail: String,
    pub span: Option<SourceSpan>,
}

impl SitefileError {
    pub fn new(code: ErrorCode, detail: impl Into<String>, span: Option<SourceSpan>) -> Self {
        SitefileError { code, detail: detail.into(), span }
    }
    pub fn at(code: ErrorCode, detail: impl Into<String>, span: &SourceSpan) -> Self {
        SitefileError { code, detail: detail.into(), span: Some(span.clone()) }
    }
    pub fn plain(code: ErrorCode, detail: impl Into<String>) -> Self {
        SitefileError { code, detail: detail.into(), span: None }
    }
    /// `file:line:col: detail`, or just `detail`.
    pub fn message(&self) -> String {
        match &self.span {
            Some(s) => format!("{s}: {}", self.detail),
            None => self.detail.clone(),
        }
    }
}

impl fmt::Display for SitefileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for SitefileError {}

pub type Result<T, E = SitefileError> = std::result::Result<T, E>;

/// Shorthand: `err(E_PATH, "msg", Some(span))`.
pub fn err<T>(code: ErrorCode, detail: impl Into<String>, span: Option<&SourceSpan>) -> Result<T> {
    Err(SitefileError::new(code, detail, span.cloned()))
}

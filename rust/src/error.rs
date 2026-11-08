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
        SourceSpan {
            file: file.into(),
            line,
            column,
        }
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
    E_SYNTAX,
    E_UNKNOWN_INSTRUCTION,
    E_UNSUPPORTED,
    E_VALIDATION,
    E_UNDEFINED,
    E_DUPLICATE,
    E_MISSING_ARG,
    E_TYPE,
    E_PATH,
    E_PATH_ESCAPE,
    E_NOT_FOUND,
    E_UNSUPPORTED_FILE,
    E_HARDLINK,
    E_PATH_COLLISION,
    E_CONFLICT,
    E_PROTOCOL,
    E_SSRF_BLOCKED,
    E_HOST_BLOCKED,
    E_FETCH,
    E_FETCH_STATUS,
    E_FETCH_TIMEOUT,
    E_TOO_LARGE,
    E_REDIRECT,
    E_INTEGRITY,
    E_LOCK_MISMATCH,
    E_LOCK_MISSING,
    E_UNPINNED,
    E_GIT,
    E_SECRET,
    E_ARCHIVE,
    E_ARCHIVE_UNSAFE,
    E_ARCHIVE_LIMIT,
    E_ENCODING,
    E_TEMPLATE,
    E_FRONTMATTER,
    E_DATA,
    E_SCHEMA,
    E_PLUGIN,
    E_BROKEN_LINK,
    E_EXPORT,
    E_EXPORT_UNSAFE,
    E_STORE,
    E_SERVER,
    E_REGISTRY,
    E_AUTH,
    E_BAKE,
    E_USAGE,
    E_CANCELLED,
);

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What a permission error asks for (see [`SitefileError::needs`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NeedsKind {
    /// A JavaScript `PLUGIN`: allowed by `--allow-plugin <name>` (or `--allow-plugins`).
    Plugin,
    /// A `TOOL` (an external program): allowed by `--allow-exec <name>`.
    Tool,
}

impl NeedsKind {
    /// `plugin` or `tool` (the TypeScript `needs.kind`).
    pub fn as_str(self) -> &'static str {
        match self {
            NeedsKind::Plugin => "plugin",
            NeedsKind::Tool => "tool",
        }
    }
    /// The flag that grants it: `--allow-plugin` or `--allow-exec`.
    pub fn flag(self) -> &'static str {
        match self {
            NeedsKind::Plugin => "--allow-plugin",
            NeedsKind::Tool => "--allow-exec",
        }
    }
}

/// The permission a build needs, carried by the `E_UNSUPPORTED` error of a
/// `PLUGIN` or `TOOL` that was not allowed, so an embedder need not parse the message.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Needs {
    pub kind: NeedsKind,
    /// The declared `PLUGIN` or `TOOL` name.
    pub name: String,
}

impl Needs {
    pub fn plugin(name: impl Into<String>) -> Self {
        Needs {
            kind: NeedsKind::Plugin,
            name: name.into(),
        }
    }
    pub fn tool(name: impl Into<String>) -> Self {
        Needs {
            kind: NeedsKind::Tool,
            name: name.into(),
        }
    }
    /// The flag that grants it: `--allow-plugin` or `--allow-exec`.
    pub fn flag(&self) -> &'static str {
        self.kind.flag()
    }
    /// The command-line arguments that grant it, e.g. `["--allow-plugin", "shout"]`.
    pub fn args(&self) -> [String; 2] {
        [self.flag().to_string(), self.name.clone()]
    }
}

#[derive(Debug, Clone)]
pub struct SitefileError {
    pub code: ErrorCode,
    /// The message without the location prefix.
    pub detail: String,
    pub span: Option<SourceSpan>,
    /// For a permission error (`E_UNSUPPORTED` at a `PLUGIN` or `TOOL` line):
    /// what must be allowed. `None` for every other error. Not part of the message.
    pub needs: Option<Needs>,
}

impl SitefileError {
    pub fn new(code: ErrorCode, detail: impl Into<String>, span: Option<SourceSpan>) -> Self {
        SitefileError {
            code,
            detail: detail.into(),
            span,
            needs: None,
        }
    }
    pub fn at(code: ErrorCode, detail: impl Into<String>, span: &SourceSpan) -> Self {
        SitefileError {
            code,
            detail: detail.into(),
            span: Some(span.clone()),
            needs: None,
        }
    }
    /// The same error at `span` when it has no location yet.
    pub fn with_span(mut self, span: Option<&SourceSpan>) -> Self {
        if self.span.is_none() {
            self.span = span.cloned();
        }
        self
    }
    pub fn plain(code: ErrorCode, detail: impl Into<String>) -> Self {
        SitefileError {
            code,
            detail: detail.into(),
            span: None,
            needs: None,
        }
    }
    /// The same error, recording the permission it asks for.
    pub fn with_needs(mut self, needs: Needs) -> Self {
        self.needs = Some(needs);
        self
    }
    /// `E_CANCELLED`: the build was stopped through its [`CancelToken`](crate::executor::CancelToken).
    pub fn cancelled() -> Self {
        SitefileError::plain(ErrorCode::E_CANCELLED, "the build was cancelled")
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

//! The typed Sitefile AST (src/language/ast.ts).

use crate::error::SourceSpan;

/// Part of a string with `${…}` interpolations. In Sitefile operands `path` is
/// empty except for a `FOR` loop variable; in templates it holds dotted access.
#[derive(Debug, Clone, PartialEq)]
pub enum TemplatePart {
    Text(String),
    Var { name: String, path: Vec<String>, span: SourceSpan },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Operand {
    pub parts: Vec<TemplatePart>,
    pub span: SourceSpan,
}

/// A heredoc body: `COPY <<CSS /a.css` … `CSS`.
#[derive(Debug, Clone, PartialEq)]
pub struct InlineBody {
    pub delimiter: String,
    /// Each line ends in `\n` (empty for an empty body).
    pub text: String,
    /// Span of the `<<` marker.
    pub span: SourceSpan,
    /// 1-based Sitefile line of the body's first line.
    pub body_line: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    Operand(Operand),
    Inline(InlineBody),
}

impl Source {
    pub fn span(&self) -> &SourceSpan {
        match self {
            Source::Operand(o) => &o.span,
            Source::Inline(b) => &b.span,
        }
    }
    pub fn as_operand(&self) -> Option<&Operand> {
        match self {
            Source::Operand(o) => Some(o),
            Source::Inline(_) => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueType {
    String,
    Number,
    Boolean,
}

impl ValueType {
    /// Every type (the parser and the language server share this list).
    pub const ALL: [ValueType; 3] = [ValueType::String, ValueType::Number, ValueType::Boolean];
    /// Case-insensitive (Unicode lowercase, as JavaScript's `toLowerCase`).
    pub fn parse(s: &str) -> Option<ValueType> {
        let lower = s.to_lowercase();
        Self::ALL.into_iter().find(|t| t.as_str() == lower)
    }
    pub fn as_str(self) -> &'static str {
        match self {
            ValueType::String => "string",
            ValueType::Number => "number",
            ValueType::Boolean => "boolean",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportKind {
    File,
    Text,
    Json,
    Tree,
    Markdown,
    Yaml,
    Csv,
    Wbn,
}

impl ImportKind {
    /// Every kind, in the order docs/LANGUAGE.md lists them.
    pub const ALL: [ImportKind; 8] = [
        ImportKind::File,
        ImportKind::Text,
        ImportKind::Json,
        ImportKind::Yaml,
        ImportKind::Csv,
        ImportKind::Markdown,
        ImportKind::Wbn,
        ImportKind::Tree,
    ];
    /// Case-insensitive (ASCII).
    pub fn parse(s: &str) -> Option<ImportKind> {
        let lower = s.to_ascii_lowercase();
        Self::ALL.into_iter().find(|k| k.as_str() == lower)
    }
    pub fn as_str(self) -> &'static str {
        match self {
            ImportKind::File => "file",
            ImportKind::Text => "text",
            ImportKind::Json => "json",
            ImportKind::Tree => "tree",
            ImportKind::Markdown => "markdown",
            ImportKind::Yaml => "yaml",
            ImportKind::Csv => "csv",
            ImportKind::Wbn => "wbn",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EscapeMode {
    None,
    Html,
}

impl EscapeMode {
    /// Every `--escape=` value.
    pub const ALL: [EscapeMode; 2] = [EscapeMode::None, EscapeMode::Html];
    /// Case-insensitive (Unicode lowercase, as JavaScript's `toLowerCase`).
    pub fn parse(s: &str) -> Option<EscapeMode> {
        let lower = s.to_lowercase();
        Self::ALL.into_iter().find(|m| m.as_str() == lower)
    }
    pub fn as_str(self) -> &'static str {
        match self {
            EscapeMode::None => "none",
            EscapeMode::Html => "html",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    Directory,
    Zip,
    Tar,
    Tgz,
    Wbn,
    Oci,
}

impl ExportFormat {
    /// Every format, in the order docs/LANGUAGE.md lists them (`tar.gz` is an alias, not listed).
    pub const ALL: [ExportFormat; 6] =
        [ExportFormat::Directory, ExportFormat::Zip, ExportFormat::Tar, ExportFormat::Tgz, ExportFormat::Wbn, ExportFormat::Oci];
    /// `tar.gz` is an alias for `tgz`; case-insensitive.
    pub fn parse(name: &str) -> Option<ExportFormat> {
        let lower = name.to_ascii_lowercase();
        if lower == "tar.gz" {
            return Some(ExportFormat::Tgz);
        }
        Self::ALL.into_iter().find(|f| f.as_str() == lower)
    }
    pub fn as_str(self) -> &'static str {
        match self {
            ExportFormat::Directory => "directory",
            ExportFormat::Zip => "zip",
            ExportFormat::Tar => "tar",
            ExportFormat::Tgz => "tgz",
            ExportFormat::Wbn => "wbn",
            ExportFormat::Oci => "oci",
        }
    }
}

/// OCI site layer encoding: `gzip` (tar+gzip, the default) or `none` (plain tar).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OciCompression {
    #[default]
    Gzip,
    None,
}

impl OciCompression {
    /// Every `COMPRESSION` value.
    pub const ALL: [OciCompression; 2] = [OciCompression::Gzip, OciCompression::None];
    /// Case-insensitive.
    pub fn parse(name: &str) -> Option<OciCompression> {
        let lower = name.to_ascii_lowercase();
        Self::ALL.into_iter().find(|c| c.as_str() == lower)
    }
    pub fn as_str(self) -> &'static str {
        match self {
            OciCompression::Gzip => "gzip",
            OciCompression::None => "none",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct StageRef {
    pub name: String,
    pub span: SourceSpan,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ForEach {
    pub as_name: String,
    pub list: String,
    pub span: SourceSpan,
}

/// A build secret sent as an HTTP request header of a remote IMPORT.
#[derive(Debug, Clone, PartialEq)]
pub struct ImportSecret {
    /// The `--secret id=…` it names.
    pub id: String,
    /// Header name as written (`Authorization` for BEARER).
    pub header: String,
    /// `AS BEARER`: the header value is `Bearer <secret>`.
    pub bearer: bool,
    pub span: SourceSpan,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SortBy {
    pub key: String,
    pub descending: bool,
    pub span: SourceSpan,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TransformOption {
    pub key: String,
    pub value: Operand,
    pub span: SourceSpan,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FromBase {
    Scratch,
    Operand(Operand),
    /// `image <ref> [AT <dir>]`: the files of an OCI site image (a registry or the local store).
    Image { reference: Operand, at: Option<Operand> },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Instruction {
    /// `stage_kw` is set for `STAGE name [FROM base]` (same meaning as `FROM base AS name`); used only in error messages.
    From { span: SourceSpan, base: FromBase, alias: Option<String>, stage_kw: bool },
    Arg { span: SourceSpan, name: String, ty: ValueType, default_value: Option<Operand> },
    Let { span: SourceSpan, name: String, ty: ValueType, value: Operand },
    Import {
        span: SourceSpan,
        name: String,
        uri: Operand,
        as_kind: Option<ImportKind>,
        integrity: Option<String>,
        sort: Option<SortBy>,
        /// `SECRET <id> AS HEADER <Name>` / `SECRET <id> AS BEARER` (build secrets).
        secrets: Vec<ImportSecret>,
    },
    /// `exclude`: `--exclude` patterns (`.sitefileignore` syntax, relative to the source directory);
    /// `chmod`: `--chmod=644|755`; `parents`: `--parents` (keep the source's path under `dest`).
    Copy {
        span: SourceSpan,
        source: Source,
        dest: Operand,
        from: Option<StageRef>,
        overwrite: bool,
        exclude: Vec<String>,
        chmod: Option<u32>,
        parents: bool,
    },
    Render {
        span: SourceSpan,
        template: Source,
        dest: Operand,
        from: Option<StageRef>,
        escape: EscapeMode,
        each: Option<ForEach>,
        overwrite: bool,
    },
    Plugin { span: SourceSpan, name: String, uri: Operand, integrity: Option<String> },
    Tool { span: SourceSpan, name: String },
    Transform {
        span: SourceSpan,
        source: Option<Source>,
        from: Option<StageRef>,
        plugin: String,
        plugin_span: SourceSpan,
        dest: Operand,
        options: Vec<TransformOption>,
        each: Option<ForEach>,
        /// argv template of a TOOL (`{input}`, `{output}`, `{option.key}`).
        args: Option<Vec<Operand>>,
        overwrite: bool,
    },
    /// `SITEMAP [--overwrite] BASE <url> [TO <dest>]` (dest defaults to `/sitemap.xml`).
    Sitemap { span: SourceSpan, base: Operand, dest: Option<Operand>, overwrite: bool },
    /// `FEED [--overwrite] <import> BASE <url> LINK <path> [TO <dest>] [TITLE <text>]`; LINK may use `${item…}`.
    Feed {
        span: SourceSpan,
        list: String,
        list_span: SourceSpan,
        base: Operand,
        link: Operand,
        dest: Option<Operand>,
        title: Option<Operand>,
        overwrite: bool,
    },
    /// `REDIRECT [--overwrite] <from> <to> [STATUS 301|302|303|307|308]`.
    Redirect { span: SourceSpan, from: Operand, to: Operand, status: u16, overwrite: bool },
    /// `HEADER [--overwrite] <pattern> <Name>: <value …>`; `name` has no trailing colon, `value` words are joined with a space.
    Header { span: SourceSpan, pattern: Operand, name: Operand, value: Vec<Operand>, overwrite: bool },
    Export {
        span: SourceSpan,
        path: Operand,
        format: ExportFormat,
        base: Option<Operand>,
        at: Option<Operand>,
        /// `COMPRESSION gzip|none` (oci only) and the span of its value.
        compression: Option<(OciCompression, SourceSpan)>,
    },
}

impl Instruction {
    pub fn span(&self) -> &SourceSpan {
        match self {
            Instruction::From { span, .. }
            | Instruction::Arg { span, .. }
            | Instruction::Let { span, .. }
            | Instruction::Import { span, .. }
            | Instruction::Copy { span, .. }
            | Instruction::Render { span, .. }
            | Instruction::Plugin { span, .. }
            | Instruction::Tool { span, .. }
            | Instruction::Transform { span, .. }
            | Instruction::Sitemap { span, .. }
            | Instruction::Feed { span, .. }
            | Instruction::Redirect { span, .. }
            | Instruction::Header { span, .. }
            | Instruction::Export { span, .. } => span,
        }
    }
    /// `FROM`, `ARG`, … as written in error messages.
    pub fn keyword(&self) -> &'static str {
        match self {
            Instruction::From { .. } => "FROM",
            Instruction::Arg { .. } => "ARG",
            Instruction::Let { .. } => "LET",
            Instruction::Import { .. } => "IMPORT",
            Instruction::Copy { .. } => "COPY",
            Instruction::Render { .. } => "RENDER",
            Instruction::Plugin { .. } => "PLUGIN",
            Instruction::Tool { .. } => "TOOL",
            Instruction::Transform { .. } => "TRANSFORM",
            Instruction::Sitemap { .. } => "SITEMAP",
            Instruction::Feed { .. } => "FEED",
            Instruction::Redirect { .. } => "REDIRECT",
            Instruction::Header { .. } => "HEADER",
            Instruction::Export { .. } => "EXPORT",
        }
    }
}

/// The loop variable a FEED's LINK uses for the current element.
pub const FEED_ITEM: &str = "item";

/// `REDIRECT … STATUS` values (the default is 301).
pub const REDIRECT_STATUSES: [u16; 5] = [301, 302, 303, 307, 308];

/// A parsed and semantically validated Sitefile.
#[derive(Debug, Clone)]
pub struct Program {
    pub file: String,
    pub instructions: Vec<Instruction>,
}

/// The text of an operand with no interpolations.
pub fn literal_text(op: &Operand) -> Option<String> {
    let mut out = String::new();
    for p in &op.parts {
        match p {
            TemplatePart::Text(t) => out.push_str(t),
            TemplatePart::Var { .. } => return None,
        }
    }
    Some(out)
}

pub fn is_ident(s: &str) -> bool {
    let mut c = s.chars();
    matches!(c.next(), Some(ch) if ch == '_' || ch.is_ascii_alphabetic()) && c.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

/// Every stage alias declared anywhere in the file.
pub fn stage_names(instructions: &[Instruction]) -> std::collections::BTreeSet<String> {
    instructions
        .iter()
        .filter_map(|i| match i {
            Instruction::From { alias: Some(a), .. } => Some(a.clone()),
            _ => None,
        })
        .collect()
}

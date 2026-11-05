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
    pub fn parse(s: &str) -> Option<ImportKind> {
        Some(match s.to_ascii_lowercase().as_str() {
            "file" => ImportKind::File,
            "text" => ImportKind::Text,
            "json" => ImportKind::Json,
            "tree" => ImportKind::Tree,
            "markdown" => ImportKind::Markdown,
            "yaml" => ImportKind::Yaml,
            "csv" => ImportKind::Csv,
            "wbn" => ImportKind::Wbn,
            _ => return None,
        })
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
    /// `tar.gz` is an alias for `tgz`; case-insensitive.
    pub fn parse(name: &str) -> Option<ExportFormat> {
        Some(match name.to_ascii_lowercase().as_str() {
            "directory" => ExportFormat::Directory,
            "zip" => ExportFormat::Zip,
            "tar" => ExportFormat::Tar,
            "tgz" | "tar.gz" => ExportFormat::Tgz,
            "wbn" => ExportFormat::Wbn,
            "oci" => ExportFormat::Oci,
            _ => return None,
        })
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
    /// Case-insensitive.
    pub fn parse(name: &str) -> Option<OciCompression> {
        match name.to_ascii_lowercase().as_str() {
            "gzip" => Some(OciCompression::Gzip),
            "none" => Some(OciCompression::None),
            _ => None,
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
}

#[derive(Debug, Clone, PartialEq)]
pub enum Instruction {
    From { span: SourceSpan, base: FromBase, alias: Option<String> },
    Arg { span: SourceSpan, name: String, ty: ValueType, default_value: Option<Operand> },
    Let { span: SourceSpan, name: String, ty: ValueType, value: Operand },
    Import {
        span: SourceSpan,
        name: String,
        uri: Operand,
        as_kind: Option<ImportKind>,
        integrity: Option<String>,
        sort: Option<SortBy>,
    },
    Copy { span: SourceSpan, source: Source, dest: Operand, from: Option<StageRef>, overwrite: bool },
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
            Instruction::Export { .. } => "EXPORT",
        }
    }
}

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

//! The typed Sitefile AST (src/language/ast.ts).

use crate::error::SourceSpan;

/// Part of a string with `${…}` interpolations. In Sitefile operands `path` is
/// empty except for a `FOR` loop variable; in templates it holds dotted access.
#[derive(Debug, Clone, PartialEq)]
pub enum TemplatePart {
    Text(String),
    Var {
        name: String,
        path: Vec<String>,
        span: SourceSpan,
    },
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

/// The template of an html LET: the rest of its line (`span` is its first character), or a heredoc.
#[derive(Debug, Clone, PartialEq)]
pub enum FragmentBody {
    Line { text: String, span: SourceSpan },
    Inline(InlineBody),
}

impl FragmentBody {
    pub fn text(&self) -> &str {
        match self {
            FragmentBody::Line { text, .. } => text,
            FragmentBody::Inline(b) => &b.text,
        }
    }
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
    /// A file-based html fragment (`IMPORT header FROM ./header.html AS html`).
    Html,
    /// `AS sitefile`: another Sitefile, whose stages this one reads as `<import>+<stage>`.
    Sitefile,
}

impl ImportKind {
    /// Every kind, in the order docs/LANGUAGE.md lists them.
    pub const ALL: [ImportKind; 10] = [
        ImportKind::File,
        ImportKind::Text,
        ImportKind::Json,
        ImportKind::Yaml,
        ImportKind::Csv,
        ImportKind::Markdown,
        ImportKind::Wbn,
        ImportKind::Html,
        ImportKind::Tree,
        ImportKind::Sitefile,
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
            ImportKind::Html => "html",
            ImportKind::Sitefile => "sitefile",
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
    Car,
    Html,
    Mhtml,
}

impl ExportFormat {
    /// Every format, in the order docs/LANGUAGE.md lists them (`tar.gz` is an alias, not listed).
    pub const ALL: [ExportFormat; 9] = [
        ExportFormat::Directory,
        ExportFormat::Zip,
        ExportFormat::Tar,
        ExportFormat::Tgz,
        ExportFormat::Wbn,
        ExportFormat::Oci,
        ExportFormat::Car,
        ExportFormat::Html,
        ExportFormat::Mhtml,
    ];
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
            ExportFormat::Car => "car",
            ExportFormat::Html => "html",
            ExportFormat::Mhtml => "mhtml",
        }
    }
}

/// `EXPORT ... FOR <host>`: whose config files the REDIRECT and HEADER rules become.
pub const EXPORT_HOSTS: [&str; 7] = [
    "netlify",
    "cloudflare",
    "vercel",
    "firebase",
    "azure",
    "github-pages",
    "s3",
];

/// `EXPORT ... COMPRESSION`: for `oci`, the site layer encoding, `gzip` (tar+gzip, the default) or `none`
/// (plain tar); for `wbn`, `gzip` wraps the bundle in gzip (the family's `.wbn.gz`) and `none` (the default
/// for wbn, so not this type's `Default`) writes the plain bundle.
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
    /// `BY key` (RENDER only): one element per distinct value of the field.
    pub by: Option<GroupBy>,
    /// `PAGE n [FIRST <dest>]` (RENDER only): pages of n items.
    pub page: Option<PageClause>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GroupBy {
    pub key: String,
    pub span: SourceSpan,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PageClause {
    pub size: Operand,
    /// `FIRST <dest>`: the first page's destination.
    pub first: Option<Operand>,
    pub span: SourceSpan,
}

/// `DRAFTS <true|false>` of an IMPORT.
#[derive(Debug, Clone, PartialEq)]
pub struct DraftsClause {
    pub value: Operand,
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

/// `ARG NAME=value` on an `IMPORT … AS sitefile`: sets an ARG of the imported Sitefile.
#[derive(Debug, Clone, PartialEq)]
pub struct ImportArg {
    pub name: String,
    pub value: Operand,
    pub span: SourceSpan,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SortBy {
    pub key: String,
    pub descending: bool,
    pub span: SourceSpan,
}

/// `SCHEMA <path>` of an IMPORT: a local YAML/JSON schema for its documents.
#[derive(Debug, Clone, PartialEq)]
pub struct SchemaClause {
    pub path: Operand,
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
    Image {
        reference: Operand,
        at: Option<Operand>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Instruction {
    /// `stage_kw` is set for `STAGE name [FROM base]` (same meaning as `FROM base AS name`); used only in error messages.
    From {
        span: SourceSpan,
        base: FromBase,
        alias: Option<String>,
        stage_kw: bool,
    },
    Arg {
        span: SourceSpan,
        name: String,
        ty: ValueType,
        default_value: Option<Operand>,
    },
    Let {
        span: SourceSpan,
        name: String,
        ty: ValueType,
        value: Operand,
    },
    /// `LET name: html = <markup>` / `LET name: html <<HTML`: an html fragment, evaluated where a
    /// RENDER template uses it (the IR of TypeScript's `FragmentInstruction`, kind `LET`, type `html`).
    Fragment {
        span: SourceSpan,
        name: String,
        body: FragmentBody,
    },
    Import {
        span: SourceSpan,
        name: String,
        uri: Operand,
        as_kind: Option<ImportKind>,
        integrity: Option<String>,
        sort: Option<SortBy>,
        /// `SCHEMA <path>`: every document (or list element) must match this schema.
        schema: Option<SchemaClause>,
        /// `DRAFTS <true|false>`: whether elements with `draft: true` are kept.
        drafts: Option<DraftsClause>,
        /// `SECRET <id> AS HEADER <Name>` / `SECRET <id> AS BEARER` (build secrets).
        secrets: Vec<ImportSecret>,
        /// `ARG NAME=value` (repeatable): build args of a Sitefile imported `AS sitefile`.
        args: Vec<ImportArg>,
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
        /// `--inline-svg`: the template's `${file … | svg}` may paste SVG markup into the page.
        inline_svg: bool,
    },
    Plugin {
        span: SourceSpan,
        name: String,
        uri: Operand,
        integrity: Option<String>,
    },
    Tool {
        span: SourceSpan,
        name: String,
    },
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
        /// `--include` / `--exclude` patterns (`.sitefileignore` syntax) for a directory source.
        include: Vec<String>,
        exclude: Vec<String>,
        overwrite: bool,
    },
    /// `SITEMAP [--overwrite] BASE <url> [TO <dest>]` (dest defaults to `/sitemap.xml`).
    Sitemap {
        span: SourceSpan,
        base: Operand,
        dest: Option<Operand>,
        overwrite: bool,
    },
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
    Redirect {
        span: SourceSpan,
        from: Operand,
        to: Operand,
        status: u16,
        overwrite: bool,
    },
    /// `HEADER [--overwrite] <pattern> <Name>: <value …>`; `name` has no trailing colon, `value` words are joined with a space.
    Header {
        span: SourceSpan,
        pattern: Operand,
        name: Operand,
        value: Vec<Operand>,
        overwrite: bool,
    },
    /// `FINGERPRINT [--overwrite] [--no-cache-headers] <pattern>… [MANIFEST <dest>] [INTEGRITY]`: rename
    /// matching files to `name.<hash>.ext` and rewrite the HTML and CSS that reference them. `integrity`
    /// adds SRI attributes; `no_cache_headers` turns off EXPORT's `Cache-Control` lines.
    Fingerprint {
        span: SourceSpan,
        patterns: Vec<Operand>,
        manifest: Option<Operand>,
        integrity: bool,
        no_cache_headers: bool,
        overwrite: bool,
    },
    /// `CANONICAL BASE <url> [PRETTY]`: a `<link rel="canonical">` before `</head>` of every page without one.
    Canonical {
        span: SourceSpan,
        base: Operand,
        pretty: bool,
    },
    /// `PRECOMPRESS [--overwrite] [gzip]… <pattern>… [MIN <bytes>]`: write a `.gz` sidecar next to each
    /// matching compressible file of at least MIN bytes, when it is smaller. `formats` is `["gzip"]`
    /// when none is given.
    Precompress {
        span: SourceSpan,
        formats: Vec<String>,
        patterns: Vec<Operand>,
        min: Option<Operand>,
        overwrite: bool,
    },
    /// `ROBOTS [--overwrite] [ALLOW <path> | DISALLOW <path>]… [SITEMAP] [TO <dest>]`: a `/robots.txt`;
    /// `rules` are (allow, path, span) in source order.
    Robots {
        span: SourceSpan,
        rules: Vec<(bool, Operand, SourceSpan)>,
        sitemap: bool,
        dest: Option<Operand>,
        overwrite: bool,
    },
    /// `SECURITY_TXT [--overwrite] CONTACT <uri>… EXPIRES <date> [ENCRYPTION <uri>]… [POLICY <uri>]… [TO <dest>]`:
    /// an RFC 9116 `/.well-known/security.txt`.
    SecurityTxt {
        span: SourceSpan,
        contact: Vec<Operand>,
        expires: Operand,
        encryption: Vec<Operand>,
        policy: Vec<Operand>,
        dest: Option<Operand>,
        overwrite: bool,
    },
    /// `CHECKLINKS [--warn] [--external]`: report references in the stage's HTML and CSS that point at nothing.
    CheckLinks {
        span: SourceSpan,
        warn: bool,
        external: bool,
    },
    Export {
        span: SourceSpan,
        path: Operand,
        format: ExportFormat,
        base: Option<Operand>,
        at: Option<Operand>,
        /// `COMPRESSION gzip|none` (oci only) and the span of its value.
        compression: Option<(OciCompression, SourceSpan)>,
        /// `FOR <host>` (one of [`EXPORT_HOSTS`], lowercase) and the span of its value.
        for_host: Option<(String, SourceSpan)>,
    },
}

impl Instruction {
    pub fn span(&self) -> &SourceSpan {
        match self {
            Instruction::From { span, .. }
            | Instruction::Arg { span, .. }
            | Instruction::Let { span, .. }
            | Instruction::Fragment { span, .. }
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
            | Instruction::Fingerprint { span, .. }
            | Instruction::Canonical { span, .. }
            | Instruction::Precompress { span, .. }
            | Instruction::Robots { span, .. }
            | Instruction::SecurityTxt { span, .. }
            | Instruction::CheckLinks { span, .. }
            | Instruction::Export { span, .. } => span,
        }
    }
    /// `FROM`, `ARG`, … as written in error messages.
    pub fn keyword(&self) -> &'static str {
        match self {
            Instruction::From { .. } => "FROM",
            Instruction::Arg { .. } => "ARG",
            Instruction::Let { .. } | Instruction::Fragment { .. } => "LET",
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
            Instruction::Fingerprint { .. } => "FINGERPRINT",
            Instruction::Canonical { .. } => "CANONICAL",
            Instruction::Precompress { .. } => "PRECOMPRESS",
            Instruction::Robots { .. } => "ROBOTS",
            Instruction::SecurityTxt { .. } => "SECURITY_TXT",
            Instruction::CheckLinks { .. } => "CHECKLINKS",
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

/// `theme+assets` -> `("theme", "assets")`: a stage of a Sitefile imported AS sitefile.
pub fn imported_stage(name: &str) -> Option<(&str, &str)> {
    let (a, b) = name.split_once('+')?;
    (is_ident(a) && is_ident(b)).then_some((a, b))
}

pub fn is_ident(s: &str) -> bool {
    let mut c = s.chars();
    matches!(c.next(), Some(ch) if ch == '_' || ch.is_ascii_alphabetic())
        && c.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
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

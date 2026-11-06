//! Resources and the policies that bound them (port of `src/resources/types.ts`).

use crate::tree::Tree;

/// Where a resource came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provenance {
    /// The URI as written (after variable interpolation), or `./relative/path`.
    pub uri: String,
    /// Final URL after redirects, for remote resources.
    pub resolved_url: Option<String>,
    /// `sha256:<hex>`: file bytes, tree digest, or the raw bytes a value was parsed from.
    pub digest: String,
    /// Git imports: the commit the content was read from (40 hex).
    pub git_commit: Option<String>,
}

/// The data format of a [`Resource::Value`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueFormat {
    Text,
    Json,
    /// One document object, or a list of them for a directory.
    Markdown,
    Yaml,
    Csv,
    Wbn,
}

impl ValueFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            ValueFormat::Text => "text",
            ValueFormat::Json => "json",
            ValueFormat::Markdown => "markdown",
            ValueFormat::Yaml => "yaml",
            ValueFormat::Csv => "csv",
            ValueFormat::Wbn => "wbn",
        }
    }
}

#[derive(Debug, Clone)]
pub enum Resource {
    File {
        bytes: Vec<u8>,
        /// 0o644 or 0o755.
        mode: u32,
        /// Suggested basename when copied to a directory destination.
        basename: String,
        provenance: Provenance,
    },
    Tree {
        tree: Tree,
        provenance: Provenance,
    },
    Value {
        format: ValueFormat,
        value: serde_json::Value,
        provenance: Provenance,
    },
}

impl Resource {
    pub fn provenance(&self) -> &Provenance {
        match self {
            Resource::File { provenance, .. } | Resource::Tree { provenance, .. } | Resource::Value { provenance, .. } => provenance,
        }
    }
    /// `file`, `tree` or `value`, as the TypeScript `kind` field.
    pub fn kind(&self) -> &'static str {
        match self {
            Resource::File { .. } => "file",
            Resource::Tree { .. } => "tree",
            Resource::Value { .. } => "value",
        }
    }
}

/// Network policy for remote resources. Defaults are secure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchPolicy {
    /// Permit plain `http:` (default false).
    pub allow_http: bool,
    /// Permit loopback, private, link-local and other non-public addresses (default false).
    pub allow_private_network: bool,
    /// Maximum response body size in bytes (default 50 MiB).
    pub max_bytes: u64,
    /// Total time budget per resource including redirects, ms (default 30 000).
    pub timeout_ms: u64,
    /// Maximum redirects followed (default 5).
    pub max_redirects: u32,
    /// Extra PEM-encoded CA certificate(s) to trust for https, in addition to
    /// the default roots (never instead of them). Certificate and hostname
    /// verification stay on.
    pub ca: Option<Vec<u8>>,
}

pub const DEFAULT_FETCH_POLICY: FetchPolicy = FetchPolicy {
    allow_http: false,
    allow_private_network: false,
    max_bytes: 50 * 1024 * 1024,
    timeout_ms: 30_000,
    max_redirects: 5,
    ca: None,
};

impl Default for FetchPolicy {
    fn default() -> Self {
        DEFAULT_FETCH_POLICY
    }
}

/// Limits applied when an archive is extracted with `IMPORT … AS tree`. Defaults are secure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArchiveLimits {
    /// Maximum number of entries (default 10 000).
    pub max_entries: u64,
    /// Maximum total uncompressed bytes (default 500 MiB).
    pub max_total_bytes: u64,
    /// Maximum uncompressed:compressed ratio of the gzip/zip data (default 100).
    pub max_ratio: u64,
}

pub const DEFAULT_ARCHIVE_LIMITS: ArchiveLimits = ArchiveLimits {
    max_entries: 10_000,
    max_total_bytes: 500 * 1024 * 1024,
    max_ratio: 100,
};

impl Default for ArchiveLimits {
    fn default() -> Self {
        DEFAULT_ARCHIVE_LIMITS
    }
}

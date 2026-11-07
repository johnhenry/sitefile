//! Export format dispatch (port of `src/export-formats.ts`).

use super::archive_writers::{write_tar, write_tgz, write_zip, WriterOptions};
use super::wbn::write_web_bundle;
use crate::error::Result;
use crate::language::ast::ExportFormat;
use crate::tree::Tree;

/// Single-file formats encoded by [`encode_tree`]. `oci` is handled separately
/// (layout directory or OCI archive) and `directory` by the exporter.
pub fn is_archive_format(f: ExportFormat) -> bool {
    !matches!(f, ExportFormat::Directory | ExportFormat::Oci)
}

/// Default output path relative to the context.
pub fn default_out_name(format: ExportFormat) -> &'static str {
    match format {
        ExportFormat::Directory => "dist",
        ExportFormat::Zip => "dist.zip",
        ExportFormat::Tar => "dist.tar",
        ExportFormat::Tgz => "dist.tar.gz",
        ExportFormat::Wbn => "dist.wbn",
        ExportFormat::Oci => "dist.oci.tar",
    }
}

#[derive(Debug, Clone, Default)]
pub struct EncodeOptions {
    pub writer: WriterOptions,
    /// Required for `wbn`: absolute https URL ending in `/`.
    pub base_url: Option<String>,
}

/// Serialize a tree in an archive format. `directory` and `oci` are not
/// archive formats; passing them is a programming error reported as E_EXPORT.
pub fn encode_tree(tree: &Tree, format: ExportFormat, options: &EncodeOptions) -> Result<Vec<u8>> {
    match format {
        ExportFormat::Zip => write_zip(tree, &options.writer),
        ExportFormat::Tar => write_tar(tree, &options.writer),
        ExportFormat::Tgz => write_tgz(tree, &options.writer),
        ExportFormat::Wbn => write_web_bundle(tree, options.base_url.as_deref()),
        ExportFormat::Directory | ExportFormat::Oci => Err(crate::error::SitefileError::plain(
            crate::error::ErrorCode::E_EXPORT,
            format!("'{}' is not an archive format", format.as_str()),
        )),
    }
}

/// The export format implied by an output file name, if it has an archive extension.
pub fn format_from_extension(out: &str) -> Option<ExportFormat> {
    let name = out.to_lowercase();
    if name.ends_with(".oci.tar") {
        Some(ExportFormat::Oci)
    } else if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
        Some(ExportFormat::Tgz)
    } else if name.ends_with(".tar") {
        Some(ExportFormat::Tar)
    } else if name.ends_with(".zip") {
        Some(ExportFormat::Zip)
    } else if name.ends_with(".wbn") {
        Some(ExportFormat::Wbn)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::export::archive_writers::tests::sample;

    #[test]
    fn dispatches_every_archive_format() {
        let t = sample();
        let o = EncodeOptions {
            base_url: Some("https://example.com/".into()),
            ..Default::default()
        };
        assert_eq!(
            &encode_tree(&t, ExportFormat::Zip, &o).unwrap()[..4],
            b"PK\x03\x04"
        );
        assert_eq!(
            &encode_tree(&t, ExportFormat::Tar, &o).unwrap()[257..262],
            b"ustar"
        );
        assert_eq!(
            &encode_tree(&t, ExportFormat::Tgz, &o).unwrap()[..2],
            &[0x1f, 0x8b]
        );
        assert_eq!(encode_tree(&t, ExportFormat::Wbn, &o).unwrap()[0], 0x85);
        assert!(encode_tree(&t, ExportFormat::Oci, &o).is_err());
        assert!(
            is_archive_format(ExportFormat::Tgz)
                && !is_archive_format(ExportFormat::Oci)
                && !is_archive_format(ExportFormat::Directory)
        );
    }

    #[test]
    fn extensions_and_defaults() {
        assert_eq!(format_from_extension("x.OCI.tar"), Some(ExportFormat::Oci));
        assert_eq!(format_from_extension("x.tar.gz"), Some(ExportFormat::Tgz));
        assert_eq!(format_from_extension("x.tgz"), Some(ExportFormat::Tgz));
        assert_eq!(format_from_extension("x.tar"), Some(ExportFormat::Tar));
        assert_eq!(format_from_extension("x.Zip"), Some(ExportFormat::Zip));
        assert_eq!(format_from_extension("x.wbn"), Some(ExportFormat::Wbn));
        assert_eq!(format_from_extension("dist"), None);
        assert_eq!(default_out_name(ExportFormat::Tgz), "dist.tar.gz");
        assert_eq!(default_out_name(ExportFormat::Oci), "dist.oci.tar");
    }
}

//! Export format dispatch (port of `src/export-formats.ts`).

use super::archive_writers::{write_tar, write_tgz, write_zip, WriterOptions};
use super::wbn::write_web_bundle;
use crate::deflate::gzip;
use crate::error::Result;
use crate::language::ast::{ExportFormat, OciCompression};
use crate::tree::Tree;

/// Single-file formats encoded by [`encode_tree`]. `oci` is handled separately
/// (layout directory or OCI archive) and `directory` by the exporter.
pub fn is_archive_format(f: ExportFormat) -> bool {
    !matches!(f, ExportFormat::Directory | ExportFormat::Oci)
}

/// Default output path relative to the context (`dist.wbn.gz` for a gzipped bundle).
pub fn default_out_name(format: ExportFormat, compression: Option<OciCompression>) -> &'static str {
    match format {
        ExportFormat::Directory => "dist",
        ExportFormat::Zip => "dist.zip",
        ExportFormat::Tar => "dist.tar",
        ExportFormat::Tgz => "dist.tar.gz",
        ExportFormat::Wbn if compression == Some(OciCompression::Gzip) => "dist.wbn.gz",
        ExportFormat::Wbn => "dist.wbn",
        ExportFormat::Oci => "dist.oci.tar",
        ExportFormat::Car => "dist.car",
        ExportFormat::Html => "dist.html",
        ExportFormat::Mhtml => "dist.mhtml",
        ExportFormat::Warc if compression == Some(OciCompression::Gzip) => "dist.warc.gz",
        ExportFormat::Warc => "dist.warc",
        ExportFormat::Epub => "dist.epub",
    }
}

#[derive(Debug, Clone, Default)]
pub struct EncodeOptions {
    pub writer: WriterOptions,
    /// Required for `wbn`: absolute https URL ending in `/`.
    pub base_url: Option<String>,
    /// `wbn`: `gzip` wraps the finished bundle in gzip (the shared deterministic encoder); default none.
    pub compression: Option<OciCompression>,
}

/// Serialize a tree in an archive format. `directory` and `oci` are not
/// archive formats; passing them is a programming error reported as E_EXPORT.
pub fn encode_tree(tree: &Tree, format: ExportFormat, options: &EncodeOptions) -> Result<Vec<u8>> {
    match format {
        ExportFormat::Zip => write_zip(tree, &options.writer),
        ExportFormat::Tar => write_tar(tree, &options.writer),
        ExportFormat::Tgz => write_tgz(tree, &options.writer),
        ExportFormat::Wbn => {
            // The bundle is written first, unchanged; gzip only wraps its bytes (header mtime 0, see deflate.rs).
            let bundle = write_web_bundle(tree, options.base_url.as_deref())?;
            Ok(if options.compression == Some(OciCompression::Gzip) {
                gzip(&bundle)
            } else {
                bundle
            })
        }
        ExportFormat::Car => Ok(super::car::encode_car(tree)?.0),
        ExportFormat::Html => super::single_file::write_single_file(tree, None),
        ExportFormat::Warc => crate::warc::write_warc(
            tree,
            options.base_url.as_deref(),
            options.writer.epoch,
            options.compression,
        ),
        ExportFormat::Mhtml => {
            super::single_file::write_mhtml(tree, None, options.base_url.as_deref())
        }
        ExportFormat::Epub => super::epub::write_epub(tree, options.writer.epoch),
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
    } else if name.ends_with(".wbn") || name.ends_with(".wbn.gz") {
        Some(ExportFormat::Wbn)
    } else if name.ends_with(".car") {
        Some(ExportFormat::Car)
    } else if name.ends_with(".warc") || name.ends_with(".warc.gz") {
        Some(ExportFormat::Warc)
    } else if name.ends_with(".mhtml") || name.ends_with(".mht") {
        Some(ExportFormat::Mhtml)
    } else if name.ends_with(".epub") {
        Some(ExportFormat::Epub)
    } else {
        None
    }
}

/// The compression implied by an output file name for `format`: `gzip` for a wbn written to `*.wbn.gz`.
pub fn compression_from_extension(out: &str, format: ExportFormat) -> Option<OciCompression> {
    let name = out.to_lowercase();
    match format {
        ExportFormat::Wbn => name.ends_with(".wbn.gz").then_some(OciCompression::Gzip),
        ExportFormat::Warc => name.ends_with(".warc.gz").then_some(OciCompression::Gzip),
        _ => None,
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
        let plain = encode_tree(&t, ExportFormat::Wbn, &o).unwrap();
        assert_eq!(plain[0], 0x85);
        let gz = EncodeOptions {
            compression: Some(OciCompression::Gzip),
            ..o.clone()
        };
        let packed = encode_tree(&t, ExportFormat::Wbn, &gz).unwrap();
        assert_eq!(packed, gzip(&plain));
        assert_eq!(&packed[..4], &[0x1f, 0x8b, 8, 0]);
        let none = EncodeOptions {
            compression: Some(OciCompression::None),
            ..o.clone()
        };
        assert_eq!(encode_tree(&t, ExportFormat::Wbn, &none).unwrap(), plain);
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
        assert_eq!(format_from_extension("x.WBN.gz"), Some(ExportFormat::Wbn));
        assert_eq!(format_from_extension("x.gz"), None);
        assert_eq!(format_from_extension("dist"), None);
        assert_eq!(
            compression_from_extension("x.wbn.GZ", ExportFormat::Wbn),
            Some(OciCompression::Gzip)
        );
        assert_eq!(compression_from_extension("x.wbn", ExportFormat::Wbn), None);
        assert_eq!(
            compression_from_extension("x.wbn.gz", ExportFormat::Zip),
            None
        );
        assert_eq!(default_out_name(ExportFormat::Tgz, None), "dist.tar.gz");
        assert_eq!(default_out_name(ExportFormat::Oci, None), "dist.oci.tar");
        assert_eq!(default_out_name(ExportFormat::Wbn, None), "dist.wbn");
        assert_eq!(
            default_out_name(ExportFormat::Wbn, Some(OciCompression::Gzip)),
            "dist.wbn.gz"
        );
        assert_eq!(
            default_out_name(ExportFormat::Wbn, Some(OciCompression::None)),
            "dist.wbn"
        );
    }
}

//! Short hover texts, condensed from docs/LANGUAGE.md. Keep them in step with it.

/// (keyword, signature, summary)
const INSTRUCTIONS: &[(&str, &str, &str)] = &[
    (
        "FROM",
        "FROM scratch|<base> [AS name]",
        "Starts a stage. `scratch` is an empty tree; a bare name of an earlier stage starts from a copy of its final tree; anything else is a context directory (`./name` forces the directory). Without any FROM, an implicit unnamed `FROM scratch` starts at the first non-ARG instruction.",
    ),
    (
        "STAGE",
        "STAGE name [FROM base]",
        "Starts a named stage: exactly `FROM base AS name`; `STAGE name` alone is `FROM scratch AS name`.",
    ),
    (
        "ARG",
        "ARG NAME[: string|number|boolean] [= value]",
        "A build parameter. Without a default it is required: pass `--build-arg NAME=value` (or `--build-arg-file`). Usable as `${NAME}` on later lines.",
    ),
    ("LET", "LET NAME[: type] = value", "A computed variable, usable as `${NAME}` on later lines. ARG and LET share one namespace; rebinding fails."),
    (
        "IMPORT",
        "IMPORT name FROM <uri> [AS kind] [INTEGRITY sha256:…] [SORT BY key [ASC|DESC]]",
        "Names a resource: a context-relative path or an `https://` URL. Kinds: file, text, json, yaml, csv, markdown, wbn, tree (default: tree for a directory, else file). COPY places files (`COPY name: /dest/`); text and data kinds are values for RENDER templates and FOR.",
    ),
    (
        "COPY",
        "COPY [--overwrite] [--from=stage] <source> <dest>",
        "Places a file or directory into the stage's tree. `<source>` is a context path, `name:` / `name:/inner` for an import, a path in an earlier stage with `--from`, or an inline `<<NAME` body. `<dest>` is absolute; a trailing `/` means a directory.",
    ),
    (
        "RENDER",
        "RENDER [--escape=none|html] [--overwrite] [--from=stage] <template> TO <dest> [FOR item IN list]",
        "Renders a template (a file or an inline `<<NAME` body) to a file. Templates use `${NAME}`, `${name.key}`, `${#each list as item}…${/each}`, `${#if x}…${else}…${/if}` and `${value|raw}`.",
    ),
    (
        "TRANSFORM",
        "TRANSFORM [--overwrite] [--from=stage] [<source>] WITH name TO <dest> [OPTIONS key=value …] [FOR item IN list] [ARGS …]",
        "Runs a PLUGIN (or an allowed TOOL) on the source and writes what it returns at `<dest>`. A TOOL needs `ARGS` with `{output}` (and `{input}` when there is a source).",
    ),
    (
        "PLUGIN",
        "PLUGIN name FROM ./plugins/x.mjs|x.wasm [INTEGRITY sha256:…]",
        "Declares a sandboxed renderer: a local ES module or an import-free WebAssembly module. URLs are not allowed. A JavaScript plugin runs only when the CLI allows it with `--allow-plugin name` (or `--allow-plugins`); a wasm plugin needs no flag. See docs/PLUGINS.md.",
    ),
    (
        "TOOL",
        "TOOL name",
        "Declares an external program for TRANSFORM … WITH name … ARGS. It runs only when the CLI allows it with `--allow-exec name`.",
    ),
    (
        "EXPORT",
        "EXPORT <path> [AS directory|zip|tar|tgz|wbn|oci] [BASE <url>] [AT <root>] [COMPRESSION gzip|none]",
        "Exports `<path>` of the last stage. Exactly one, and the last instruction. The CLI's `--out`, `--format` and `--base-url` override it.",
    ),
];

/// (instruction or "*", clause, text)
const CLAUSES: &[(&str, &str, &str)] = &[
    ("FROM", "AS", "`AS name` names this stage, so a later `FROM name`, `STAGE x FROM name` or `--from=name` can use it."),
    ("FROM", "SCRATCH", "`scratch` (the bare word) is an empty tree. `\"scratch\"` is a path."),
    ("STAGE", "FROM", "`FROM base`: `scratch`, an earlier stage, or a context directory. Omitted: `scratch`."),
    ("STAGE", "SCRATCH", "`scratch` (the bare word) is an empty tree."),
    ("IMPORT", "FROM", "`FROM <uri>`: a context-relative path or an `https://` URL (`http://` only with `--allow-http`)."),
    ("IMPORT", "AS", "`AS kind`: file, text, json, yaml, csv, markdown, wbn or tree. `AS tree` on a file extracts an archive."),
    ("IMPORT", "INTEGRITY", "`INTEGRITY sha256:<64 lowercase hex>`: the content must match; remote pins are recorded in Sitefile.lock."),
    ("IMPORT", "SORT", "`SORT BY key [ASC|DESC]` orders a list import (markdown, json, yaml, csv) by a field; numbers and numeric strings compare numerically."),
    ("IMPORT", "BY", "`SORT BY key`: the field to sort by."),
    ("IMPORT", "ASC", "Ascending order (the default)."),
    ("IMPORT", "DESC", "Descending order."),
    ("PLUGIN", "FROM", "`FROM ./path`: a local `.mjs` module (in its own folder) or `.wasm` module."),
    ("PLUGIN", "INTEGRITY", "`INTEGRITY sha256:<64 lowercase hex>`: the plugin file must match."),
    ("RENDER", "TO", "`TO <dest>`: the output file (absolute, no trailing `/`)."),
    ("RENDER", "FOR", "`FOR item IN list` renders once per element of a list import; `<dest>` may use `${item.key}`."),
    ("RENDER", "IN", "`IN list`: an IMPORT that is a list (markdown documents, a JSON/YAML array, CSV rows)."),
    ("TRANSFORM", "WITH", "`WITH name`: a PLUGIN or TOOL declared on an earlier line."),
    ("TRANSFORM", "TO", "`TO <dest>`: where the result is written (a file, or a directory for several files)."),
    ("TRANSFORM", "OPTIONS", "`OPTIONS key=value …`: passed to the plugin; TOOL ARGS can use `{option.key}`. Values may use `${item.key}`."),
    ("TRANSFORM", "FOR", "`FOR item IN list` runs once per element of a list import."),
    ("TRANSFORM", "IN", "`IN list`: an IMPORT that is a list."),
    ("TRANSFORM", "ARGS", "`ARGS …` (rest of the line): the TOOL's argv, no shell. `{input}`, `{output}` (required) and `{option.key}` are replaced."),
    ("EXPORT", "AS", "`AS format`: directory (default), zip, tar, tgz (alias tar.gz), wbn or oci."),
    ("EXPORT", "BASE", "`BASE <url>`: for wbn, the https URL the files are mounted at; for oci, a container image to build on."),
    ("EXPORT", "AT", "`AT <root>` (oci only): the directory the files occupy in the image (default /srv/www)."),
    ("EXPORT", "COMPRESSION", "`COMPRESSION gzip|none` (oci only): how the site layer is stored."),
];

const FLAGS: &[(&str, &str)] = &[
    ("overwrite", "`--overwrite`: replace files that already exist at the destination (otherwise a different file there is `E_CONFLICT`)."),
    ("from", "`--from=stage`: read the source from the final tree of an earlier, completed stage (an absolute path inside it)."),
    ("escape", "`--escape=none|html`: with `html`, `& < > \" '` are escaped in every substituted value (`${x|raw}` opts out)."),
];

const IMPORT_KINDS: &[(&str, &str)] = &[
    ("file", "One file, placed with COPY (the default for a file)."),
    ("text", "A UTF-8 text value for templates (`${name}`); cannot be COPY'd."),
    ("json", "A parsed JSON value for templates and FOR (a directory: a list of its .json files)."),
    ("yaml", "A parsed YAML value (safe subset; a directory: a list of its .yaml/.yml files)."),
    ("csv", "CSV rows as a list of objects of strings (the header row gives the keys)."),
    ("markdown", "Markdown documents: frontmatter fields plus slug, date, path, content (HTML) and excerpt."),
    ("wbn", "A Web Bundle as data: version, primaryUrl and entries."),
    ("tree", "A directory tree; on a file (zip, tar, tar.gz, wbn), extract the archive."),
];

const EXPORT_FORMATS: &[(&str, &str)] = &[
    ("directory", "A directory of files (the default)."),
    ("zip", "A zip archive (deterministic: fixed timestamps and modes)."),
    ("tar", "A tar archive."),
    ("tgz", "A gzip-compressed tar archive (alias `tar.gz`)."),
    ("wbn", "A Web Bundle; needs `BASE <https url>` or `--base-url`."),
    ("oci", "An OCI image (archive for `-o *.tar`, else a layout directory); `BASE`, `AT` and `COMPRESSION` apply."),
];

const COMPRESSIONS: &[(&str, &str)] =
    &[("gzip", "A tar+gzip site layer (the default)."), ("none", "A plain tar site layer (digest = diff_id).")];

const ESCAPES: &[(&str, &str)] =
    &[("none", "Substitute values as they are (the default)."), ("html", "Escape `& < > \" '` in substituted values.")];

const TYPES: &[(&str, &str)] = &[
    ("string", "Any text (the default)."),
    ("number", "A number; the value must parse as one."),
    ("boolean", "`true` or `false`."),
];

fn lookup<'a>(table: &'a [(&str, &str)], key: &str) -> Option<&'a str> {
    table.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| *v)
}

/// Markdown for an instruction keyword.
pub fn instruction(kw: &str) -> Option<String> {
    INSTRUCTIONS
        .iter()
        .find(|(k, _, _)| k.eq_ignore_ascii_case(kw))
        .map(|(_, sig, text)| format!("```sitefile\n{sig}\n```\n{text}"))
}

/// One-line signature of an instruction (completion detail).
pub fn signature(kw: &str) -> Option<&'static str> {
    INSTRUCTIONS.iter().find(|(k, _, _)| k.eq_ignore_ascii_case(kw)).map(|(_, sig, _)| *sig)
}

pub fn clause(kw: &str, clause: &str) -> Option<&'static str> {
    CLAUSES
        .iter()
        .find(|(k, c, _)| k.eq_ignore_ascii_case(kw) && c.eq_ignore_ascii_case(clause))
        .map(|(_, _, t)| *t)
}

pub fn flag(name: &str) -> Option<&'static str> {
    lookup(FLAGS, name)
}

pub fn import_kind(name: &str) -> Option<&'static str> {
    lookup(IMPORT_KINDS, name)
}

pub fn export_format(name: &str) -> Option<&'static str> {
    if name.eq_ignore_ascii_case("tar.gz") {
        return lookup(EXPORT_FORMATS, "tgz");
    }
    lookup(EXPORT_FORMATS, name)
}

pub fn compression(name: &str) -> Option<&'static str> {
    lookup(COMPRESSIONS, name)
}

pub fn escape(name: &str) -> Option<&'static str> {
    lookup(ESCAPES, name)
}

pub fn value_type(name: &str) -> Option<&'static str> {
    lookup(TYPES, name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language::ast::{EscapeMode, ExportFormat, ImportKind, OciCompression, ValueType};
    use crate::language::parser::{
        flags_of, EXPORT_CLAUSES, IMPORT_CLAUSES, INSTRUCTIONS as KWS, SORT_ORDERS, TRANSFORM_CLAUSES,
    };

    #[test]
    fn every_keyword_has_docs() {
        for kw in KWS {
            assert!(instruction(kw).is_some(), "{kw}");
            for (f, _) in flags_of(kw) {
                assert!(flag(f).is_some(), "--{f}");
            }
        }
        for k in ImportKind::ALL {
            assert!(import_kind(k.as_str()).is_some());
        }
        for f in ExportFormat::ALL {
            assert!(export_format(f.as_str()).is_some());
        }
        for c in OciCompression::ALL {
            assert!(compression(c.as_str()).is_some());
        }
        for e in EscapeMode::ALL {
            assert!(escape(e.as_str()).is_some());
        }
        for t in ValueType::ALL {
            assert!(value_type(t.as_str()).is_some());
        }
        for c in IMPORT_CLAUSES.iter().chain(&SORT_ORDERS) {
            assert!(clause("IMPORT", c).is_some(), "{c}");
        }
        for c in EXPORT_CLAUSES {
            assert!(clause("EXPORT", c).is_some(), "{c}");
        }
        for c in TRANSFORM_CLAUSES {
            assert!(clause("TRANSFORM", c).is_some(), "{c}");
        }
    }
}

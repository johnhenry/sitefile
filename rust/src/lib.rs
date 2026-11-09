//! Sitefile: a Docker-inspired static artifact builder.
//!
//! This crate is a port of the TypeScript reference implementation in `../src`.
//! Both are maintained; `../conformance` checks that they produce identical
//! output, and `../bench` compares their speed. Module names mirror the
//! TypeScript files they port.

// Core (shared by every module)
pub mod casefold_data;
pub mod cbor;
pub mod deflate;
pub mod dictionary;
pub mod digest;
pub mod ed25519;
pub mod error;
pub mod paths;
pub mod portable;
pub mod sha1;
pub mod tree;
pub mod values;
pub mod zstd;

// Language
pub mod build_args;
pub mod language;
pub mod lint;
pub mod planner;
pub mod secrets;

// Templates and data formats
pub mod bake;
pub mod bcrypt_verify;
pub mod csv;
pub mod documents;
pub mod frontmatter;
pub mod markdown;
pub mod media;
pub mod pages;
pub mod render;
pub mod schema;
pub mod yaml;

// Inputs
pub mod resources;

// Outputs, store and servers
pub mod cache_limit;
pub mod canonical;
pub mod checklinks;
pub mod csp;
pub mod export;
pub mod export_for;
pub mod fingerprint;
pub mod integrity;
pub mod links;
pub mod mime_data;
pub mod precache;
pub mod precompress;
pub mod qr;
pub mod refs;
pub mod serve_auth;
pub mod server;
pub mod servers;
pub mod servers_file;
pub mod site;
pub mod store;
pub mod toml;

// Build, plugins, registry and CLI
pub mod base_image;
pub mod cli;
pub mod executor;
pub mod explain;
pub mod host_config;
pub mod hosts;
pub mod memo;
pub mod outdated;
pub mod plugins;
pub mod profile;
pub mod vendor;
pub mod wacz;
pub mod warc;
pub mod watch;

// Editor tooling (Rust only; docs/LSP.md)
pub mod lsp;
pub mod registry;
pub mod registry_cache;

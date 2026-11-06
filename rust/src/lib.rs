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
pub mod digest;
pub mod sha1;
pub mod error;
pub mod paths;
pub mod tree;
pub mod values;

// Language
pub mod build_args;
pub mod language;
pub mod planner;
pub mod secrets;

// Templates and data formats
pub mod csv;
pub mod documents;
pub mod frontmatter;
pub mod markdown;
pub mod render;
pub mod yaml;
pub mod bake;

// Inputs
pub mod resources;

// Outputs, store and servers
pub mod export;
pub mod refs;
pub mod server;
pub mod servers;
pub mod site;
pub mod store;
pub mod cache_limit;

// Build, plugins, registry and CLI
pub mod base_image;
pub mod cli;
pub mod executor;
pub mod outdated;
pub mod explain;
pub mod plugins;
pub mod memo;
pub mod profile;
pub mod vendor;
pub mod watch;

// Editor tooling (Rust only; docs/LSP.md)
pub mod lsp;
pub mod registry;
pub mod registry_cache;

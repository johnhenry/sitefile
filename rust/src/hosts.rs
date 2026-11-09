//! `sitefile hosts` (port of `src/hosts.ts`, issue #176): the hosts a build
//! would contact, found from the plan alone (nothing is fetched), with what
//! each is needed for and whether the current host rules allow it.
//!
//! Listed: every remote IMPORT (https and git+https: the URL's host), every
//! `FROM image` stage base with a registry, and the BASE image of an oci
//! export (or `--base`): the registry's API host. Not known before fetching,
//! so not listed: redirects, a registry's token realm and blob CDN, the inputs
//! of an imported Sitefile (`IMPORT … AS sitefile`, marked `nested`), and the
//! links `CHECKLINKS --external` checks (reported as `checklinksExternal`).

use crate::error::{Needs, Result, SourceSpan};
use crate::executor::{load_program, BuildOptions, Loaded};
use crate::language::ast::{ExportFormat, ImportKind};
use crate::planner::{plan, ImportLocation, Operation, StageBase};
use crate::refs::{parse_ref, registry_api_host, with_default_registry};
use crate::resources::network::{check_secret_hosts, host_allowed, host_matches, NetworkSettings};
use serde_json::{json, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostStatus {
    Allowed,
    Blocked,
    Denied,
}

impl HostStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            HostStatus::Allowed => "allowed",
            HostStatus::Blocked => "blocked",
            HostStatus::Denied => "denied",
        }
    }
}

#[derive(Debug, Clone)]
pub struct HostUse {
    /// `import`, `image` or `base`.
    pub kind: &'static str,
    /// The IMPORT name, the stage of a `FROM image` (its name, or `#<n>`), or `BASE`.
    pub name: String,
    /// The URL, or the canonical image reference.
    pub source: String,
    /// The instruction (None for a BASE given with --base or on EXPORT).
    pub span: Option<SourceSpan>,
    /// `IMPORT … SECRET` ids sent to this host.
    pub secrets: Vec<String>,
    /// An imported Sitefile: its own inputs are only known once it is fetched.
    pub nested: bool,
    /// Strict mode: the `--allow-secret-host` rule this use still needs (the first missing one).
    pub needs: Option<Needs>,
}

#[derive(Debug, Clone)]
pub struct HostEntry {
    /// As written in the URL (lowercase, IPv6 without brackets).
    pub host: String,
    pub port: u16,
    pub status: HostStatus,
    /// For `blocked`: the `--allow-hosts` that allows it.
    pub needs: Option<Needs>,
    pub uses: Vec<HostUse>,
}

#[derive(Debug, Clone)]
pub struct HostsReport {
    pub strict: bool,
    pub hosts: Vec<HostEntry>,
    /// Lines of `CHECKLINKS --external`, which contacts the hosts of the site's external links.
    pub checklinks_external: Vec<u32>,
}

fn status_of(host: &str, port: u16, network: &NetworkSettings) -> HostStatus {
    if host_allowed(host, port, network) {
        HostStatus::Allowed
    } else if network
        .deny_hosts
        .iter()
        .any(|p| host_matches(host, port, p))
    {
        HostStatus::Denied
    } else {
        HostStatus::Blocked
    }
}

fn normalize(host: &str) -> String {
    let h = host.to_lowercase();
    let h = h
        .strip_prefix('[')
        .and_then(|x| x.strip_suffix(']'))
        .unwrap_or(&h)
        .to_string();
    h.strip_suffix('.').map(str::to_string).unwrap_or(h)
}

pub fn hosts(options: &BuildOptions) -> Result<HostsReport> {
    let Loaded { program, .. } = load_program(
        &options.context_dir,
        options.sitefile.as_deref(),
        options.label.as_deref(),
    )?;
    let p = plan(&program, &options.build_args)?;
    if let Some(w) = &options.on_warning {
        for m in &p.warnings {
            w(m);
        }
    }
    let network = &options.fetch_policy.network;
    let mut entries: Vec<(String, u16, Vec<HostUse>)> = Vec::new();
    let mut add = |host: &str, port: u16, used: HostUse| {
        let h = normalize(host);
        match entries.iter_mut().find(|e| e.0 == h && e.1 == port) {
            Some(e) => e.2.push(used),
            None => entries.push((h, port, vec![used])),
        }
    };
    for spec in &p.imports {
        let (target, source) = match &spec.location {
            ImportLocation::Local { .. } => continue,
            ImportLocation::Remote { url } => (url.as_str(), url.clone()),
            ImportLocation::Git(loc) => (loc.repo.as_str(), loc.url.clone()),
        };
        let Some((host, port)) = crate::resources::https::host_port(target) else {
            continue;
        };
        let mut ids: Vec<&str> = Vec::new();
        for s in &spec.secrets {
            if !ids.contains(&s.id.as_str()) {
                ids.push(&s.id);
            }
        }
        let needs = match check_secret_hosts(&spec.name, &ids, target, network, Some(&spec.span)) {
            Ok(()) => None,
            Err(e) => match e.needs {
                Some(n) => Some(n),
                None => return Err(e),
            },
        };
        add(
            &host,
            port,
            HostUse {
                kind: "import",
                name: spec.name.clone(),
                source,
                span: Some(spec.span.clone()),
                secrets: ids.iter().map(|s| s.to_string()).collect(),
                nested: spec.as_kind == Some(ImportKind::Sitefile),
                needs,
            },
        );
    }
    let mut image =
        |kind: &'static str, name: String, input: &str, span: Option<SourceSpan>| -> Result<()> {
            let r = with_default_registry(&parse_ref(input)?);
            let registry = r.registry.clone().unwrap_or_default();
            let source = format!(
                "{registry}/{}{}{}",
                r.name,
                r.tag.as_ref().map(|t| format!(":{t}")).unwrap_or_default(),
                r.digest
                    .as_ref()
                    .map(|d| format!("@{d}"))
                    .unwrap_or_default()
            );
            let api = registry_api_host(&registry);
            let (host, port) = match api.rsplit_once(':') {
                Some((h, p)) if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => {
                    (h, p.parse::<u16>().unwrap_or(443))
                }
                _ => (api, 443),
            };
            add(
                host,
                port,
                HostUse {
                    kind,
                    name,
                    source,
                    span,
                    secrets: Vec::new(),
                    nested: false,
                    needs: None,
                },
            );
            Ok(())
        };
    for (i, st) in p.stages.iter().enumerate() {
        if let StageBase::Image {
            reference,
            local: false,
            span,
            ..
        } = &st.base
        {
            image(
                "image",
                st.name.clone().unwrap_or_else(|| format!("#{}", i + 1)),
                reference,
                Some(span.clone()),
            )?;
        }
    }
    let base_input = options.base_image.clone().or_else(|| {
        if p.export_format == ExportFormat::Oci {
            p.export_base.clone()
        } else {
            None
        }
    });
    if let Some(input) = base_input {
        image("base", "BASE".into(), &input, None)?;
    }
    let checklinks_external = p
        .stages
        .iter()
        .flat_map(|st| st.operations.iter())
        .filter_map(|op| match op {
            Operation::CheckLinks {
                external: true,
                span,
                ..
            } => Some(span.line),
            _ => None,
        })
        .collect();
    let hosts = entries
        .into_iter()
        .map(|(host, port, uses)| {
            let status = status_of(&host, port, network);
            HostEntry {
                needs: (status == HostStatus::Blocked).then(|| Needs::host(host.clone())),
                host,
                port,
                status,
                uses,
            }
        })
        .collect();
    Ok(HostsReport {
        strict: network.strict,
        hosts,
        checklinks_external,
    })
}

impl HostsReport {
    /// The `--json` document, keys in the documented order (the same as the TypeScript `hostsJson`).
    pub fn to_json(&self) -> Value {
        let needs = |n: &Option<Needs>| n.as_ref().map(Needs::to_json).unwrap_or(Value::Null);
        let hosts: Vec<Value> = self
            .hosts
            .iter()
            .map(|h| {
                let uses: Vec<Value> = h
                    .uses
                    .iter()
                    .map(|u| {
                        json!({
                            "kind": u.kind,
                            "name": u.name,
                            "source": u.source,
                            "span": u.span.as_ref().map(|s| json!({ "file": s.file, "line": s.line, "column": s.column })).unwrap_or(Value::Null),
                            "secrets": u.secrets,
                            "nested": u.nested,
                            "needs": needs(&u.needs),
                        })
                    })
                    .collect();
                json!({
                    "host": h.host,
                    "port": h.port,
                    "status": h.status.as_str(),
                    "needs": needs(&h.needs),
                    "uses": uses,
                })
            })
            .collect();
        json!({ "strict": self.strict, "hosts": hosts, "checklinksExternal": self.checklinks_external })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(sitefile: &str) -> String {
        let d = std::env::temp_dir().join(format!(
            "sitefile-hosts-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("Sitefile"), sitefile).unwrap();
        d.to_string_lossy().into_owned()
    }

    const SITEFILE: &str = "FROM scratch AS a
IMPORT css FROM https://cdn.example.com/a.css
IMPORT js FROM https://CDN.example.com./b.js SECRET tok AS BEARER SECRET tok AS HEADER X-Tok
IMPORT repo FROM git+https://git.example.org/r.git#main:site
IMPORT theme FROM git+https://themes.test:8443/t.git#main AS sitefile
IMPORT here FROM ./Sitefile AS text
STAGE img FROM image ghcr.io/x/site:1
STAGE mine FROM image site:1
FROM image localhost:5000/a/b:2
CHECKLINKS --external
EXPORT / AS oci BASE nginx:alpine
";

    #[test]
    fn lists_hosts_from_the_plan_with_their_status_and_needs() {
        let dir = context(SITEFILE);
        let r = hosts(&BuildOptions {
            context_dir: dir.clone(),
            ..Default::default()
        })
        .unwrap();
        let rows: Vec<(String, u16, &str, Vec<String>)> = r
            .hosts
            .iter()
            .map(|h| {
                (
                    h.host.clone(),
                    h.port,
                    h.status.as_str(),
                    h.uses
                        .iter()
                        .map(|u| format!("{}:{}", u.kind, u.name))
                        .collect(),
                )
            })
            .collect();
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(
            rows,
            vec![
                (
                    "cdn.example.com".into(),
                    443,
                    "allowed",
                    s(&["import:css", "import:js"])
                ),
                (
                    "git.example.org".into(),
                    443,
                    "allowed",
                    s(&["import:repo"])
                ),
                ("themes.test".into(), 8443, "allowed", s(&["import:theme"])),
                ("ghcr.io".into(), 443, "allowed", s(&["image:img"])),
                ("localhost".into(), 5000, "allowed", s(&["image:#4"])),
                (
                    "registry-1.docker.io".into(),
                    443,
                    "allowed",
                    s(&["base:BASE"])
                ),
            ]
        );
        assert_eq!(r.hosts[0].uses[1].secrets, ["tok"]);
        assert!(r.hosts[2].uses[0].nested);
        assert_eq!(r.checklinks_external, [10]);

        let mut o = BuildOptions {
            context_dir: dir,
            ..Default::default()
        };
        o.fetch_policy.network = NetworkSettings {
            strict: true,
            allow_hosts: vec!["example.com".into(), "ghcr.io".into()],
            deny_hosts: vec!["localhost".into()],
            ..NetworkSettings::NONE
        };
        let r = hosts(&o).unwrap();
        let by = |h: &str| r.hosts.iter().find(|e| e.host == h).unwrap();
        assert_eq!(by("cdn.example.com").status, HostStatus::Allowed);
        assert_eq!(by("git.example.org").status, HostStatus::Blocked);
        assert_eq!(
            by("git.example.org").needs,
            Some(Needs::host("git.example.org"))
        );
        assert_eq!(by("localhost").status, HostStatus::Denied);
        assert_eq!(by("localhost").needs, None);
        assert_eq!(
            by("cdn.example.com").uses[1].needs,
            Some(Needs::secret_host("tok", "cdn.example.com"))
        );
        assert_eq!(by("cdn.example.com").uses[0].needs, None);
        let keys: Vec<String> = r.to_json()["hosts"][0]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        assert_eq!(keys, ["host", "port", "status", "needs", "uses"]);
    }
}

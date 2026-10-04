use std::collections::{BTreeMap, BTreeSet};

use core_lib::{
    CtiEventClass, CtiLogLevel, CtiNodeKind, TaskEventDescriptor, TaskEventDictionary,
};
use sha2::{Digest, Sha256};

use crate::embedded_rel::extract_embedded_rel;
use crate::relc::PhysicalRelSource;
use crate::runtime_image::RuntimeImage;
use crate::source_registry::{RelSourceKind, SourceId};
use crate::SymbolId;

use super::{
    err, ContainerTaskBoundaryHint, ContainerTaskDiscoveryError, ContainerTaskSourceFile,
    ContainerTaskSourceSite,
};

#[derive(Debug, Clone)]
pub(super) struct SourceText {
    path: String,
    source: String,
    first_line: usize,
}

pub(super) type SourceTextCatalog = BTreeMap<(RelSourceKind, String), SourceText>;

pub(super) fn source_texts(
    raw_server: &str,
    physical: &[PhysicalRelSource],
) -> Result<SourceTextCatalog, ContainerTaskDiscoveryError> {
    let mut out = BTreeMap::new();
    for source in physical {
        let key = (source.kind, source.logical_name.clone());
        if out
            .insert(
                key,
                SourceText {
                    path: logical_path(source.kind, &source.logical_name),
                    source: source.source.clone(),
                    first_line: 1,
                },
            )
            .is_some()
        {
            return Err(err(format!(
                "duplicate CTI source {}:{}",
                source.kind, source.logical_name
            )));
        }
    }

    let embedded = extract_embedded_rel(raw_server).map_err(|error| err(error.to_string()))?;
    for source in embedded.embedded {
        let key = (source.kind, source.logical_name.clone());
        if out
            .insert(
                key,
                SourceText {
                    path: format!(
                        "server.server#{}:{}",
                        source.kind.as_str(),
                        source.logical_name
                    ),
                    source: source.source,
                    first_line: source.start_line,
                },
            )
            .is_some()
        {
            return Err(err("duplicate embedded CTI source"));
        }
    }
    Ok(out)
}

pub(super) fn source_provenance_hashes(
    image: &RuntimeImage,
    sources: &BTreeSet<SourceId>,
    texts: &SourceTextCatalog,
) -> Result<BTreeMap<SourceId, String>, ContainerTaskDiscoveryError> {
    let mut out = BTreeMap::new();
    for id in sources {
        let manifest = image
            .source(id)
            .ok_or_else(|| err(format!("missing Runtime Image source {id}")))?;
        let text = texts
            .get(&(manifest.kind, manifest.logical_name.clone()))
            .ok_or_else(|| err(format!("missing source bytes for Container Task dependency {id}")))?;
        out.insert(
            id.clone(),
            hex::encode(Sha256::digest(text.source.as_bytes())),
        );
    }
    Ok(out)
}

pub(super) fn build_source_map(
    image: &RuntimeImage,
    reachable: &BTreeSet<SymbolId>,
    texts: &SourceTextCatalog,
) -> Result<
    (Vec<ContainerTaskSourceFile>, Vec<ContainerTaskSourceSite>),
    ContainerTaskDiscoveryError,
> {
    let mut paths = BTreeSet::new();
    for symbol in reachable {
        if let Some(manifest) = image.source(&symbol.source) {
            if let Some(text) = texts.get(&(manifest.kind, manifest.logical_name.clone())) {
                paths.insert(text.path.clone());
            }
        }
    }

    let mut files = Vec::with_capacity(paths.len());
    for (index, path) in paths.into_iter().enumerate() {
        let file_id = u32::try_from(index)
            .map_err(|_| err("CTI source-file ID space exhausted"))?;
        files.push(ContainerTaskSourceFile { file_id, path });
    }
    let file_ids = files
        .iter()
        .map(|file| (file.path.clone(), file.file_id))
        .collect::<BTreeMap<_, _>>();

    let mut sites = Vec::new();
    for symbol in reachable {
        let manifest = image
            .source(&symbol.source)
            .ok_or_else(|| err(format!("missing Runtime Image source {}", symbol.source)))?;
        let text = texts
            .get(&(manifest.kind, manifest.logical_name.clone()))
            .ok_or_else(|| err(format!("missing CTI source-map text for {}", symbol.source)))?;
        let file_id = *file_ids
            .get(&text.path)
            .ok_or_else(|| err("CTI source-map file ID invariant failed"))?;
        let site_id = u32::try_from(sites.len() + 1)
            .map_err(|_| err("CTI error-site ID space exhausted"))?;
        let (line, column) = locate(&text.source, &symbol.name).unwrap_or((1, 1));
        sites.push(ContainerTaskSourceSite {
            site_id,
            source: symbol.source.clone(),
            symbol: symbol.name.clone(),
            file_id,
            line: u32::try_from(text.first_line.saturating_add(line.saturating_sub(1)))
                .map_err(|_| err("CTI source line does not fit u32"))?,
            column: u32::try_from(column)
                .map_err(|_| err("CTI source column does not fit u32"))?,
        });
    }
    Ok((files, sites))
}

pub(super) fn build_log_dictionary(
    task: &str,
    sites: &[ContainerTaskSourceSite],
    hints: &[ContainerTaskBoundaryHint],
) -> Result<TaskEventDictionary, ContainerTaskDiscoveryError> {
    let mut entries = Vec::new();
    let mut next_id = 1u32;
    let mut push = |class: CtiEventClass,
                    level: CtiLogLevel,
                    symbol: &str,
                    template: &str|
     -> Result<(), ContainerTaskDiscoveryError> {
        if next_id > u16::MAX as u32 {
            return Err(err("CTI log event ID space exhausted"));
        }
        entries.push(TaskEventDescriptor {
            event_id: next_id as u16,
            class,
            level,
            symbol: symbol.into(),
            template: template.into(),
        });
        next_id += 1;
        Ok(())
    };

    push(CtiEventClass::Task, CtiLogLevel::Task, task, "TASK : started {symbol}")?;
    push(
        CtiEventClass::Task,
        CtiLogLevel::Task,
        task,
        "TASK : completed {symbol}",
    )?;
    push(
        CtiEventClass::Error,
        CtiLogLevel::Error,
        task,
        "ER : Task {symbol} failed at site {site}",
    )?;

    for site in sites {
        push(
            CtiEventClass::Function,
            CtiLogLevel::Trace,
            &site.symbol,
            "FNCT : executed {symbol}",
        )?;
        push(
            CtiEventClass::Error,
            CtiLogLevel::Error,
            &site.symbol,
            "ER : Function {symbol} failed at site {site}",
        )?;
    }

    for hint in hints {
        let (class, template) = match hint.node_kind() {
            CtiNodeKind::ServiceCall => (CtiEventClass::Service, "SVC : {symbol} completed"),
            CtiNodeKind::QuickDb => (CtiEventClass::QuickDb, "QDB : {symbol} completed"),
            _ => (CtiEventClass::Capability, "CAP : {symbol} completed"),
        };
        push(class, CtiLogLevel::Normal, &hint.label, template)?;
    }

    TaskEventDictionary::new(entries).map_err(|error| err(error.to_string()))
}

fn logical_path(kind: RelSourceKind, name: &str) -> String {
    match kind {
        RelSourceKind::Route => format!("api/{name}.route"),
        RelSourceKind::Module => format!("module/{name}.module"),
        RelSourceKind::Service => format!("service/{name}.service"),
        RelSourceKind::Field => format!("api/{name}.field"),
        RelSourceKind::Server => "server.server".into(),
    }
}

fn locate(source: &str, symbol: &str) -> Option<(usize, usize)> {
    let leaf = symbol.rsplit('.').next().unwrap_or(symbol);
    let primary = symbol
        .strip_prefix("Route.")
        .map(|verb| format!("{verb}("))
        .or_else(|| {
            symbol
                .strip_prefix("Service.")
                .map(|verb| format!("{verb}("))
        })
        .unwrap_or_else(|| format!("function {leaf}"));
    let fallback = format!("{leaf}(");
    source.lines().enumerate().find_map(|(index, line)| {
        line.find(&primary)
            .or_else(|| line.find(&fallback))
            .map(|column| (index + 1, column + 1))
    })
}

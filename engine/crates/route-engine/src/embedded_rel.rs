//! Literal REL source extraction from `server.server`.
//!
//! Extraction happens before Server REL lexing so embedded Route/Module/Service
//! syntax is never interpreted as Server policy syntax. The cleaned Server REL
//! keeps the same line count for diagnostic remapping.

use std::collections::BTreeMap;
use std::fmt;

use crate::source_registry::RelSourceKind;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddedRelSource {
    pub kind: RelSourceKind,
    pub logical_name: String,
    pub attributes: BTreeMap<String, String>,
    pub block_index: usize,
    pub start_line: usize,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedServerRel {
    pub server_source: String,
    pub embedded: Vec<EmbeddedRelSource>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddedRelError {
    pub line: usize,
    pub message: String,
}

impl fmt::Display for EmbeddedRelError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "embedded REL error at server.server:{}: {}",
            self.line, self.message
        )
    }
}

impl std::error::Error for EmbeddedRelError {}

pub fn extract_embedded_rel(source: &str) -> Result<ExtractedServerRel, EmbeddedRelError> {
    let mut server_source = String::with_capacity(source.len());
    let mut embedded = Vec::new();
    let mut active: Option<ActiveBlock> = None;

    for (index, line) in source.split_inclusive('\n').enumerate() {
        let line_number = index + 1;
        let trimmed = line.trim();

        if trimmed.starts_with("[file-start:") {
            if let Some(active) = &active {
                return Err(error(
                    line_number,
                    format!(
                        "nested [file-start:*] block inside {}.{} is not allowed",
                        active.kind, active.logical_name
                    ),
                ));
            }
            let (kind, logical_name, attributes) = parse_start_marker(trimmed, line_number)?;
            active = Some(ActiveBlock {
                kind,
                logical_name,
                attributes,
                start_line: line_number + 1,
                payload: String::new(),
            });
            preserve_line(&mut server_source, line);
            continue;
        }

        if trimmed.starts_with("[file-end:") {
            let end_kind = parse_end_marker(trimmed, line_number)?;
            let Some(block) = active.take() else {
                return Err(error(
                    line_number,
                    "[file-end:*] without a matching file-start",
                ));
            };
            if block.kind != end_kind {
                return Err(error(
                    line_number,
                    format!(
                        "mismatched embedded REL end marker: started {}, ended {}",
                        block.kind, end_kind
                    ),
                ));
            }
            let block_index = embedded.len();
            embedded.push(EmbeddedRelSource {
                kind: block.kind,
                logical_name: block.logical_name,
                attributes: block.attributes,
                block_index,
                start_line: block.start_line,
                source: block.payload,
            });
            preserve_line(&mut server_source, line);
            continue;
        }

        if let Some(block) = active.as_mut() {
            block.payload.push_str(line);
            preserve_line(&mut server_source, line);
        } else {
            server_source.push_str(line);
        }
    }

    // `split_inclusive` returns no item for an empty source and preserves a final
    // non-newline line, so no second line pass is required here.
    if let Some(block) = active {
        return Err(error(
            block.start_line.saturating_sub(1),
            format!(
                "unterminated embedded {} REL source `{}`",
                block.kind, block.logical_name
            ),
        ));
    }

    Ok(ExtractedServerRel {
        server_source,
        embedded,
    })
}

struct ActiveBlock {
    kind: RelSourceKind,
    logical_name: String,
    attributes: BTreeMap<String, String>,
    start_line: usize,
    payload: String,
}

fn parse_start_marker(
    marker: &str,
    line: usize,
) -> Result<(RelSourceKind, String, BTreeMap<String, String>), EmbeddedRelError> {
    if !marker.ends_with(']') {
        return Err(error(line, "unterminated [file-start:*] marker"));
    }
    let inner = marker
        .strip_prefix("[file-start:")
        .and_then(|value| value.strip_suffix(']'))
        .ok_or_else(|| error(line, "invalid [file-start:*] marker"))?
        .trim();
    let mut pieces = split_header(inner, line)?;
    let identity = pieces
        .next()
        .ok_or_else(|| error(line, "embedded source identity is missing"))?;
    let Some((kind, logical_name)) = identity.split_once('.') else {
        return Err(error(
            line,
            "embedded source must use kind.NAME, for example module.Auth",
        ));
    };
    let kind = parse_kind(kind, line)?;
    if kind == RelSourceKind::Server {
        return Err(error(
            line,
            "server.server cannot embed another Server REL source",
        ));
    }
    let logical_name = logical_name.trim();
    if logical_name.is_empty() {
        return Err(error(line, "embedded REL logical name must not be empty"));
    }

    let mut attributes = BTreeMap::new();
    for piece in pieces {
        let Some((name, value)) = piece.split_once('=') else {
            return Err(error(
                line,
                format!("embedded source attribute `{piece}` must use name=value"),
            ));
        };
        let name = name.trim();
        let value = unquote(value.trim(), line)?;
        if name.is_empty() || value.is_empty() {
            return Err(error(line, "embedded source attributes must not be empty"));
        }
        if attributes.insert(name.to_string(), value).is_some() {
            return Err(error(
                line,
                format!("duplicate embedded source attribute `{name}`"),
            ));
        }
    }

    if kind == RelSourceKind::Route {
        if let Some(path) = attributes.get("path") {
            if !path.starts_with('/') {
                return Err(error(line, "embedded route path must start with `/`"));
            }
        }
    } else if attributes.contains_key("path") {
        return Err(error(
            line,
            "the `path` embedded-source attribute is only valid for Route REL",
        ));
    }

    Ok((kind, logical_name.to_string(), attributes))
}

fn parse_end_marker(marker: &str, line: usize) -> Result<RelSourceKind, EmbeddedRelError> {
    if !marker.ends_with(']') {
        return Err(error(line, "unterminated [file-end:*] marker"));
    }
    let kind = marker
        .strip_prefix("[file-end:")
        .and_then(|value| value.strip_suffix(']'))
        .ok_or_else(|| error(line, "invalid [file-end:*] marker"))?;
    parse_kind(kind.trim(), line)
}

fn parse_kind(kind: &str, line: usize) -> Result<RelSourceKind, EmbeddedRelError> {
    match kind.to_ascii_lowercase().as_str() {
        "route" => Ok(RelSourceKind::Route),
        "module" => Ok(RelSourceKind::Module),
        "service" => Ok(RelSourceKind::Service),
        "server" => Ok(RelSourceKind::Server),
        other => Err(error(
            line,
            format!("unknown embedded REL source kind `{other}`"),
        )),
    }
}

/// Header splitter supporting quoted attribute values without turning this
/// source-container syntax into a second REL lexer.
fn split_header(input: &str, line: usize) -> Result<impl Iterator<Item = &str>, EmbeddedRelError> {
    let mut spans = Vec::new();
    let bytes = input.as_bytes();
    let mut start = None;
    let mut quote = None;
    let mut index = 0usize;
    while index < bytes.len() {
        let byte = bytes[index];
        match quote {
            Some(active) if byte == active => quote = None,
            Some(_) => {}
            None if byte == b'\"' || byte == b'\'' => quote = Some(byte),
            None if byte.is_ascii_whitespace() => {
                if let Some(begin) = start.take() {
                    spans.push(&input[begin..index]);
                }
            }
            None => {
                start.get_or_insert(index);
            }
        }
        index += 1;
    }
    if quote.is_some() {
        return Err(error(line, "unterminated quote in embedded source marker"));
    }
    if let Some(begin) = start {
        spans.push(&input[begin..]);
    }
    Ok(spans.into_iter())
}

fn unquote(value: &str, line: usize) -> Result<String, EmbeddedRelError> {
    if value.len() >= 2 {
        let first = value.as_bytes()[0];
        let last = value.as_bytes()[value.len() - 1];
        if (first == b'\"' && last == b'\"') || (first == b'\'' && last == b'\'') {
            return Ok(value[1..value.len() - 1].to_string());
        }
        if first == b'\"' || first == b'\'' || last == b'\"' || last == b'\'' {
            return Err(error(
                line,
                "mismatched quotes in embedded source attribute",
            ));
        }
    }
    Ok(value.to_string())
}

fn preserve_line(output: &mut String, original: &str) {
    if original.ends_with('\n') {
        output.push('\n');
    } else {
        // Keep a harmless space so a final marker/content line does not erase
        // the source's final line from parser diagnostics.
        output.push(' ');
    }
}

fn error(line: usize, message: impl Into<String>) -> EmbeddedRelError {
    EmbeddedRelError {
        line,
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_multiple_roles_and_preserves_server_line_numbers() {
        let source = r#"server Main {
    status online;
}
[file-start:module.Auth]
export function name() { return "Auth"; }
[file-end:module]
[file-start:route.Health path="/health"]
class Route { get() { return true; } }
[file-end:route]
"#;
        let extracted = extract_embedded_rel(source).unwrap();
        assert_eq!(extracted.embedded.len(), 2);
        assert_eq!(extracted.embedded[0].kind, RelSourceKind::Module);
        assert_eq!(extracted.embedded[0].logical_name, "Auth");
        assert_eq!(extracted.embedded[1].attributes["path"], "/health");
        assert_eq!(
            extracted.server_source.lines().count(),
            source.lines().count()
        );
    }

    #[test]
    fn rejects_nested_and_mismatched_blocks() {
        let nested = "[file-start:module.A]\n[file-start:module.B]\n";
        assert!(extract_embedded_rel(nested).is_err());
        let mismatched = "[file-start:module.A]\nx\n[file-end:service]\n";
        assert!(extract_embedded_rel(mismatched).is_err());
    }
}

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::lexer::Lexer;
use crate::parser::Parser;

const RESERVED_NATIVE_API_PREFIXES: &[&str] = &[
    "/api/account",
    "/api/admin",
    "/api/auth",
    "/api/broadcast",
    "/api/contact",
    "/api/maintenance",
    "/api/streaming",
];

#[derive(Debug, Clone, PartialEq, Eq)]
struct RouteCollision {
    path: PathBuf,
    message: String,
}

pub(crate) fn validate(api_dir: &Path) -> anyhow::Result<()> {
    let collisions = find_collisions(api_dir)?;
    if collisions.is_empty() {
        return Ok(());
    }

    let error_path = PathBuf::from("data")
        .join("admin")
        .join("compiler-error.txt");
    if let Some(parent) = error_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut report = String::new();
    for collision in &collisions {
        report.push_str(&format!(
            "E3013: {}: {}\n",
            collision.path.display(),
            collision.message
        ));
    }
    fs::write(&error_path, report)?;

    Err(anyhow::anyhow!(
        "route compiler found {} route collision(s); see {}",
        collisions.len(),
        error_path.display()
    ))
}

fn find_collisions(api_dir: &Path) -> anyhow::Result<Vec<RouteCollision>> {
    let mut files = Vec::new();
    collect_route_files(api_dir, &mut files)?;
    files.sort();

    let mut owners: HashMap<(String, String), PathBuf> = HashMap::new();
    let mut collisions = Vec::new();

    for path in files {
        // Syntax/read failures deliberately remain the route compiler's job so
        // this preflight does not replace the richer compiler diagnostics.
        let source = match fs::read_to_string(&path) {
            Ok(source) => source,
            Err(_) => continue,
        };
        let tokens = match Lexer::new(&source).tokenize() {
            Ok(tokens) => tokens,
            Err(_) => continue,
        };
        let file = match Parser::new(tokens).parse_file() {
            Ok(file) => file,
            Err(_) => continue,
        };
        let url_path = url_path_for(api_dir, &path);

        if let Some(prefix) = RESERVED_NATIVE_API_PREFIXES
            .iter()
            .find(|prefix| is_in_native_namespace(&url_path, prefix))
        {
            collisions.push(RouteCollision {
                path: path.clone(),
                message: format!(
                    "route URL `{url_path}` conflicts with native API namespace `{prefix}`"
                ),
            });
            continue;
        }

        for method in file.methods {
            let verb = method.verb.to_ascii_lowercase();
            let key = (url_path.clone(), verb.clone());
            if let Some(existing) = owners.get(&key) {
                collisions.push(RouteCollision {
                    path: path.clone(),
                    message: format!(
                        "route {} `{}` conflicts with {}",
                        verb.to_ascii_uppercase(),
                        url_path,
                        existing.display()
                    ),
                });
            } else {
                owners.insert(key, path.clone());
            }
        }
    }

    Ok(collisions)
}

fn collect_route_files(dir: &Path, out: &mut Vec<PathBuf>) -> anyhow::Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_route_files(&path, out)?;
        } else if path.extension().and_then(|extension| extension.to_str()) == Some("route") {
            out.push(path);
        }
    }
    Ok(())
}

fn url_path_for(api_dir: &Path, file_path: &Path) -> String {
    let relative = file_path.strip_prefix(api_dir).unwrap_or(file_path);
    let without_ext = relative.with_extension("");
    let mut segments: Vec<String> = without_ext
        .components()
        .map(|component| component.as_os_str().to_string_lossy().to_string())
        .collect();
    if segments.last().is_some_and(|segment| segment == "index") {
        segments.pop();
    }
    format!("/api/{}", segments.join("/"))
}

fn is_in_native_namespace(url_path: &str, prefix: &str) -> bool {
    url_path == prefix
        || url_path
            .strip_prefix(prefix)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_api_dir() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock before unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("rbe-route-collision-{nonce}"))
    }

    #[test]
    fn catches_index_and_sibling_method_collision() {
        let root = temp_api_dir();
        fs::create_dir_all(root.join("foo")).unwrap();
        fs::write(
            root.join("foo.route"),
            "class Route { get(req) { return true; } }",
        )
        .unwrap();
        fs::write(
            root.join("foo/index.route"),
            "class Route { get(req) { return true; } }",
        )
        .unwrap();

        let collisions = find_collisions(&root).unwrap();
        assert_eq!(collisions.len(), 1);
        assert!(collisions[0].message.contains("GET `/api/foo`"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn catches_native_api_namespace_collision() {
        let root = temp_api_dir();
        fs::create_dir_all(root.join("auth")).unwrap();
        fs::write(
            root.join("auth/login.route"),
            "class Route { post(req) { return true; } }",
        )
        .unwrap();

        let collisions = find_collisions(&root).unwrap();
        assert_eq!(collisions.len(), 1);
        assert!(collisions[0].message.contains("native API namespace `/api/auth`"));
        let _ = fs::remove_dir_all(root);
    }
}

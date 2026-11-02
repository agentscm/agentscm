//! Revision-bound navigation from static source references, never a runtime call graph.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use super::{
    GitEntryKindV1, GitExplorer, RawTreeEntry, deadline, normalize_optional_path, parse_tree,
};
use crate::Result;

const MAX_FILES: usize = 384;
const MAX_READS: usize = 128;
const MAX_FILE_BYTES: u64 = 64 * 1024;
const MAX_READ_BYTES: u64 = 4 * 1024 * 1024;
const MAX_RELATIONS: usize = 1024;
const MAX_UNRESOLVED: usize = 128;

#[derive(Debug, Serialize)]
pub struct RepositoryMap {
    pub version: u32,
    pub repository_id: String,
    pub scope: String,
    pub extractor: &'static str,
    pub head: Snapshot,
    pub base: Option<Snapshot>,
}

#[derive(Debug, Serialize)]
pub struct Snapshot {
    pub commit: String,
    pub tree: String,
    pub files: Vec<MapFile>,
    pub relations: Vec<Relation>,
    pub unresolved: Vec<Unresolved>,
    pub coverage: Coverage,
}

#[derive(Debug, Serialize)]
pub struct MapFile {
    pub path: String,
    pub oid: String,
    pub kind: GitEntryKindV1,
    pub inspection: &'static str,
}

#[derive(Debug, Serialize)]
pub struct Relation {
    pub from: usize,
    pub to: usize,
    pub line: usize,
    pub kind: &'static str,
}

#[derive(Debug, Serialize)]
pub struct Unresolved {
    pub file: usize,
    pub line: usize,
    pub reference: String,
    pub reason: &'static str,
}

#[derive(Debug, Serialize)]
pub struct Coverage {
    pub discovered_files: usize,
    pub tree_complete: bool,
    pub omitted_files: usize,
    pub inspected_files: usize,
    pub omitted_relations: usize,
    pub omitted_unresolved: usize,
}

impl GitExplorer {
    /// Both sides share one deadline. Uses committed blobs only; dirty content is not read.
    pub fn repository_map(
        &self,
        commit: &str,
        base: Option<&str>,
        scope: Option<&str>,
    ) -> Result<RepositoryMap> {
        let scope = normalize_optional_path(scope)?.unwrap_or_default();
        let until = deadline();
        // Resolve both source identities before reading either side's contents.
        let tree = self.commit_tree(commit, until)?;
        let base_tree = base.map(|id| self.commit_tree(id, until)).transpose()?;
        let head = self.map_snapshot(commit, tree, &scope, until)?;
        let base = match (base, base_tree) {
            (Some(id), Some(tree)) => Some(self.map_snapshot(id, tree, &scope, until)?),
            _ => None,
        };
        Ok(RepositoryMap {
            version: 1,
            repository_id: self.repository_id().to_owned(),
            scope,
            extractor: "static-reference-candidates-v1",
            head,
            base,
        })
    }

    fn map_snapshot(
        &self,
        commit: &str,
        tree: String,
        scope: &str,
        until: std::time::Instant,
    ) -> Result<Snapshot> {
        let args = vec!["ls-tree", "-r", "-z", "-l", "--full-tree", &tree];
        let output = self.run_complete(&args, None, 8 * 1024 * 1024, until, &[0])?;
        let entries = parse_tree(&output.stdout, self.object_format)?;
        let all_names = entries.iter().map(|e| e.path.clone()).collect::<Vec<_>>();
        let all_paths = all_names
            .iter()
            .enumerate()
            .map(|(i, p)| (p.clone(), i))
            .collect::<BTreeMap<_, _>>();
        let prefix = format!("{scope}/");
        let entries = entries
            .into_iter()
            .filter(|e| scope.is_empty() || e.path == scope || e.path.starts_with(&prefix))
            .collect::<Vec<_>>();
        let discovered_files = entries.len();
        // Round-robin directories so a large first directory cannot hide every other component.
        let entries = select_files(entries);
        let paths = entries
            .iter()
            .enumerate()
            .map(|(i, e)| (e.path.clone(), i))
            .collect::<BTreeMap<_, _>>();
        let mut snapshot = Snapshot {
            commit: commit.to_owned(),
            tree,
            files: Vec::new(),
            relations: Vec::new(),
            unresolved: Vec::new(),
            coverage: Coverage {
                discovered_files,
                tree_complete: !output.output_limited,
                omitted_files: discovered_files.saturating_sub(entries.len()),
                inspected_files: 0,
                omitted_relations: 0,
                omitted_unresolved: 0,
            },
        };
        let mut read_bytes = 0;
        for (index, entry) in entries.iter().enumerate() {
            let language = language(&entry.path);
            let mut file = MapFile {
                path: entry.path.clone(),
                oid: entry.oid.clone(),
                kind: entry.kind,
                inspection: "unsupported",
            };
            if matches!(
                entry.kind,
                GitEntryKindV1::File | GitEntryKindV1::Executable
            ) && let Some(language) = language
            {
                let size = entry.size.unwrap_or(u64::MAX);
                file.inspection = if size > MAX_FILE_BYTES {
                    "too_large"
                } else if snapshot.coverage.inspected_files >= MAX_READS
                    || read_bytes + size > MAX_READ_BYTES
                {
                    "not_inspected"
                } else {
                    read_bytes += size;
                    snapshot.coverage.inspected_files += 1;
                    match self.blob(&entry.oid, until)? {
                        None => "unavailable",
                        Some(bytes) if bytes.contains(&0) => "binary",
                        Some(bytes) => match std::str::from_utf8(&bytes) {
                            Err(_) => "binary",
                            Ok(source) => {
                                for reference in references(source, language) {
                                    let candidates = resolve(&entry.path, &reference, &all_paths);
                                    let resolved = candidates
                                        .first()
                                        .and_then(|i| paths.get(&all_names[*i]))
                                        .copied();
                                    if candidates.len() == 1
                                        && let Some(resolved) = resolved
                                        && !output.output_limited
                                    {
                                        if snapshot.relations.len() < MAX_RELATIONS {
                                            snapshot.relations.push(Relation {
                                                from: index,
                                                to: resolved,
                                                line: reference.line,
                                                kind: reference.kind,
                                            });
                                        } else {
                                            snapshot.coverage.omitted_relations += 1;
                                        }
                                    } else if snapshot.unresolved.len() < MAX_UNRESOLVED {
                                        let reason = if output.output_limited {
                                            "incomplete_tree"
                                        } else if matches!(
                                            reference.kind,
                                            "unsupported_module" | "unsupported_use"
                                        ) {
                                            "unsupported_syntax"
                                        } else if candidates.len() > 1 {
                                            "ambiguous"
                                        } else if candidates.len() == 1 {
                                            "outside_view"
                                        } else if reference.kind == "import_candidate"
                                            && !reference.target.starts_with("./")
                                            && !reference.target.starts_with("../")
                                        {
                                            "external_or_alias"
                                        } else {
                                            "unresolved"
                                        };
                                        snapshot.unresolved.push(Unresolved {
                                            file: index,
                                            line: reference.line,
                                            reference: reference.target.chars().take(256).collect(),
                                            reason,
                                        });
                                    } else {
                                        snapshot.coverage.omitted_unresolved += 1;
                                    }
                                }
                                "inspected"
                            }
                        },
                    }
                };
            }
            snapshot.files.push(file);
        }
        Ok(snapshot)
    }
}

fn select_files(entries: Vec<RawTreeEntry>) -> Vec<RawTreeEntry> {
    let mut groups: BTreeMap<String, std::collections::VecDeque<RawTreeEntry>> = BTreeMap::new();
    for entry in entries {
        let group = entry.path.rsplit_once('/').map_or("", |(parent, _)| parent);
        groups.entry(group.to_owned()).or_default().push_back(entry);
    }
    let mut selected = Vec::new();
    while selected.len() < MAX_FILES {
        let before = selected.len();
        for group in groups.values_mut() {
            if let Some(entry) = group.pop_front() {
                selected.push(entry);
            }
            if selected.len() == MAX_FILES {
                break;
            }
        }
        if before == selected.len() {
            break;
        }
    }
    selected.sort_by(|a, b| a.path.cmp(&b.path));
    selected
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Language {
    Rust,
    Javascript,
}

fn language(path: &str) -> Option<Language> {
    match path.rsplit('.').next()? {
        "rs" => Some(Language::Rust),
        "js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs" | "mts" | "cts" => Some(Language::Javascript),
        _ => None,
    }
}

struct Reference {
    target: String,
    line: usize,
    kind: &'static str,
}

#[derive(Debug)]
struct Token {
    text: String,
    line: usize,
    quoted: bool,
}

// A bounded lexical scan, not a language parser. Comments and quoted/template bodies are skipped.
fn tokens(source: &str, language: Language) -> Vec<Token> {
    let bytes = source.as_bytes();
    let (mut i, mut line) = (0, 1);
    let mut result = Vec::new();
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'\n' {
            line += 1;
            i += 1;
            continue;
        }
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if bytes[i..].starts_with(b"//") {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if bytes[i..].starts_with(b"/*") {
            i += 2;
            let mut depth = 1;
            while i < bytes.len() && depth > 0 {
                if bytes[i..].starts_with(b"/*") {
                    depth += 1;
                    i += 2;
                } else if bytes[i..].starts_with(b"*/") {
                    depth -= 1;
                    i += 2;
                } else {
                    if bytes[i] == b'\n' {
                        line += 1;
                    }
                    i += 1;
                }
            }
            continue;
        }
        if language == Language::Rust
            && (c == b'r' || (matches!(c, b'b' | b'c') && bytes.get(i + 1) == Some(&b'r')))
        {
            let raw_start = if c == b'r' { i } else { i + 1 };
            let mut quote = raw_start + 1;
            while quote < bytes.len() && bytes[quote] == b'#' {
                quote += 1;
            }
            if bytes.get(quote) == Some(&b'"') {
                let hashes = quote - raw_start - 1;
                i = quote + 1;
                while i < bytes.len() {
                    if bytes[i] == b'\n' {
                        line += 1;
                    }
                    if bytes[i] == b'"'
                        && bytes
                            .get(i + 1..i + 1 + hashes)
                            .is_some_and(|tail| tail.iter().all(|b| *b == b'#'))
                    {
                        i += hashes + 1;
                        break;
                    }
                    i += 1;
                }
                result.push(Token {
                    text: String::new(),
                    line,
                    quoted: true,
                });
                continue;
            }
        }
        // A lifetime does not open a quoted character literal.
        if language == Language::Rust
            && c == b'\''
            && bytes
                .get(i + 1)
                .is_some_and(|b| b.is_ascii_alphabetic() || *b == b'_')
            && bytes.get(i + 2) != Some(&b'\'')
        {
            i += 1;
            continue;
        }
        if matches!(c, b'\'' | b'"' | b'`') {
            let start_line = line;
            let start = i + 1;
            i += 1;
            let mut escaped = false;
            while i < bytes.len() && bytes[i] != c {
                if bytes[i] == b'\\' {
                    escaped = true;
                    i += 1;
                }
                if i < bytes.len() && bytes[i] == b'\n' {
                    line += 1;
                }
                i += 1;
            }
            if i < bytes.len() && !escaped && c != b'`' && line == start_line {
                result.push(Token {
                    text: source[start..i].to_owned(),
                    line: start_line,
                    quoted: true,
                });
            } else {
                result.push(Token {
                    text: String::new(),
                    line: start_line,
                    quoted: true,
                });
            }
            i = (i + 1).min(bytes.len());
            continue;
        }
        let start = i;
        if c.is_ascii_alphanumeric() || c == b'_' || c == b'$' {
            while i < bytes.len()
                && (bytes[i].is_ascii_alphanumeric() || matches!(bytes[i], b'_' | b'$'))
            {
                i += 1;
            }
        } else {
            i += 1;
        }
        // Non-ASCII bytes are separators; never slice inside their UTF-8 encoding.
        let text = String::from_utf8_lossy(&bytes[start..i]).into_owned();
        result.push(Token {
            text,
            line,
            quoted: false,
        });
    }
    result
}

fn references(source: &str, language: Language) -> Vec<Reference> {
    let tokens = tokens(source, language);
    let mut result = Vec::new();
    // Include path inside cfg_attr without evaluating its condition. One pass avoids
    // repeatedly scanning nested or malformed attributes from untrusted source.
    let mut attribute_depth = 0usize;
    let mut has_path_attribute = false;
    for (i, token) in tokens.iter().enumerate() {
        if token.quoted {
            continue;
        }
        if token.text == "[" && (attribute_depth > 0 || i > 0 && tokens[i - 1].text == "#") {
            attribute_depth += 1;
        } else if attribute_depth > 0 {
            if token.text == "]" {
                attribute_depth -= 1;
            } else if token.text == "path" {
                has_path_attribute = true;
            }
        }
    }
    let mut skip_through = 0;
    for (i, token) in tokens.iter().enumerate() {
        if token.quoted || i < skip_through {
            continue;
        }
        let rest = &tokens[i + 1..];
        if language == Language::Rust
            && token.text == "mod"
            && rest.get(1).is_some_and(|t| t.text == "{")
        {
            let mut depth = 1;
            skip_through = tokens.len();
            for (offset, t) in tokens[i + 3..].iter().enumerate() {
                if t.quoted {
                    continue;
                }
                if t.text == "{" {
                    depth += 1;
                }
                if t.text == "}" {
                    depth -= 1;
                }
                if depth == 0 {
                    skip_through = i + 4 + offset;
                    break;
                }
            }
            continue;
        }
        if language == Language::Javascript && matches!(token.text.as_str(), "import" | "export") {
            if i > 0
                && tokens[i - 1].line == token.line
                && ![";", "}"].contains(&tokens[i - 1].text.as_str())
            {
                continue;
            }
            // Only re-export declarations can carry a static `from` clause.
            let export_start = if rest.first().is_some_and(|t| t.text == "type") {
                rest.get(1)
            } else {
                rest.first()
            };
            if token.text == "export"
                && !export_start.is_some_and(|t| matches!(t.text.as_str(), "{" | "*"))
            {
                continue;
            }
            if token.text == "import"
                && rest.first().is_some_and(|t| t.quoted && !t.text.is_empty())
            {
                result.push(Reference {
                    target: rest[0].text.clone(),
                    line: token.line,
                    kind: "import_candidate",
                });
                continue;
            }
            if rest.first().is_some_and(|t| t.text == "(" || t.text == ".") {
                continue;
            }
            for pair in rest
                .iter()
                .take(128)
                .take_while(|t| {
                    t.line <= token.line + 16
                        && t.text != ";"
                        && (t.quoted || !matches!(t.text.as_str(), "export" | "import"))
                })
                .collect::<Vec<_>>()
                .windows(2)
            {
                if !pair[0].quoted
                    && pair[0].text == "from"
                    && pair[1].quoted
                    && !pair[1].text.is_empty()
                {
                    result.push(Reference {
                        target: pair[1].text.clone(),
                        line: token.line,
                        kind: "import_candidate",
                    });
                    break;
                }
            }
        }
        if language == Language::Rust
            && token.text == "mod"
            && rest.len() >= 2
            && rest[1].text == ";"
            && !rest[0].quoted
        {
            result.push(Reference {
                target: rest[0].text.clone(),
                line: token.line,
                kind: if has_path_attribute {
                    "unsupported_module"
                } else {
                    "module_candidate"
                },
            });
        }
        if language == Language::Rust && token.text == "use" {
            let target = rest
                .iter()
                .take(64)
                .take_while(|t| {
                    !t.quoted
                        && (t.text == ":"
                            || t.text
                                .bytes()
                                .all(|b| b.is_ascii_alphanumeric() || b == b'_'))
                        && t.text != "as"
                })
                .map(|t| t.text.as_str())
                .collect::<String>();
            if target.starts_with("crate::")
                || target.starts_with("self::")
                || target.starts_with("super::")
            {
                result.push(Reference {
                    target: target.trim_end_matches(':').to_owned(),
                    line: token.line,
                    kind: if has_path_attribute {
                        "unsupported_use"
                    } else {
                        "use_candidate"
                    },
                });
            }
        }
    }
    result
}

fn normalize(parent: &str, relative: &str) -> Option<String> {
    let joined = format!("{parent}/{relative}");
    let mut parts = Vec::new();
    for part in joined.split('/') {
        match part {
            "" | "." => (),
            ".." => {
                parts.pop()?;
            }
            other => parts.push(other),
        }
    }
    Some(parts.join("/"))
}

fn resolve(path: &str, reference: &Reference, paths: &BTreeMap<String, usize>) -> Vec<usize> {
    if matches!(reference.kind, "unsupported_module" | "unsupported_use") {
        return Vec::new();
    }
    let parent = path.rsplit_once('/').map_or("", |(parent, _)| parent);
    let mut candidates = BTreeSet::new();
    fn add(candidates: &mut BTreeSet<usize>, paths: &BTreeMap<String, usize>, candidate: String) {
        if let Some(index) = paths.get(&candidate) {
            candidates.insert(*index);
        }
    }
    if reference.kind == "import_candidate" {
        if !reference.target.starts_with("./") && !reference.target.starts_with("../") {
            return Vec::new();
        }
        let Some(stem) = normalize(parent, &reference.target) else {
            return Vec::new();
        };
        add(&mut candidates, paths, stem.clone());
        for ext in ["ts", "tsx", "js", "jsx", "mjs", "cjs", "mts", "cts"] {
            add(&mut candidates, paths, format!("{stem}.{ext}"));
            add(&mut candidates, paths, format!("{stem}/index.{ext}"));
        }
        if let Some(stem) = stem.strip_suffix(".js") {
            add(&mut candidates, paths, format!("{stem}.ts"));
            add(&mut candidates, paths, format!("{stem}.tsx"));
        }
    } else {
        // Binary targets can have separate module roots. All Rust resolution here
        // needs manifest context; guessing from the filename can select unrelated files.
        if path.starts_with("src/bin/") || path.contains("/src/bin/") {
            return Vec::new();
        }
        let file_stem = path.strip_suffix(".rs").unwrap_or(path);
        let module_dir =
            if ["mod.rs", "lib.rs", "main.rs"].contains(&path.rsplit('/').next().unwrap_or("")) {
                parent
            } else {
                file_stem
            };
        let target = if reference.kind == "module_candidate" {
            normalize(module_dir, &reference.target)
        } else if let Some(rest) = reference.target.strip_prefix("crate::") {
            // Rust crate-relative roots are inferred from a conventional src directory only.
            let root = path
                .rfind("/src/")
                .map(|i| &path[..i + 4])
                .or_else(|| path.starts_with("src/").then_some("src"));
            root.and_then(|root| normalize(root, &rest.replace("::", "/")))
        } else if let Some(rest) = reference.target.strip_prefix("self::") {
            normalize(module_dir, &rest.replace("::", "/"))
        } else if let Some(rest) = reference.target.strip_prefix("super::") {
            normalize(module_dir, &format!("../{}", rest.replace("::", "/")))
        } else {
            None
        };
        if let Some(mut target) = target {
            loop {
                add(&mut candidates, paths, format!("{target}.rs"));
                add(&mut candidates, paths, format!("{target}/mod.rs"));
                if reference.kind == "module_candidate" || !candidates.is_empty() {
                    break;
                }
                let Some((prefix, _)) = target.rsplit_once('/') else {
                    break;
                };
                target = prefix.to_owned();
            }
        }
    }
    candidates.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lexical_candidates_ignore_comment_template_and_raw_string_decoys() {
        let javascript = "/* import './comment'; */\nconst text = `import './template';`;\n// export { x } from './hidden';\nimport {\n value,\n} from './actual';\nexport { x } from './exported';\nimport('./dynamic');\n";
        let refs = references(javascript, Language::Javascript);
        assert_eq!(
            refs.iter().map(|r| r.target.as_str()).collect::<Vec<_>>(),
            ["./actual", "./exported"]
        );
        assert_eq!(refs[0].line, 4);
        let rust = "const TEXT: &str = r###\"use crate::hidden; mod decoy;\"###;\nfn borrow<'a>(x: &'a str) {}\nuse crate::actual::Value;\nmod child;\nmod inline { mod nested; use super::not_at_root; }\n";
        let refs = references(rust, Language::Rust);
        assert_eq!(
            refs.iter().map(|r| r.target.as_str()).collect::<Vec<_>>(),
            ["crate::actual::Value", "child"]
        );
    }

    #[test]
    fn raw_prefixed_strings_and_semicolonless_exports_do_not_invent_edges() {
        let source = r####"const BODY: &[u8] = br#" " use crate::hidden::Value; "#;
const C: &CStr = cr##" " mod decoy; "##;
use crate::actual::Value;"####;
        let refs = references(source, Language::Rust);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].target, "crate::actual::Value");
        let refs = references(
            "export const version = 1\nexport { api } from '../server/api'",
            Language::Javascript,
        );
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].line, 2);
        let paths = BTreeMap::from([("src/shared.rs".into(), 0), ("src/bin/shared.rs".into(), 1)]);
        let refs = references("use crate::shared::Value;", Language::Rust);
        assert!(resolve("src/bin/tool.rs", &refs[0], &paths).is_empty());
        let paths = BTreeMap::from([
            ("src/bin/shared.rs".into(), 0),
            ("src/bin/tool/shared.rs".into(), 1),
        ]);
        for reference in references("mod shared; use self::shared::Value;", Language::Rust) {
            assert!(resolve("src/bin/tool.rs", &reference, &paths).is_empty());
        }
    }

    #[test]
    fn ambiguous_resolution_and_path_attributes_never_pick_a_file() {
        let paths = BTreeMap::from([("ui/target.ts".into(), 0), ("ui/target/index.ts".into(), 1)]);
        let reference = Reference {
            target: "./target".into(),
            line: 1,
            kind: "import_candidate",
        };
        assert_eq!(resolve("ui/main.ts", &reference, &paths), [0, 1]);
        assert!(normalize("ui", "../../outside").is_none());
        let bare = Reference {
            target: ".package".into(),
            line: 1,
            kind: "import_candidate",
        };
        assert!(
            resolve(
                "ui/main.ts",
                &bare,
                &BTreeMap::from([("ui/.package.ts".into(), 0)])
            )
            .is_empty()
        );
        let conditional = references(
            "#[cfg_attr(feature = \"alternate\", path = \"alternate.rs\")] mod named;",
            Language::Rust,
        );
        assert_eq!(conditional[0].kind, "unsupported_module");
        assert!(
            resolve(
                "src/lib.rs",
                &conditional[0],
                &BTreeMap::from([("src/named.rs".into(), 0)])
            )
            .is_empty()
        );
        for attribute in [
            "#[path = \"alternate.rs\"]",
            "#[cfg_attr(feature = \"alternate\", path = \"alternate.rs\")]",
        ] {
            let source = format!(
                "{attribute} mod named; use self::named::Value; use crate::named::Value; use super::named::Value;"
            );
            let paths = BTreeMap::from([("src/named.rs".into(), 0)]);
            for reference in references(&source, Language::Rust) {
                assert!(matches!(
                    reference.kind,
                    "unsupported_module" | "unsupported_use"
                ));
                assert!(resolve("src/lib.rs", &reference, &paths).is_empty());
            }
        }
        let refs = references("#[path = \"elsewhere.rs\"]\nmod named;", Language::Rust);
        assert_eq!(refs[0].kind, "unsupported_module");
        assert!(
            resolve(
                "src/lib.rs",
                &refs[0],
                &BTreeMap::from([("src/named.rs".into(), 0)])
            )
            .is_empty()
        );
    }
}

//! Bounded, language-aware code snapshotting. Parsing stays in the tool layer;
//! durable generation publication stays in `builder-core`.
use anyhow::{Context, Result, ensure};
use builder_core::{
    code_index::{CodeChunk, CodeIndexFile, CodeIndexSnapshot},
    config::PipelineSettings,
    memory::digest,
};
use ignore::WalkBuilder;
use std::{collections::BTreeSet, io::Read, path::Path};

use crate::Workspace;

// Bump when extraction changes so unchanged checkouts rebuild their artifacts.
const SNAPSHOT_FORMAT_VERSION: u32 = 2;
const CHUNK_LINES: usize = 120;
const CHUNK_BYTES: usize = 7_000;
const CHUNK_OVERLAP: usize = 12;

pub fn capture(workspace: &Workspace, settings: &PipelineSettings) -> Result<CodeIndexSnapshot> {
    settings.validate()?;
    let mut files = Vec::new();
    let mut chunks = Vec::new();
    let mut source_bytes = 0usize;
    let mut skipped = 0usize;
    let walker = WalkBuilder::new(workspace.root())
        .hidden(false)
        .require_git(false)
        .follow_links(false)
        .filter_entry(|entry| {
            !matches!(
                entry.file_name().to_str(),
                Some(
                    ".git"
                        | ".builder"
                        | "target"
                        | "node_modules"
                        | "vendor"
                        | "dist"
                        | "build"
                        | "__pycache__"
                )
            )
        })
        .build();
    for entry in walker {
        let entry = entry?;
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let relative = entry.path().strip_prefix(workspace.root())?;
        let Some(language) = language(relative) else {
            continue;
        };
        ensure!(
            files.len() < settings.code_index_max_files,
            "Code index exceeds configured file limit; previous generation remains active"
        );
        let size = entry.metadata()?.len() as usize;
        if size > settings.code_index_max_file_bytes {
            skipped += 1;
            continue;
        }
        ensure!(
            source_bytes.saturating_add(size) <= settings.code_index_max_bytes,
            "Code index exceeds configured total byte limit; previous generation remains active"
        );
        let path = relative
            .to_str()
            .context("Non-UTF8 source path")?
            .to_owned();
        let mut bytes = Vec::with_capacity(size);
        std::fs::File::open(workspace.resolve(&path)?)?
            .take(settings.code_index_max_file_bytes as u64 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > settings.code_index_max_file_bytes {
            skipped += 1;
            continue;
        }
        let Ok(content) = String::from_utf8(bytes) else {
            skipped += 1;
            continue;
        };
        if content.lines().any(|line| line.len() > CHUNK_BYTES) {
            skipped += 1;
            continue;
        }
        let file_hash = digest(content.as_bytes());
        let mut file_chunks = chunk_file(&path, language, &file_hash, &content)?;
        ensure!(
            chunks.len().saturating_add(file_chunks.len()) <= settings.code_index_max_chunks,
            "Code index exceeds configured chunk limit; previous generation remains active"
        );
        source_bytes += content.len();
        files.push(CodeIndexFile {
            path,
            hash: file_hash,
            source_bytes: content.len(),
            language: language.into(),
        });
        chunks.append(&mut file_chunks);
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    chunks.sort_by(|a, b| (&a.path, a.start_line).cmp(&(&b.path, b.start_line)));
    let manifest = files
        .iter()
        .map(|file| (&file.path, &file.hash, file.source_bytes, &file.language))
        .collect::<Vec<_>>();
    Ok(CodeIndexSnapshot {
        snapshot_hash: digest(&serde_json::to_vec(&(SNAPSHOT_FORMAT_VERSION, &manifest))?),
        files,
        chunks,
        source_bytes,
        skipped,
    })
}

fn chunk_file(
    path: &str,
    language: &str,
    file_hash: &str,
    content: &str,
) -> Result<Vec<CodeChunk>> {
    if content.is_empty() {
        return Ok(Vec::new());
    }
    let lines = content.lines().collect::<Vec<_>>();
    let mut declarations = syntax_declarations(language, content).unwrap_or_else(|| {
        lines
            .iter()
            .enumerate()
            .filter_map(|(line, text)| declared_symbol(language, text).map(|symbol| (line, symbol)))
            .collect::<Vec<_>>()
    });
    // Keep leading documentation and attributes with the declaration they describe.
    for (line, _) in &mut declarations {
        while *line > 0 {
            let previous = lines[*line - 1].trim();
            if previous.starts_with("//") || previous.starts_with("#[") {
                *line -= 1;
            } else {
                break;
            }
        }
    }
    let mut boundaries = vec![0usize];
    boundaries.extend(
        declarations
            .iter()
            .map(|(line, _)| *line)
            .filter(|line| *line > 0),
    );
    boundaries.sort_unstable();
    boundaries.dedup();
    boundaries.push(lines.len());
    let mut chunks = Vec::new();
    for window in boundaries.windows(2) {
        let section_start = window[0];
        let section_end = window[1];
        let mut start = section_start;
        while start < section_end {
            let mut end = start;
            let mut bytes = 0usize;
            while end < section_end && end - start < CHUNK_LINES {
                let next = lines[end].len() + usize::from(end + 1 < lines.len());
                if end > start && bytes + next > CHUNK_BYTES {
                    break;
                }
                bytes += next;
                end += 1;
            }
            ensure!(end > start, "Unable to create bounded code chunk");
            let text = lines[start..end].join("\n");
            if !text.trim().is_empty() {
                let symbols = declarations
                    .iter()
                    .filter(|(line, _)| *line >= start && *line < end)
                    .map(|(_, symbol)| symbol.clone())
                    .collect::<Vec<_>>();
                let references = identifiers(&text, &symbols);
                let content_hash = digest(
                    serde_json::to_string(&(path, start + 1, language, &symbols, &text))?
                        .as_bytes(),
                );
                let id =
                    digest(serde_json::to_string(&(path, start + 1, end, file_hash))?.as_bytes());
                chunks.push(CodeChunk {
                    id,
                    path: path.into(),
                    file_hash: file_hash.into(),
                    content_hash,
                    language: language.into(),
                    kind: if symbols.is_empty() {
                        "region"
                    } else {
                        "declaration"
                    }
                    .into(),
                    start_line: start + 1,
                    end_line: end,
                    symbols,
                    references,
                    content: text,
                });
            }
            if end == section_end {
                break;
            }
            start = end.saturating_sub(CHUNK_OVERLAP).max(start + 1);
        }
    }
    Ok(chunks)
}

/// Navigable declarations with 1-based lines. Grammar-backed where
/// available; JavaScript and TypeScript also list functions bound to
/// variables, which is how most components and hooks are written.
pub(crate) fn outline(path: &Path, content: &str) -> Vec<(usize, String)> {
    let Some(language) = language(path) else {
        return Vec::new();
    };
    let mut entries = syntax_declarations(language, content).unwrap_or_else(|| {
        content
            .lines()
            .enumerate()
            .filter_map(|(line, text)| declared_symbol(language, text).map(|symbol| (line, symbol)))
            .collect()
    });
    if matches!(language, "javascript" | "typescript") {
        entries.extend(
            content
                .lines()
                .enumerate()
                .filter_map(|(line, text)| bound_function(text).map(|symbol| (line, symbol))),
        );
        entries.sort();
        entries.dedup();
    }
    entries
        .into_iter()
        .map(|(line, symbol)| (line + 1, symbol))
        .collect()
}

/// `const name = (…) =>`, `const name = async …`, `const name = function`,
/// or `const name = useSomething(` on one line.
fn bound_function(line: &str) -> Option<String> {
    let text = line.trim_start();
    let text = text.strip_prefix("export ").unwrap_or(text);
    let text = text
        .strip_prefix("const ")
        .or_else(|| text.strip_prefix("let "))?;
    let name_end = text
        .find(|character: char| !character.is_alphanumeric() && !matches!(character, '_' | '$'))
        .unwrap_or(text.len());
    let name = &text[..name_end];
    let (_, value) = text[name_end..].split_once('=')?;
    let value = value.trim_start();
    let callable = value.starts_with("async")
        || value.starts_with("function")
        || (value.starts_with('(') && value.contains("=>"))
        || value
            .strip_prefix("use")
            .is_some_and(|rest| rest.starts_with(|character: char| character.is_ascii_uppercase()));
    (valid_identifier(name) && !value.starts_with('=') && callable).then(|| name.to_owned())
}

fn language(path: &Path) -> Option<&'static str> {
    let name = path.file_name()?.to_str()?;
    if matches!(
        name,
        "Dockerfile" | "Makefile" | "Justfile" | "CMakeLists.txt"
    ) {
        return Some("build");
    }
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "rs" => Some("rust"),
        "py" | "pyi" => Some("python"),
        "js" | "mjs" | "cjs" | "jsx" => Some("javascript"),
        "ts" | "mts" | "cts" | "tsx" => Some("typescript"),
        "go" => Some("go"),
        "java" => Some("java"),
        "kt" | "kts" => Some("kotlin"),
        "c" | "h" => Some("c"),
        "cc" | "cpp" | "cxx" | "hh" | "hpp" | "hxx" => Some("cpp"),
        "cs" => Some("csharp"),
        "rb" => Some("ruby"),
        "php" => Some("php"),
        "swift" => Some("swift"),
        "sh" | "bash" | "zsh" | "fish" => Some("shell"),
        "sql" => Some("sql"),
        "html" | "htm" | "vue" | "svelte" => Some("web"),
        "css" | "scss" | "sass" | "less" => Some("style"),
        "json" | "jsonc" => Some("json"),
        "yaml" | "yml" => Some("yaml"),
        "toml" => Some("toml"),
        "md" | "mdx" => Some("markdown"),
        "proto" | "graphql" | "gql" => Some("schema"),
        _ => None,
    }
}

fn declared_symbol(language: &str, line: &str) -> Option<String> {
    let mut text = line.trim_start();
    if text.starts_with("//") || text.starts_with('#') || text.starts_with('*') {
        return None;
    }
    for prefix in [
        "pub(crate) ",
        "pub(super) ",
        "pub ",
        "export default ",
        "export ",
        "async ",
        "static ",
        "public ",
        "private ",
        "protected ",
        "internal ",
    ] {
        if let Some(rest) = text.strip_prefix(prefix) {
            text = rest.trim_start();
        }
    }
    let keywords: &[&str] = match language {
        "rust" => &[
            "fn ", "struct ", "enum ", "trait ", "type ", "mod ", "const ", "static ",
        ],
        "python" => &["def ", "class ", "async def "],
        "javascript" | "typescript" => &[
            "function ",
            "class ",
            "interface ",
            "type ",
            "enum ",
            "namespace ",
        ],
        "go" => &["func ", "type ", "var ", "const "],
        "java" | "kotlin" | "csharp" | "cpp" | "c" | "swift" => &[
            "class ",
            "struct ",
            "interface ",
            "enum ",
            "protocol ",
            "func ",
        ],
        "ruby" => &["def ", "class ", "module "],
        "php" => &["function ", "class ", "interface ", "trait "],
        "sql" => &[
            "create table ",
            "create view ",
            "create function ",
            "create procedure ",
        ],
        _ => &[],
    };
    let lower = text.to_ascii_lowercase();
    for keyword in keywords {
        if lower.starts_with(keyword) {
            let rest = &text[keyword.len()..];
            let symbol = rest
                .trim_start_matches(|character: char| {
                    !character.is_ascii_alphanumeric() && character != '_'
                })
                .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
                .next()
                .unwrap_or("");
            if !symbol.is_empty() {
                return Some(symbol.into());
            }
        }
    }
    None
}

fn syntax_declarations(language: &str, content: &str) -> Option<Vec<(usize, String)>> {
    let grammar = match language {
        "rust" => tree_sitter_rust::LANGUAGE,
        "python" => tree_sitter_python::LANGUAGE,
        "javascript" => tree_sitter_javascript::LANGUAGE,
        "typescript" => tree_sitter_typescript::LANGUAGE_TYPESCRIPT,
        "go" => tree_sitter_go::LANGUAGE,
        "java" => tree_sitter_java::LANGUAGE,
        _ => return None,
    };
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&grammar.into()).ok()?;
    let tree = parser.parse(content, None)?;
    let mut declarations: Vec<(usize, String)> = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if declaration_node(language, node.kind())
            && let Some(name) = node.child_by_field_name("name")
            && let Ok(symbol) = name.utf8_text(content.as_bytes())
            && valid_identifier(symbol)
        {
            declarations.push((node.start_position().row, symbol.into()));
        }
        // Local declarations belong to their enclosing callable's passage.
        // Splitting at a local type would detach the signature and documentation
        // from the body that implements them.
        if matches!(
            node.kind(),
            "function_item"
                | "function_definition"
                | "function_declaration"
                | "generator_function_declaration"
                | "method_definition"
                | "method_declaration"
                | "constructor_declaration"
        ) {
            continue;
        }
        let mut cursor = node.walk();
        let children = node.named_children(&mut cursor).collect::<Vec<_>>();
        stack.extend(children.into_iter().rev());
    }
    declarations.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    declarations.dedup();
    Some(declarations)
}

fn declaration_node(language: &str, kind: &str) -> bool {
    match language {
        "rust" => matches!(
            kind,
            "function_item"
                | "struct_item"
                | "enum_item"
                | "trait_item"
                | "impl_item"
                | "type_item"
                | "mod_item"
                | "const_item"
                | "static_item"
        ),
        "python" => matches!(kind, "function_definition" | "class_definition"),
        "javascript" | "typescript" => matches!(
            kind,
            "function_declaration"
                | "generator_function_declaration"
                | "class_declaration"
                | "method_definition"
                | "interface_declaration"
                | "type_alias_declaration"
                | "enum_declaration"
        ),
        "go" => matches!(
            kind,
            "function_declaration" | "method_declaration" | "type_declaration" | "type_spec"
        ),
        "java" => matches!(
            kind,
            "class_declaration"
                | "interface_declaration"
                | "enum_declaration"
                | "record_declaration"
                | "method_declaration"
                | "constructor_declaration"
        ),
        _ => false,
    }
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .chars()
            .all(|character| character.is_alphanumeric() || matches!(character, '_' | '$'))
}

fn identifiers(content: &str, declarations: &[String]) -> Vec<String> {
    let declared = declarations
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let tokens = content
        .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
        .filter(|token| {
            (2..=80).contains(&token.len()) && token.as_bytes()[0].is_ascii_alphabetic()
        });
    let mut unique = BTreeSet::new();
    for token in tokens {
        if !declared.contains(token) && !COMMON.contains(&token) {
            unique.insert(token.to_owned());
        }
        for part in identifier_parts(token) {
            if !COMMON.contains(&part.as_str()) {
                unique.insert(part);
            }
        }
    }
    let mut identifiers = unique.into_iter().collect::<Vec<_>>();
    if identifiers.len() > 96 {
        identifiers.truncate(96);
    }
    identifiers
}

fn identifier_parts(identifier: &str) -> Vec<String> {
    let mut parts = Vec::new();
    for section in identifier.split('_') {
        let bytes = section.as_bytes();
        let mut start = 0;
        for index in 1..bytes.len() {
            if bytes[index].is_ascii_uppercase() && bytes[index - 1].is_ascii_lowercase() {
                if index - start >= 2 {
                    parts.push(section[start..index].to_ascii_lowercase());
                }
                start = index;
            }
        }
        if section.len().saturating_sub(start) >= 2 {
            parts.push(section[start..].to_ascii_lowercase());
        }
    }
    parts
}

const COMMON: &[&str] = &[
    "and",
    "as",
    "async",
    "await",
    "break",
    "case",
    "class",
    "const",
    "continue",
    "def",
    "do",
    "else",
    "enum",
    "false",
    "fn",
    "for",
    "from",
    "function",
    "if",
    "impl",
    "import",
    "in",
    "interface",
    "let",
    "match",
    "mod",
    "new",
    "none",
    "null",
    "pub",
    "return",
    "self",
    "static",
    "struct",
    "super",
    "this",
    "trait",
    "true",
    "type",
    "use",
    "var",
    "where",
    "while",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callable_keeps_documentation_and_local_types_with_its_body() {
        let source = "fn previous() {}\n\n/// Serialize only the future prompt representation.\n#[inline]\nfn prompt_bytes() {\n    struct Payload { value: usize }\n    encode(Payload { value: 1 });\n}\nfn next() {}\n";
        let chunks = chunk_file("lib.rs", "rust", "hash", source).unwrap();
        let callable = chunks
            .iter()
            .find(|chunk| chunk.symbols.iter().any(|symbol| symbol == "prompt_bytes"))
            .unwrap();
        assert!(callable.content.starts_with("/// Serialize"));
        assert!(callable.content.contains("encode(Payload"));
        assert!(!callable.content.contains("fn next"));
        assert_eq!((callable.start_line, callable.end_line), (3, 8));
        assert!(!chunks[0].content.contains("Serialize"));
    }

    #[test]
    fn snapshot_is_content_addressed_structural_and_complete() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("lib.rs"),
            "pub struct Shield { power: u32 }\n\npub fn drop_rate(mode: Mode) -> f32 { 0.025 }\n",
        )
        .unwrap();
        std::fs::write(root.path().join("ignored.bin"), [0, 159, 146, 150]).unwrap();
        let workspace = Workspace::new(root.path()).unwrap();
        let first = capture(&workspace, &PipelineSettings::default()).unwrap();
        assert_eq!(first.files.len(), 1);
        assert!(
            first
                .chunks
                .iter()
                .any(|chunk| chunk.symbols.contains(&"Shield".into()))
        );
        assert!(
            first
                .chunks
                .iter()
                .any(|chunk| chunk.symbols.contains(&"drop_rate".into()))
        );
        let unchanged = capture(&workspace, &PipelineSettings::default()).unwrap();
        assert_eq!(first.snapshot_hash, unchanged.snapshot_hash);
        std::fs::write(root.path().join("lib.rs"), "pub fn changed() {}\n").unwrap();
        let changed = capture(&workspace, &PipelineSettings::default()).unwrap();
        assert_ne!(first.snapshot_hash, changed.snapshot_hash);
    }

    #[test]
    fn exceeding_complete_generation_bound_fails_instead_of_publishing_a_prefix() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.rs"), "fn a() {}\n").unwrap();
        std::fs::write(root.path().join("b.rs"), "fn b() {}\n").unwrap();
        let workspace = Workspace::new(root.path()).unwrap();
        let settings = PipelineSettings {
            code_index_max_files: 1,
            ..Default::default()
        };
        assert!(
            capture(&workspace, &settings)
                .unwrap_err()
                .to_string()
                .contains("previous generation")
        );
    }

    #[test]
    fn syntax_trees_find_nested_and_qualified_declarations_across_languages() {
        for (language, source, expected) in [
            (
                "rust",
                "struct Arena; impl Arena { pub async fn resolve_collision(&self) {} }",
                "resolve_collision",
            ),
            (
                "python",
                "@checked\ndef shield_drop_rate(mode):\n    return 0.02\n",
                "shield_drop_rate",
            ),
            (
                "typescript",
                "export abstract class ArenaController { private resolveBossCollision() {} }",
                "resolveBossCollision",
            ),
            (
                "go",
                "package arena\nfunc (game *Game) DropShield() {}\n",
                "DropShield",
            ),
            (
                "java",
                "public final class Arena { void collideWithBoss() {} }",
                "collideWithBoss",
            ),
        ] {
            let declarations = syntax_declarations(language, source).unwrap();
            assert!(
                declarations.iter().any(|(_, symbol)| symbol == expected),
                "{language}: {declarations:?}"
            );
        }
    }
}

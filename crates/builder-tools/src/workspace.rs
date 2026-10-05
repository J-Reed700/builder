use crate::Action;
use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use globset::Glob;
use ignore::WalkBuilder;
use std::{
    collections::HashMap,
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

mod shell;

#[cfg(unix)]
pub(crate) use shell::ProcessGroup;
pub use shell::{ShellFailure, ShellFailureKind};

const MAX_FILE: u64 = 2 * 1024 * 1024;
const MAX_OUTPUT: usize = 32 * 1024;
const WHOLE_FILE_LINES: usize = 200;
const MAX_READ_LINES: usize = 500;
const MAX_READ_OUTPUT: usize = 12 * 1024;
const MAX_SEARCH_OUTPUT: usize = 8 * 1024;
/// Room kept for range notes after the numbered lines.
const READ_NOTE_RESERVE: usize = 640;
const OUTLINE_ENTRIES: usize = 80;
const OUTLINE_BYTES: usize = 3 * 1024;

#[derive(Clone)]
pub struct Workspace {
    root: PathBuf,
    observed: Arc<Mutex<HashMap<PathBuf, String>>>,
}
impl Workspace {
    pub fn new(root: &Path) -> Result<Self> {
        let root = root.canonicalize().context("Workspace does not exist")?;
        ensure!(root.is_dir(), "Workspace must be a directory");
        Ok(Self {
            root,
            observed: Arc::new(Mutex::new(HashMap::new())),
        })
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn resolve(&self, relative: &str) -> Result<PathBuf> {
        let path = self.root.join(relative);
        let resolved = if path.exists() {
            path.canonicalize()?
        } else {
            // Do not permit a dangling symlink to redirect a future write.
            ensure!(
                std::fs::symlink_metadata(&path).is_err(),
                "Dangling symlink is not allowed"
            );
            path.parent()
                .context("Missing parent")?
                .canonicalize()?
                .join(path.file_name().context("Missing filename")?)
        };
        ensure!(
            resolved.starts_with(&self.root),
            "Path escapes the workspace"
        );
        let relative = resolved.strip_prefix(&self.root)?;
        ensure!(
            !relative
                .components()
                .any(|c| matches!(c.as_os_str().to_str(), Some(".git" | ".builder"))),
            "Builder metadata and .git internals are protected"
        );
        Ok(resolved)
    }
    pub fn source_hash(&self, path: &str) -> Result<String> {
        Ok(builder_core::memory::digest(
            self.read(&self.resolve(path)?)?.as_bytes(),
        ))
    }
    pub(crate) fn files(&self, glob: Option<&str>) -> Result<Vec<PathBuf>> {
        let matcher = glob
            .map(|s| Glob::new(s).map(|g| g.compile_matcher()))
            .transpose()?;
        let mut files = vec![];
        for entry in WalkBuilder::new(&self.root)
            .hidden(true)
            .require_git(false)
            .follow_links(false)
            .build()
        {
            let entry = entry?;
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            let relative = entry.path().strip_prefix(&self.root)?;
            if matcher.as_ref().is_none_or(|m| m.is_match(relative)) {
                files.push(relative.to_path_buf());
            }
            if files.len() >= 20_000 {
                break;
            }
        }
        files.sort();
        Ok(files)
    }
    pub(crate) fn read(&self, path: &Path) -> Result<String> {
        ensure!(path.is_file(), "Not a regular file");
        ensure!(
            std::fs::metadata(path)?.len() <= MAX_FILE,
            "File exceeds 2 MiB"
        );
        let file = std::fs::File::open(path).context("Cannot open source file")?;
        let mut content = String::new();
        std::io::Read::read_to_string(&mut std::io::Read::take(file, MAX_FILE + 1), &mut content)
            .context("Cannot read file as UTF-8")?;
        ensure!(content.len() as u64 <= MAX_FILE, "File exceeds 2 MiB");
        Ok(content)
    }
    pub(crate) fn write(
        &self,
        path: &Path,
        content: &str,
        expected: Option<&str>,
    ) -> Result<String> {
        ensure!(content.len() as u64 <= MAX_FILE, "Write exceeds 2 MiB");
        if path.exists() {
            ensure!(path.is_file(), "Not a regular file");
            ensure!(
                std::fs::metadata(path)?.len() <= MAX_FILE,
                "Existing file exceeds 2 MiB; refusing an unbounded read or backup"
            );
        }
        if path.exists() && std::fs::read(path)? == content.as_bytes() {
            self.remember(path, builder_core::memory::digest(content.as_bytes()));
            return Ok(format!(
                "UNCHANGED: {} already has the requested contents; no file was written",
                path.display()
            ));
        }
        ensure!(
            !path.exists() || expected.is_some(),
            "Existing file has not been read in this workspace; refusing to overwrite it. Read the current file and retry."
        );
        let _lock = self.write_lock()?;
        let mut backup = None;
        if path.exists() {
            ensure!(path.is_file(), "Not a regular file");
            ensure!(
                std::fs::metadata(path)?.len() <= MAX_FILE,
                "Existing file exceeds 2 MiB; refusing an unbounded backup"
            );
            let current = std::fs::read(path)?;
            let current_hash = builder_core::memory::digest(&current);
            if current == content.as_bytes() {
                self.remember(path, current_hash);
                return Ok(format!(
                    "UNCHANGED: {} already has the requested contents; no file was written",
                    path.display()
                ));
            }
            ensure!(
                expected.is_some_and(|hash| hash == current_hash),
                "File changed since it was read (or has not been read in this workspace); refusing to overwrite newer contents. Read the current file and retry."
            );
            let dir = self.root.join(".builder/backups");
            // Local metadata must not be redirected outside the workspace.
            for p in [self.root.join(".builder"), dir.clone()] {
                if let Ok(meta) = std::fs::symlink_metadata(&p) {
                    ensure!(
                        !meta.file_type().is_symlink(),
                        "Backup directory cannot be a symlink"
                    );
                }
            }
            std::fs::create_dir_all(&dir)?;
            let dest = dir.join(uuid::Uuid::new_v4().to_string());
            let mut source = std::fs::File::open(path)?;
            let mut copy = std::fs::File::options()
                .write(true)
                .create_new(true)
                .open(&dest)?;
            std::io::copy(&mut source, &mut copy)?;
            copy.sync_all()?;
            backup = Some(dest);
        }
        if !path.exists() {
            ensure!(
                expected.is_none(),
                "File was removed since it was read; refusing to recreate it"
            );
        }
        let tmp = path.with_file_name(format!(".builder-{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| -> Result<()> {
            let mut file = std::fs::File::options()
                .write(true)
                .create_new(true)
                .open(&tmp)?;
            if path.exists() {
                file.set_permissions(std::fs::metadata(path)?.permissions())?;
            }
            file.write_all(content.as_bytes())?;
            file.sync_all()?;
            if path.exists() {
                let now = std::fs::read(path)?;
                ensure!(
                    expected.is_some_and(|hash| hash == builder_core::memory::digest(&now)),
                    "File changed during write; refusing to overwrite newer contents"
                );
            } else {
                ensure!(
                    expected.is_none(),
                    "File was removed during write; refusing to recreate it"
                );
            }
            std::fs::rename(&tmp, path)?;
            #[cfg(unix)]
            std::fs::File::open(path.parent().unwrap())?.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result?;
        self.remember(path, builder_core::memory::digest(content.as_bytes()));
        Ok(format!(
            "Wrote {} bytes to {}.{}",
            content.len(),
            path.display(),
            backup
                .map(|p| format!(" Backup: {}", p.display()))
                .unwrap_or_default()
        ))
    }
    fn remember(&self, path: &Path, hash: String) {
        self.observed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(path.to_path_buf(), hash);
    }
    fn observed_hash(&self, path: &Path) -> Option<String> {
        self.observed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(path)
            .cloned()
    }
    fn write_lock(&self) -> Result<std::fs::File> {
        let metadata = self.root.join(".builder");
        if let Ok(meta) = std::fs::symlink_metadata(&metadata) {
            ensure!(
                !meta.file_type().is_symlink(),
                "Builder metadata cannot be a symlink"
            );
        }
        std::fs::create_dir_all(&metadata)?;
        let lock_path = metadata.join("write.lock");
        if let Ok(meta) = std::fs::symlink_metadata(&lock_path) {
            ensure!(
                !meta.file_type().is_symlink(),
                "Write lock cannot be a symlink"
            );
        }
        // Advisory lock shared by Builder processes; unrelated editors do not
        // participate, so the destination is also rechecked before rename.
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)?;
        file.lock_exclusive()?;
        Ok(file)
    }
    /// A bounded, numbered range. Limits shape the result instead of
    /// rejecting it: a model that asks for too much still gets the part that
    /// fits and the exact next range, never an empty retry round.
    fn read_range(
        &self,
        path: &str,
        start_line: Option<usize>,
        end_line: Option<usize>,
    ) -> Result<String> {
        let resolved = self.resolve(path)?;
        let content = self.read(&resolved)?;
        self.remember(&resolved, builder_core::memory::digest(content.as_bytes()));
        let total_lines = content.lines().count();
        // One approximate location is enough to begin: a lone start_line
        // reads forward from it, a lone end_line reads the chunk ending at
        // it, and out-of-range ends clamp to the file.
        let start = match (start_line, end_line) {
            (Some(start), _) => start,
            (None, Some(end)) => end.saturating_sub(MAX_READ_LINES - 1).max(1),
            (None, None) => 1,
        };
        ensure!(
            start > 0,
            "Invalid line range: start_line must be at least 1"
        );
        if let Some(end) = end_line {
            ensure!(
                end >= start,
                "Invalid line range: end_line {end} is before start_line {start}"
            );
        }
        let mut header = format!(
            "File: {} · {total_lines} total lines · {} bytes\nSource-SHA256: {}\nLine format: number|source. Everything after the first | is exact source indentation; omit the number and | when editing.",
            serde_json::to_string(path)?,
            content.len(),
            builder_core::memory::digest(content.as_bytes())
        );
        if total_lines == 0 {
            return Ok(format!("{header}\n[empty file]"));
        }
        if start > total_lines {
            return Ok(format!(
                "{header}\n[start_line {start} is beyond the last line {total_lines}; no lines returned. Use a start_line between 1 and {total_lines}]"
            ));
        }
        let mut end = end_line.unwrap_or(total_lines).min(total_lines);
        let mut notes: Vec<String> = Vec::new();
        let whole_request = start_line.is_none() && end_line.is_none();
        if whole_request && (total_lines > WHOLE_FILE_LINES || content.len() > MAX_READ_OUTPUT) {
            header.push_str(&outline(path, &content));
            end = end.min(WHOLE_FILE_LINES);
            notes.push(
                "large file: this whole-file request returns only its opening lines; choose the next range from the outline, code_search, or search instead of paging through the file"
                    .to_owned(),
            );
        }
        if start > 1 {
            notes.push(format!("lines 1–{} precede this range", start - 1));
        }
        if end - start + 1 > MAX_READ_LINES {
            end = start + MAX_READ_LINES - 1;
            notes.push(format!(
                "requested range exceeds {MAX_READ_LINES} lines; showing {start}–{end}"
            ));
        }
        // Fill the byte budget line by line; notes are short and reserved.
        let budget = MAX_READ_OUTPUT.saturating_sub(header.len() + READ_NOTE_RESERVE);
        let mut body = String::new();
        let mut shown = start - 1;
        for (index, line) in content
            .lines()
            .enumerate()
            .skip(start - 1)
            .take(end - start + 1)
        {
            let numbered = format!("{:>5}|{line}", index + 1);
            let cost = numbered.len() + usize::from(!body.is_empty());
            if body.len() + cost > budget {
                if shown < start {
                    // A single minified or generated line: show its head.
                    let mut cut = budget.saturating_sub(1);
                    while !numbered.is_char_boundary(cut) {
                        cut -= 1;
                    }
                    body.push_str(&numbered[..cut]);
                    body.push('…');
                    shown = index + 1;
                    notes.push(format!(
                        "line {shown} exceeds the {MAX_READ_OUTPUT}-byte output budget and was cut at …; that line is not exact source, so use search to locate the needed part"
                    ));
                } else {
                    notes.push(format!(
                        "{MAX_READ_OUTPUT}-byte output budget reached; showing {start}–{shown}"
                    ));
                }
                break;
            }
            if !body.is_empty() {
                body.push('\n');
            }
            body.push_str(&numbered);
            shown = index + 1;
        }
        if shown < total_lines {
            notes.push(format!(
                "lines {}–{total_lines} remain; continue with start_line={}",
                shown + 1,
                shown + 1
            ));
        }
        let mut output = header;
        output.push('\n');
        output.push_str(&body);
        if !notes.is_empty() {
            output.push_str(&format!("\n[{}]", notes.join("; ")));
        }
        Ok(output)
    }
    /// Side-effect-free inspection. Synchronous and self-contained, so
    /// several may run at once on blocking threads.
    pub fn inspect(&self, action: &Action) -> Result<String> {
        let output = match action {
            Action::ListFiles { glob } => {
                let files = self.files(glob.as_deref())?;
                let mut out = files
                    .iter()
                    .take(1000)
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join("\n");
                if files.len() > 1000 {
                    out.push_str("\n[limited to 1000 files; narrow the glob]");
                }
                out
            }
            Action::ReadFile {
                path,
                start_line,
                end_line,
            } => self.read_range(path, *start_line, *end_line)?,
            Action::Search { query, glob } => {
                ensure!(!query.is_empty(), "Search query cannot be empty");
                let mut results = vec![];
                'files: for relative in self.files(glob.as_deref())? {
                    let Ok(content) = self.read(&self.root.join(&relative)) else {
                        continue;
                    };
                    for (n, line) in content.lines().enumerate() {
                        if line.contains(query) {
                            results.push(format!(
                                "{}:{}: {}",
                                relative.display(),
                                n + 1,
                                truncate(line.to_owned(), 1000)
                            ));
                        }
                        if results.len() == 30 {
                            results
                                .push("[limited to 30 matches; narrow the query or glob]".into());
                            break 'files;
                        }
                    }
                }
                truncate(results.join("\n"), MAX_SEARCH_OUTPUT)
            }
            _ => bail!("Not a workspace inspection"),
        };
        Ok(truncate(output, MAX_OUTPUT))
    }
    pub async fn execute(&self, action: &Action) -> Result<String> {
        let output = match action {
            Action::ListFiles { .. } | Action::ReadFile { .. } | Action::Search { .. } => {
                return self.inspect(action);
            }
            Action::Subagent { .. } => bail!("Subagents require the application runtime"),
            Action::Research { .. }
            | Action::CodeSearch { .. }
            | Action::MemorySearch { .. }
            | Action::MemoryGet { .. }
            | Action::MemoryUpsert { .. }
            | Action::MemoryForget { .. } => {
                bail!("Memory actions require the application memory runtime")
            }
            Action::TodoWrite { todos } => {
                todos.validate()?;
                todos.receipt()
            }
            Action::WriteFile { path, content } => {
                let resolved = self.resolve(path)?;
                let expected = self.observed_hash(&resolved);
                self.write(&resolved, content, expected.as_deref())?
            }
            Action::EditFile { path, old, new } => {
                let path = self.resolve(path)?;
                let expected = self.observed_hash(&path);
                let content = self.read(&path)?;
                self.write(
                    &path,
                    &replace(&content, old, new, false)?,
                    expected.as_deref(),
                )?
            }
            Action::MultiEdit { path, edits } => {
                ensure!(
                    (1..=crate::MAX_EDITS).contains(&edits.len()),
                    "multi_edit takes 1–{} edits",
                    crate::MAX_EDITS
                );
                let path = self.resolve(path)?;
                let expected = self.observed_hash(&path);
                let mut content = self.read(&path)?;
                for (index, edit) in edits.iter().enumerate() {
                    content = replace(&content, &edit.old, &edit.new, edit.replace_all).map_err(
                        |error| {
                            anyhow::anyhow!(
                                "Edit {} of {} failed, so none were applied: {error:#}",
                                index + 1,
                                edits.len()
                            )
                        },
                    )?;
                }
                self.write(&path, &content, expected.as_deref())?
            }
            Action::Shell {
                command,
                timeout_secs,
            } => shell::run(&self.root, command, timeout_secs.unwrap_or(30)).await?,
        };
        Ok(truncate(output, MAX_OUTPUT))
    }
}
/// Replace exact text: once and uniquely, or every occurrence when `all`.
/// A failed match returns a diagnostic anchor, never a fuzzy replacement.
fn replace(content: &str, old: &str, new: &str, all: bool) -> Result<String> {
    ensure!(!old.is_empty(), "Old text cannot be empty");
    ensure!(
        old != new,
        "Old and new text are identical; file unchanged. Propose an actual change or report the existing result."
    );
    let matches = content.matches(old).count();
    if all && matches > 0 {
        return Ok(content.replace(old, new));
    }
    if matches == 1 {
        return Ok(content.replacen(old, new, 1));
    }
    let mut hint = String::new();
    if matches == 0
        && let Some(anchor) = old.lines().find(|line| !line.trim().is_empty())
    {
        let mut candidates = content
            .lines()
            .enumerate()
            .filter(|(_, line)| line.trim() == anchor.trim());
        if let Some((index, _)) = candidates.next()
            && candidates.next().is_none()
        {
            let snippet = content
                .lines()
                .skip(index)
                .take(8)
                .map(str::to_owned)
                .collect::<Vec<_>>()
                .join("\n");
            hint = format!(
                " A possible first-line anchor is at line {}. Current source, without line-number prefixes:\n{}",
                index + 1,
                truncate(snippet, 2048)
            );
        }
    }
    let expectation = if all {
        "Old text must match at least once"
    } else {
        "Old text must match exactly once"
    };
    let uniqueness = if matches > 1 {
        " Add surrounding lines to make it unique, or set replace_all when every occurrence should change."
    } else {
        ""
    };
    bail!(
        "{expectation}; found {matches} matches. File unchanged. Preserve source indentation and actual newlines in old/new; do not copy line-number prefixes. Use a unique exact block.{uniqueness}{hint}"
    )
}
/// Declarations with line numbers for a file too large to read whole. Rows
/// use `L<line> <name>` so they are never mistaken for numbered source.
fn outline(path: &str, content: &str) -> String {
    let entries = crate::code_index::outline(Path::new(path), content);
    if entries.is_empty() {
        return String::new();
    }
    let mut text = String::from("\nOutline (line and declaration; read a range with start_line):");
    let mut shown = 0;
    for (line, symbol) in &entries {
        let row = format!("\n L{line} {symbol}");
        if shown == OUTLINE_ENTRIES || text.len() + row.len() > OUTLINE_BYTES {
            break;
        }
        text.push_str(&row);
        shown += 1;
    }
    if shown < entries.len() {
        text.push_str(&format!(
            "\n [outline shows {shown} of {} declarations; use search for others]",
            entries.len()
        ));
    }
    text
}
fn truncate(mut text: String, limit: usize) -> String {
    if text.len() > limit {
        let marker = "\n[output truncated]";
        let mut at = limit.saturating_sub(marker.len());
        while !text.is_char_boundary(at) {
            at -= 1;
        }
        text.truncate(at);
        text.push_str(marker);
    }
    text
}

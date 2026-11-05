//! Zip snapshots of a commit: every file and folder of its tree, under one top folder.
//!
//! The archive is built from the commit's tree, not with `git archive`, so `.gitattributes`
//! export rules do not apply: every tracked file is included with its recorded mode, and
//! symbolic links stay links. Optionally, submodules are included at the commits the tree
//! records (when their repository is available locally), and Git LFS pointers are replaced
//! by their content, from the local LFS store or through `git lfs smudge`. Whatever cannot be
//! resolved is reported in [`ArchiveSummary`] and kept as Git records it: an empty folder for
//! a submodule, the pointer for an LFS file.

use crate::cancellation::Cancellation;
use crate::lfs::LfsPointer;
use crate::native_bytes::bytes_to_path_buf;
use crate::process::{GitProcessCommand, GitStreamProcess, ProcessArgument};
use crate::types::ObjectId;
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::io::{self, BufRead, BufReader, Read, Seek, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, DateTime, System, ZipWriter};

/// The largest recursive tree listing read for one repository.
pub const MAX_TREE_LISTING_BYTES: usize = 256 * 1024 * 1024;
/// How deep submodules inside submodules are followed.
pub const MAX_SUBMODULE_DEPTH: usize = 8;
/// Paths listed per kind in [`ArchiveSummary`]; the counts stay exact.
pub const MAX_REPORTED_PATHS: usize = 32;
/// Blobs up to this size are checked for an LFS pointer.
const LFS_POINTER_BYTES: u64 = 1024;
const COPY_BUFFER: usize = 64 * 1024;
const SMALL_OUTPUT: usize = 1024 * 1024;

static NEXT_TEMPORARY: AtomicU64 = AtomicU64::new(0);

/// What to archive.
#[derive(Clone, Debug)]
pub struct ArchiveRequest {
    /// The Git executable.
    pub git: OsString,
    /// A worktree or Git directory of the repository that holds [`Self::commit`].
    pub repository: PathBuf,
    /// The repository's worktree, for submodule checkouts and `lfs smudge`; `None` when bare.
    pub worktree: Option<PathBuf>,
    pub commit: ObjectId,
    /// The archive's top folder, such as `project-1a2b3c4`; empty puts files at the root.
    pub prefix: String,
    /// Include submodules at their recorded commits.
    pub submodules: bool,
    /// Replace LFS pointers by their content.
    pub lfs: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ArchiveProgress {
    /// Files written, submodule files included.
    pub files: u64,
    /// Files known so far; grows as submodules are entered.
    pub total_files: u64,
    /// Uncompressed content bytes written.
    pub bytes: u64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ArchiveSummary {
    pub files: u64,
    pub bytes: u64,
    /// Submodules kept as empty folders because their commit is not available locally.
    pub missing_submodules: Vec<Vec<u8>>,
    pub missing_submodule_count: u64,
    /// LFS files kept as pointers because their content could not be obtained.
    pub unresolved_lfs: Vec<Vec<u8>>,
    pub unresolved_lfs_count: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArchiveError {
    /// The prefix is not a relative folder name.
    InvalidRequest,
    /// The commit does not exist in the repository.
    MissingCommit,
    /// The tree listing exceeds [`MAX_TREE_LISTING_BYTES`].
    TooLarge,
    /// Git failed or answered unexpectedly.
    Git,
    /// Writing the archive failed.
    Io,
    Canceled,
}

impl std::fmt::Display for ArchiveError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::InvalidRequest => "invalid archive request",
            Self::MissingCommit => "commit not found",
            Self::TooLarge => "tree is too large to archive",
            Self::Git => "Git could not read the commit",
            Self::Io => "archive could not be written",
            Self::Canceled => "archive canceled",
        })
    }
}

impl std::error::Error for ArchiveError {}

impl From<zip::result::ZipError> for ArchiveError {
    fn from(error: zip::result::ZipError) -> Self {
        match error {
            zip::result::ZipError::Io(error) if error.kind() == io::ErrorKind::Interrupted => {
                Self::Canceled
            }
            _ => Self::Io,
        }
    }
}

/// Writes the zip snapshot of `request.commit` to `output` and returns it with a summary.
///
/// # Errors
///
/// See [`ArchiveError`]. The output then holds an incomplete archive; callers write to a
/// temporary file and discard it.
pub fn write_commit_zip<W: Write + Seek>(
    request: &ArchiveRequest,
    output: W,
    cancellation: &dyn Cancellation,
    progress: &mut dyn FnMut(ArchiveProgress),
) -> Result<(W, ArchiveSummary), ArchiveError> {
    let prefix = folder_prefix(&request.prefix)?;
    let repository = Repository {
        git: request.git.clone(),
        path: request.repository.clone(),
        worktree: request.worktree.clone(),
    };
    if !repository.contains_commit(&request.commit) {
        return Err(ArchiveError::MissingCommit);
    }
    let modified = zip_time(repository.commit_time(&request.commit)?);
    let mut writer = Writer {
        zip: ZipWriter::new(output),
        modified,
        request,
        cancellation,
        progress,
        state: ArchiveProgress::default(),
        summary: ArchiveSummary::default(),
    };
    if !prefix.is_empty() {
        writer.directory(&prefix)?;
    }
    writer.tree(&repository, &request.commit, &prefix, 0)?;
    let Writer {
        zip,
        mut summary,
        state,
        ..
    } = writer;
    let output = zip.finish()?;
    summary.files = state.files;
    summary.bytes = state.bytes;
    Ok((output, summary))
}

/// A Unix time as a zip timestamp, clamped to the format's 1980–2107 range.
fn zip_time(seconds: i64) -> DateTime {
    let days = seconds.div_euclid(86_400);
    let rest = seconds.rem_euclid(86_400);
    // Howard Hinnant's civil-from-days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let clamped = |value: i64, low: i64, high: i64| value.clamp(low, high);
    if year < 1980 {
        return DateTime::default();
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    DateTime::from_date_and_time(
        clamped(year, 1980, 2107) as u16,
        month as u8,
        day as u8,
        (rest / 3600) as u8,
        (rest % 3600 / 60) as u8,
        (rest % 60) as u8,
    )
    .unwrap_or_default()
}

fn folder_prefix(prefix: &str) -> Result<String, ArchiveError> {
    let trimmed = prefix.trim_matches('/');
    if trimmed.is_empty() {
        return Ok(String::new());
    }
    let valid = !prefix.starts_with('/')
        && !trimmed.contains(['\\', '\0', '\n', '\r'])
        && trimmed
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..");
    if valid {
        Ok(format!("{trimmed}/"))
    } else {
        Err(ArchiveError::InvalidRequest)
    }
}

struct Repository {
    git: OsString,
    path: PathBuf,
    worktree: Option<PathBuf>,
}

enum EntryKind {
    Blob,
    Commit,
}

struct TreeEntry {
    mode: u32,
    kind: EntryKind,
    id: String,
    path: Vec<u8>,
}

impl Repository {
    fn command(&self, arguments: &[&OsStr]) -> GitProcessCommand {
        GitProcessCommand::git(self.git.clone())
            .arg(ProcessArgument::visible("-C"))
            .arg(ProcessArgument::visible(self.path.as_os_str()))
            .args(
                arguments
                    .iter()
                    .map(|value| ProcessArgument::visible(*value)),
            )
    }

    /// stdout of a short command when it exits successfully.
    fn capture(&self, arguments: &[&OsStr], limit: usize) -> Result<Option<Vec<u8>>, ArchiveError> {
        let mut process =
            GitStreamProcess::spawn(self.command(arguments).output_limit(SMALL_OUTPUT))
                .map_err(|_| ArchiveError::Git)?;
        process.close_stdin();
        let mut output = Vec::new();
        process
            .stdout()
            .take(limit as u64 + 1)
            .read_to_end(&mut output)
            .map_err(|_| ArchiveError::Git)?;
        if output.len() > limit {
            return Err(ArchiveError::TooLarge);
        }
        let (status, _) = process.finish().map_err(|_| ArchiveError::Git)?;
        Ok(status.success().then_some(output))
    }

    fn contains_commit(&self, commit: &ObjectId) -> bool {
        let object = format!("{}^{{commit}}", commit.as_str());
        matches!(
            self.capture(
                &["cat-file".as_ref(), "-e".as_ref(), object.as_ref()],
                SMALL_OUTPUT
            ),
            Ok(Some(_))
        )
    }

    fn commit_time(&self, commit: &ObjectId) -> Result<i64, ArchiveError> {
        let output = self
            .capture(
                &[
                    "show".as_ref(),
                    "-s".as_ref(),
                    "--format=%ct".as_ref(),
                    commit.as_str().as_ref(),
                ],
                SMALL_OUTPUT,
            )?
            .ok_or(ArchiveError::Git)?;
        std::str::from_utf8(&output)
            .ok()
            .and_then(|value| value.trim().parse().ok())
            .ok_or(ArchiveError::Git)
    }

    fn common_dir(&self) -> Option<PathBuf> {
        let output = self
            .capture(
                &[
                    "rev-parse".as_ref(),
                    "--path-format=absolute".as_ref(),
                    "--git-common-dir".as_ref(),
                ],
                SMALL_OUTPUT,
            )
            .ok()??;
        let text = output.strip_suffix(b"\n").unwrap_or(&output);
        bytes_to_path_buf(text).ok()
    }

    fn tree(&self, commit: &ObjectId) -> Result<Vec<TreeEntry>, ArchiveError> {
        let output = self
            .capture(
                &[
                    "ls-tree".as_ref(),
                    "-r".as_ref(),
                    "-z".as_ref(),
                    "--full-tree".as_ref(),
                    commit.as_str().as_ref(),
                ],
                MAX_TREE_LISTING_BYTES,
            )?
            .ok_or(ArchiveError::Git)?;
        output
            .split(|byte| *byte == 0)
            .filter(|record| !record.is_empty())
            .map(parse_tree_record)
            .collect()
    }

    /// Submodule paths at `commit` mapped to their names in `.gitmodules`.
    fn submodule_names(&self, commit: &ObjectId) -> HashMap<Vec<u8>, String> {
        let blob = format!("{}:.gitmodules", commit.as_str());
        let Ok(Some(output)) = self.capture(
            &[
                "config".as_ref(),
                "--blob".as_ref(),
                blob.as_ref(),
                "-z".as_ref(),
                "--get-regexp".as_ref(),
                r"^submodule\..*\.path$".as_ref(),
            ],
            SMALL_OUTPUT,
        ) else {
            return HashMap::new();
        };
        output
            .split(|byte| *byte == 0)
            .filter_map(|record| {
                let newline = record.iter().position(|byte| *byte == b'\n')?;
                let key = std::str::from_utf8(&record[..newline]).ok()?;
                let name = key.strip_prefix("submodule.")?.strip_suffix(".path")?;
                Some((record[newline + 1..].to_vec(), name.to_owned()))
            })
            .collect()
    }

    /// The local repository of the submodule at `path` that holds `commit`.
    fn submodule(&self, path: &[u8], name: Option<&str>, commit: &ObjectId) -> Option<Repository> {
        let relative = bytes_to_path_buf(path).ok()?;
        let worktree = self.worktree.as_ref().map(|root| root.join(&relative));
        let mut candidates = Vec::new();
        if let Some(name) = name.filter(|name| safe_relative(name)) {
            if let Some(common) = self.common_dir() {
                candidates.push(common.join("modules").join(name));
            }
        }
        if let Some(worktree) = &worktree {
            candidates.push(worktree.clone());
        }
        candidates
            .into_iter()
            .filter(|candidate| candidate.exists())
            .map(|candidate| Repository {
                git: self.git.clone(),
                path: candidate,
                worktree: worktree.clone().filter(|path| path.join(".git").exists()),
            })
            .find(|repository| repository.contains_commit(commit))
    }
}

fn safe_relative(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with(['/', '\\'])
        && !name.contains(':')
        && name
            .split(['/', '\\'])
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

/// `<mode> SP <type> SP <object> TAB <path>`.
fn parse_tree_record(record: &[u8]) -> Result<TreeEntry, ArchiveError> {
    let tab = record
        .iter()
        .position(|byte| *byte == b'\t')
        .ok_or(ArchiveError::Git)?;
    let header = std::str::from_utf8(&record[..tab]).map_err(|_| ArchiveError::Git)?;
    let mut fields = header.split(' ');
    let (Some(mode), Some(kind), Some(id), None) =
        (fields.next(), fields.next(), fields.next(), fields.next())
    else {
        return Err(ArchiveError::Git);
    };
    let mode = u32::from_str_radix(mode, 8).map_err(|_| ArchiveError::Git)?;
    let kind = match kind {
        "blob" => EntryKind::Blob,
        "commit" => EntryKind::Commit,
        _ => return Err(ArchiveError::Git),
    };
    let path = record[tab + 1..].to_vec();
    if path.is_empty() || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(ArchiveError::Git);
    }
    Ok(TreeEntry {
        mode,
        kind,
        id: id.to_owned(),
        path,
    })
}

struct Writer<'a, W: Write + Seek> {
    zip: ZipWriter<W>,
    modified: DateTime,
    request: &'a ArchiveRequest,
    cancellation: &'a dyn Cancellation,
    progress: &'a mut dyn FnMut(ArchiveProgress),
    state: ArchiveProgress,
    summary: ArchiveSummary,
}

impl<W: Write + Seek> Writer<'_, W> {
    fn options(&self, permissions: u32, size: u64) -> SimpleFileOptions {
        // Entries carry Git's Unix modes on every host. Without an explicit system, the
        // writer records DOS attributes on Windows and the executable bit is lost.
        SimpleFileOptions::default()
            .system(System::Unix)
            .compression_method(CompressionMethod::Deflated)
            .last_modified_time(self.modified)
            .unix_permissions(permissions)
            .large_file(size >= u64::from(u32::MAX))
    }

    fn check(&self) -> Result<(), ArchiveError> {
        if self.cancellation.is_cancelled() {
            Err(ArchiveError::Canceled)
        } else {
            Ok(())
        }
    }

    fn directory(&mut self, name: &str) -> Result<(), ArchiveError> {
        let options = self.options(0o755, 0);
        self.zip.add_directory(name, options)?;
        Ok(())
    }

    fn tree(
        &mut self,
        repository: &Repository,
        commit: &ObjectId,
        prefix: &str,
        depth: usize,
    ) -> Result<(), ArchiveError> {
        self.check()?;
        let entries = repository.tree(commit)?;
        self.state.total_files += entries
            .iter()
            .filter(|entry| matches!(entry.kind, EntryKind::Blob))
            .count() as u64;
        (self.progress)(self.state);
        let names = if self.request.submodules
            && entries
                .iter()
                .any(|entry| matches!(entry.kind, EntryKind::Commit))
        {
            repository.submodule_names(commit)
        } else {
            HashMap::new()
        };
        let mut batch = GitStreamProcess::spawn(
            repository
                .command(&["cat-file".as_ref(), "--batch".as_ref()])
                .output_limit(SMALL_OUTPUT),
        )
        .map_err(|_| ArchiveError::Git)?;
        let mut lfs = Lfs::new(repository);
        {
            let (Some(stdin), stdout) = batch.pipes() else {
                return Err(ArchiveError::Git);
            };
            let mut reader = BufReader::with_capacity(COPY_BUFFER, stdout);
            for entry in &entries {
                self.check()?;
                let name = format!("{prefix}{}", String::from_utf8_lossy(&entry.path));
                match entry.kind {
                    EntryKind::Blob => {
                        writeln!(stdin, "{}", entry.id).map_err(|_| ArchiveError::Git)?;
                        stdin.flush().map_err(|_| ArchiveError::Git)?;
                        let size = blob_header(&mut reader, &entry.id)?;
                        self.blob(&mut reader, entry, &name, size, &mut lfs)?;
                        let mut newline = [0_u8];
                        reader
                            .read_exact(&mut newline)
                            .map_err(|_| ArchiveError::Git)?;
                        if newline != *b"\n" {
                            return Err(ArchiveError::Git);
                        }
                        self.state.files += 1;
                        (self.progress)(self.state);
                    }
                    EntryKind::Commit => {
                        self.directory(&format!("{name}/"))?;
                        let id =
                            ObjectId::parse(entry.id.clone()).map_err(|_| ArchiveError::Git)?;
                        let child = (self.request.submodules && depth < MAX_SUBMODULE_DEPTH)
                            .then(|| {
                                repository.submodule(
                                    &entry.path,
                                    names.get(&entry.path).map(String::as_str),
                                    &id,
                                )
                            })
                            .flatten();
                        match child {
                            Some(child) => {
                                self.tree(&child, &id, &format!("{name}/"), depth + 1)?;
                            }
                            None if self.request.submodules => {
                                record(
                                    &mut self.summary.missing_submodules,
                                    &mut self.summary.missing_submodule_count,
                                    name.as_bytes(),
                                );
                            }
                            None => {}
                        }
                    }
                }
            }
        }
        batch.close_stdin();
        let (status, _) = batch.finish().map_err(|_| ArchiveError::Git)?;
        if status.success() {
            Ok(())
        } else {
            Err(ArchiveError::Git)
        }
    }

    fn blob(
        &mut self,
        reader: &mut impl BufRead,
        entry: &TreeEntry,
        name: &str,
        size: u64,
        lfs: &mut Lfs<'_>,
    ) -> Result<(), ArchiveError> {
        if entry.mode == 0o120_000 {
            let mut target = Vec::new();
            reader
                .take(size)
                .read_to_end(&mut target)
                .map_err(|_| ArchiveError::Git)?;
            let options = self.options(0o777, size);
            self.zip
                .add_symlink(name, String::from_utf8_lossy(&target), options)?;
            self.state.bytes += size;
            return Ok(());
        }
        let permissions = if entry.mode & 0o111 == 0 {
            0o644
        } else {
            0o755
        };
        if self.request.lfs && size <= LFS_POINTER_BYTES {
            let mut content = Vec::new();
            reader
                .take(size)
                .read_to_end(&mut content)
                .map_err(|_| ArchiveError::Git)?;
            if (content.len() as u64) < size {
                return Err(ArchiveError::Git);
            }
            let pointer = std::str::from_utf8(&content)
                .ok()
                .and_then(|text| LfsPointer::parse(text).ok().flatten());
            if let Some(pointer) = pointer {
                if let Some(mut source) = lfs.content(&pointer, &entry.path, &content) {
                    let options = self.options(permissions, pointer.size);
                    self.zip.start_file(name, options)?;
                    let copied = self.copy(&mut source.reader(), pointer.size)?;
                    if copied == pointer.size {
                        return Ok(());
                    }
                    return Err(ArchiveError::Io);
                }
                record(
                    &mut self.summary.unresolved_lfs,
                    &mut self.summary.unresolved_lfs_count,
                    name.as_bytes(),
                );
            }
            let options = self.options(permissions, size);
            self.zip.start_file(name, options)?;
            self.zip.write_all(&content).map_err(|_| ArchiveError::Io)?;
            self.state.bytes += size;
            return Ok(());
        }
        let options = self.options(permissions, size);
        self.zip.start_file(name, options)?;
        if self.copy(reader, size)? == size {
            Ok(())
        } else {
            Err(ArchiveError::Git)
        }
    }

    /// Copies up to `size` bytes into the current entry, checking cancellation per chunk.
    fn copy(&mut self, reader: &mut impl Read, size: u64) -> Result<u64, ArchiveError> {
        let mut buffer = vec![0_u8; COPY_BUFFER];
        let mut copied = 0;
        while copied < size {
            self.check()?;
            let wanted =
                usize::try_from((size - copied).min(COPY_BUFFER as u64)).unwrap_or(COPY_BUFFER);
            let read = reader
                .read(&mut buffer[..wanted])
                .map_err(|_| ArchiveError::Git)?;
            if read == 0 {
                break;
            }
            self.zip
                .write_all(&buffer[..read])
                .map_err(|_| ArchiveError::Io)?;
            copied += read as u64;
            self.state.bytes += read as u64;
        }
        Ok(copied)
    }
}

fn record(paths: &mut Vec<Vec<u8>>, count: &mut u64, path: &[u8]) {
    *count += 1;
    if paths.len() < MAX_REPORTED_PATHS {
        paths.push(path.to_vec());
    }
}

/// Reads `<object> blob <size>\n` and returns the size.
fn blob_header(reader: &mut impl BufRead, id: &str) -> Result<u64, ArchiveError> {
    let mut header = Vec::new();
    reader
        .by_ref()
        .take(512)
        .read_until(b'\n', &mut header)
        .map_err(|_| ArchiveError::Git)?;
    let header = header.strip_suffix(b"\n").ok_or(ArchiveError::Git)?;
    let header = std::str::from_utf8(header).map_err(|_| ArchiveError::Git)?;
    let mut fields = header.split(' ');
    match (fields.next(), fields.next(), fields.next(), fields.next()) {
        (Some(object), Some("blob"), Some(size), None) if object == id => {
            size.parse().map_err(|_| ArchiveError::Git)
        }
        _ => Err(ArchiveError::Git),
    }
}

/// Where an LFS file's content comes from.
enum LfsSource {
    Store(std::fs::File),
    Smudged(TemporaryFile),
}

impl LfsSource {
    fn reader(&mut self) -> &mut std::fs::File {
        match self {
            Self::Store(file) => file,
            Self::Smudged(temporary) => &mut temporary.file,
        }
    }
}

struct TemporaryFile {
    path: PathBuf,
    file: std::fs::File,
}

impl Drop for TemporaryFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

struct Lfs<'a> {
    repository: &'a Repository,
    store: std::cell::OnceCell<Option<PathBuf>>,
    /// Set after the first smudge failure: Git LFS is missing or cannot reach the server.
    smudge_failed: bool,
}

impl<'a> Lfs<'a> {
    fn new(repository: &'a Repository) -> Self {
        Self {
            repository,
            store: std::cell::OnceCell::new(),
            smudge_failed: false,
        }
    }

    fn content(&mut self, pointer: &LfsPointer, path: &[u8], raw: &[u8]) -> Option<LfsSource> {
        let store = self
            .store
            .get_or_init(|| {
                self.repository
                    .common_dir()
                    .map(|dir| dir.join("lfs").join("objects"))
            })
            .clone();
        if let Some(store) = store {
            let oid = &pointer.oid_sha256;
            let object = store.join(&oid[..2]).join(&oid[2..4]).join(oid);
            if let Ok(file) = std::fs::File::open(&object) {
                if file
                    .metadata()
                    .is_ok_and(|metadata| metadata.is_file() && metadata.len() == pointer.size)
                {
                    return Some(LfsSource::Store(file));
                }
            }
        }
        if self.smudge_failed {
            return None;
        }
        let smudged = self.smudge(pointer, path, raw);
        if smudged.is_none() {
            self.smudge_failed = true;
        }
        smudged.map(LfsSource::Smudged)
    }

    /// Runs `git lfs smudge` into a private temporary file; the content counts only when Git
    /// LFS succeeds with exactly the pointer's size.
    fn smudge(&self, pointer: &LfsPointer, path: &[u8], raw: &[u8]) -> Option<TemporaryFile> {
        let directory = self
            .repository
            .worktree
            .as_ref()
            .unwrap_or(&self.repository.path);
        let relative = bytes_to_path_buf(path).ok()?;
        let command = GitProcessCommand::git(self.repository.git.clone())
            .arg(ProcessArgument::visible("-C"))
            .arg(ProcessArgument::visible(directory.as_os_str()))
            .arg(ProcessArgument::visible("lfs"))
            .arg(ProcessArgument::visible("smudge"))
            .arg(ProcessArgument::visible("--"))
            .arg(ProcessArgument::visible(relative.into_os_string()))
            .output_limit(SMALL_OUTPUT);
        let mut process = GitStreamProcess::spawn(command).ok()?;
        process.stdin()?.write_all(raw).ok()?;
        process.close_stdin();
        let temporary = temporary_file()?;
        let mut file = &temporary.file;
        let copied = io::copy(&mut process.stdout().take(pointer.size + 1), &mut file).ok()?;
        let (status, _) = process.finish().ok()?;
        if !status.success() || copied != pointer.size {
            return None;
        }
        let mut temporary = temporary;
        temporary.file.rewind().ok()?;
        Some(temporary)
    }
}

fn temporary_file() -> Option<TemporaryFile> {
    let path = std::env::temp_dir().join(format!(
        "devmil-git-lfs-{}-{}",
        std::process::id(),
        NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed)
    ));
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(&path).ok()?;
    Some(TemporaryFile { path, file })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefixes_are_relative_folders() {
        assert_eq!(folder_prefix("").unwrap(), "");
        assert_eq!(folder_prefix("project-1a2b").unwrap(), "project-1a2b/");
        assert_eq!(folder_prefix("a/b/").unwrap(), "a/b/");
        for invalid in ["/abs", "..", "a/../b", "a\\b", "a//b"] {
            assert_eq!(
                folder_prefix(invalid),
                Err(ArchiveError::InvalidRequest),
                "{invalid}"
            );
        }
    }

    #[test]
    fn timestamps_convert_and_clamp() {
        let time = zip_time(1_700_000_000); // 2023-11-14 22:13:20 UTC
        assert_eq!(
            (
                time.year(),
                time.month(),
                time.day(),
                time.hour(),
                time.minute(),
                time.second()
            ),
            (2023, 11, 14, 22, 13, 20)
        );
        assert_eq!(zip_time(0), DateTime::default());
    }

    #[test]
    fn tree_records_parse_files_links_and_submodules() {
        let file = parse_tree_record(b"100755 blob 0123abcd\tbin/run").unwrap();
        assert_eq!(
            (file.mode, file.path.as_slice()),
            (0o100_755, b"bin/run".as_slice())
        );
        let link = parse_tree_record(b"120000 blob 0123abcd\tlink").unwrap();
        assert_eq!(link.mode, 0o120_000);
        assert!(matches!(
            parse_tree_record(b"160000 commit 0123abcd\tvendor/lib")
                .unwrap()
                .kind,
            EntryKind::Commit
        ));
        assert!(parse_tree_record(b"100644 blob nothex\tx").is_err());
        assert!(parse_tree_record(b"100644 tree 0123\tx").is_err());
    }

    #[test]
    fn submodule_names_must_stay_inside_the_modules_folder() {
        assert!(safe_relative("lib"));
        assert!(safe_relative("vendor/lib"));
        for unsafe_name in ["", "../x", "a/../../b", "/abs", "C:x", "a\\..\\b"] {
            assert!(!safe_relative(unsafe_name), "{unsafe_name}");
        }
    }
}

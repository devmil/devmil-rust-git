//! Theme-independent syntax categories for the sources of a Git diff.
//!
//! The engine is syntect with two-face's bat-curated grammar collection and
//! the Oniguruma regex backend. It annotates whole, exact source versions (see
//! [`super::source`]): the state at a displayed line depends on every line
//! before it, so a hunk is never parsed on its own. Results are half-open
//! UTF-8 byte ranges into each requested line, without its terminator, and a
//! closed set of consumer-neutral [`SyntaxTokenKind`]s. Grammar scopes,
//! themes and colours stay inside this module.
//!
//! The engine never runs Git and never loads repository-supplied grammars.
//! Callers decide when and for which lines annotation runs, on a worker of
//! their own: an [`SyntaxAnnotator`] is not meant to be shared between threads.

use crate::cancellation::Cancellation;
use crate::diff::source::SourceLines;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};
use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxSet};

/// The engine and grammar versions that determine every result.
pub const ENGINE_IDENTITY: &str = "syntect=5.3.0;two-face=0.5.2+bat-0.26.1;regex=onig";

/// The closed category vocabulary. Unknown grammar scopes map to `Plain`.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum SyntaxTokenKind {
    Plain,
    Comment,
    Keyword,
    String,
    Number,
    Type,
    Function,
    Variable,
    Constant,
    Operator,
    Punctuation,
    Attribute,
    Tag,
}

impl SyntaxTokenKind {
    pub const ALL: [Self; 13] = [
        Self::Plain,
        Self::Comment,
        Self::Keyword,
        Self::String,
        Self::Number,
        Self::Type,
        Self::Function,
        Self::Variable,
        Self::Constant,
        Self::Operator,
        Self::Punctuation,
        Self::Attribute,
        Self::Tag,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Self::Plain => "plain",
            Self::Comment => "comment",
            Self::Keyword => "keyword",
            Self::String => "string",
            Self::Number => "number",
            Self::Type => "type",
            Self::Function => "function",
            Self::Variable => "variable",
            Self::Constant => "constant",
            Self::Operator => "operator",
            Self::Punctuation => "punctuation",
            Self::Attribute => "attribute",
            Self::Tag => "tag",
        }
    }
}

/// A half-open UTF-8 byte range of one line, without its terminator.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SyntaxSpan {
    pub start: u32,
    pub end: u32,
    pub kind: SyntaxTokenKind,
}

/// Caller-chosen bounds of one annotator. Defaults are conservative
/// implementation ceilings, not measured acceptance values.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SyntaxLimits {
    /// Larger sources are not parsed.
    pub source_bytes: usize,
    /// A longer line (terminator included) ends parsing of its source: the
    /// state after it is unknown, so it and every later line stay plain.
    pub line_bytes: usize,
    /// A line with more spans after coalescing stays plain.
    pub spans_per_line: usize,
    /// A checkpoint captures the state before every `checkpoint_interval`th line.
    pub checkpoint_interval: u32,
    pub checkpoints_per_source: usize,
    pub checkpoints: usize,
    /// Lines between cancellation and deadline checks.
    pub slice_lines: u32,
    /// Parsing time one [`SyntaxAnnotator::annotate`] call may use.
    pub request_time: Duration,
}

impl Default for SyntaxLimits {
    fn default() -> Self {
        Self {
            source_bytes: 8 * 1024 * 1024,
            line_bytes: 16 * 1024,
            spans_per_line: 64,
            checkpoint_interval: 256,
            checkpoints_per_source: 32,
            checkpoints: 128,
            slice_lines: 32,
            request_time: Duration::from_millis(500),
        }
    }
}

/// One grammar of a [`SyntaxCatalog`].
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SyntaxLanguage(usize);

/// The bundled grammars and the scope classification rules.
pub struct SyntaxCatalog {
    set: SyntaxSet,
    plain_text: Option<usize>,
    textual: [Scope; 3],
    rules: Vec<(Scope, SyntaxTokenKind)>,
    fingerprint: [u8; 32],
}

impl std::fmt::Debug for SyntaxCatalog {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SyntaxCatalog")
            .field("grammars", &self.set.syntaxes().len())
            .finish_non_exhaustive()
    }
}

/// Scope prefixes in precedence order: the innermost scope with a matching
/// rule decides, the first matching rule of that scope wins.
const RULES: &[(&str, SyntaxTokenKind)] = &[
    ("constant.numeric", SyntaxTokenKind::Number),
    ("constant.character.escape", SyntaxTokenKind::String),
    ("constant", SyntaxTokenKind::Constant),
    ("keyword.operator", SyntaxTokenKind::Operator),
    ("keyword", SyntaxTokenKind::Keyword),
    ("storage.type.primitive", SyntaxTokenKind::Type),
    ("storage.type.annotation", SyntaxTokenKind::Attribute),
    ("storage", SyntaxTokenKind::Keyword),
    ("variable.language", SyntaxTokenKind::Keyword),
    ("variable.function", SyntaxTokenKind::Function),
    ("variable.annotation", SyntaxTokenKind::Attribute),
    ("variable.other.constant", SyntaxTokenKind::Constant),
    ("variable", SyntaxTokenKind::Variable),
    ("entity.name.function", SyntaxTokenKind::Function),
    ("entity.name.tag", SyntaxTokenKind::Tag),
    ("entity.other.attribute-name", SyntaxTokenKind::Attribute),
    ("entity.other.inherited-class", SyntaxTokenKind::Type),
    ("entity.name.section", SyntaxTokenKind::Keyword),
    ("entity.name", SyntaxTokenKind::Type),
    ("support.function", SyntaxTokenKind::Function),
    ("support.type", SyntaxTokenKind::Type),
    ("support.class", SyntaxTokenKind::Type),
    ("support.constant", SyntaxTokenKind::Constant),
    ("support.variable", SyntaxTokenKind::Variable),
    ("meta.annotation", SyntaxTokenKind::Attribute),
    ("meta.attribute", SyntaxTokenKind::Attribute),
    ("markup.heading", SyntaxTokenKind::Keyword),
    ("markup.underline.link", SyntaxTokenKind::Constant),
    ("punctuation", SyntaxTokenKind::Punctuation),
];

/// File names whose grammar the bundled catalog cannot find by its own name
/// rules. Keys are compared case-insensitively.
const ALIASES: &[(&str, &str)] = &[
    ("containerfile", "Dockerfile"),
    ("gnumakefile", "Makefile"),
    ("justfile", "Makefile"),
];

/// Bytes of a first line that language detection reads.
const FIRST_LINE_BYTES: usize = 1024;

impl SyntaxCatalog {
    /// Loads two-face's grammars for newline-terminated lines. The catalog
    /// is immutable; load it once per worker, lazily, never per request.
    pub fn bundled() -> Self {
        Self::from_set(two_face::syntax::extra_newlines())
    }

    fn from_set(set: SyntaxSet) -> Self {
        let scope = |name: &str| Scope::new(name).expect("static scope names are valid");
        let mut digest = Sha256::new();
        digest.update(ENGINE_IDENTITY.as_bytes());
        for syntax in set.syntaxes() {
            digest.update(syntax.name.as_bytes());
            digest.update([0]);
            digest.update(syntax.scope.build_string().as_bytes());
            digest.update([0]);
        }
        Self {
            plain_text: set
                .syntaxes()
                .iter()
                .position(|syntax| syntax.name == "Plain Text"),
            textual: [scope("comment"), scope("string"), scope("markup.raw")],
            rules: RULES
                .iter()
                .map(|(name, kind)| (scope(name), *kind))
                .collect(),
            fingerprint: digest.finalize().into(),
            set,
        }
    }

    /// Identifies the engine, grammar versions and grammar set; include it in
    /// every cache key of annotation results.
    pub const fn fingerprint(&self) -> [u8; 32] {
        self.fingerprint
    }

    pub fn language_name(&self, language: SyntaxLanguage) -> Option<&str> {
        self.set
            .syntaxes()
            .get(language.0)
            .map(|syntax| syntax.name.as_str())
    }

    /// The grammar for a repository path and its source, or `None` for plain
    /// text. Order: full file name, compound then final suffix (exact case
    /// first, then lowercase), a small alias table, then the first line
    /// (shebangs, modelines). Only the final path component is read, so a
    /// non-UTF-8 directory does not prevent detection.
    pub fn resolve(&self, path: &[u8], source: &[u8]) -> Option<SyntaxLanguage> {
        let name = path.rsplit(|byte| *byte == b'/').next().unwrap_or(path);
        let by_name = std::str::from_utf8(name)
            .ok()
            .and_then(|name| self.resolve_name(name));
        by_name
            .or_else(|| self.resolve_first_line(source))
            .filter(|language| Some(language.0) != self.plain_text)
    }

    fn resolve_name(&self, name: &str) -> Option<SyntaxLanguage> {
        let lower = name.to_ascii_lowercase();
        let mut candidates = vec![name.to_owned(), lower.clone()];
        for (index, _) in name.match_indices('.') {
            let suffix = &name[index + 1..];
            if !suffix.is_empty() {
                candidates.push(suffix.to_owned());
                candidates.push(suffix.to_ascii_lowercase());
            }
        }
        for candidate in &candidates {
            if let Some(found) = self.by_extension(candidate) {
                return Some(found);
            }
        }
        ALIASES
            .iter()
            .find(|(alias, _)| *alias == lower)
            .and_then(|(_, grammar)| self.by_name(grammar))
    }

    fn resolve_first_line(&self, source: &[u8]) -> Option<SyntaxLanguage> {
        let end = source
            .iter()
            .take(FIRST_LINE_BYTES)
            .position(|byte| *byte == b'\n')
            .unwrap_or_else(|| source.len().min(FIRST_LINE_BYTES));
        let line = std::str::from_utf8(&source[..end]).ok()?;
        let syntax = self.set.find_syntax_by_first_line(line)?;
        self.index_of(&syntax.name)
    }

    fn by_extension(&self, extension: &str) -> Option<SyntaxLanguage> {
        let syntax = self.set.find_syntax_by_extension(extension)?;
        self.index_of(&syntax.name)
    }

    fn by_name(&self, name: &str) -> Option<SyntaxLanguage> {
        self.index_of(name)
    }

    fn index_of(&self, name: &str) -> Option<SyntaxLanguage> {
        self.set
            .syntaxes()
            .iter()
            .position(|syntax| syntax.name == name)
            .map(SyntaxLanguage)
    }

    /// The category of the innermost scope of `stack` with a rule. Anything
    /// inside a comment or string stays a comment or string, even where the
    /// grammar marks an identifier within it.
    fn classify(&self, stack: &[Scope]) -> SyntaxTokenKind {
        for scope in stack.iter().rev() {
            if self.textual[0].is_prefix_of(*scope) {
                return SyntaxTokenKind::Comment;
            }
            if self.textual[1].is_prefix_of(*scope) || self.textual[2].is_prefix_of(*scope) {
                return SyntaxTokenKind::String;
            }
        }
        for scope in stack.iter().rev() {
            if let Some((_, kind)) = self
                .rules
                .iter()
                .find(|(rule, _)| rule.is_prefix_of(*scope))
            {
                return *kind;
            }
        }
        SyntaxTokenKind::Plain
    }
}

/// The licence notices of the bundled grammars that require acknowledgement,
/// as Markdown: one section per grammar licence, in the bundle's order. An
/// application ships them with its other notices and checks them for
/// freshness when the grammar version moves.
pub fn grammar_notices_markdown() -> String {
    use std::fmt::Write as _;
    let mut markdown = String::from(
        "# Syntax grammars\n\nThe syntax grammars bundled for diff highlighting come from \
         two-face, which curates them from the bat project. These licences require \
         acknowledgement.\n",
    );
    for license in two_face::acknowledgement::listing().for_syntaxes() {
        let path = license.rel_path.to_string_lossy().replace('\\', "/");
        let _ = write!(
            markdown,
            "\n## {path}\n\n```text\n{}",
            license.text.trim_end()
        );
        markdown.push_str("\n```\n");
    }
    markdown
}

/// One annotation request.
#[derive(Clone, Copy, Debug)]
pub struct AnnotationRequest<'a> {
    /// The identity of `source`'s exact bytes, such as an immutable object ID
    /// or a content digest. Checkpoints are kept under it and `language`.
    pub source_key: &'a [u8],
    pub source: &'a [u8],
    pub language: SyntaxLanguage,
    /// One-based source lines whose spans are wanted.
    pub lines: &'a [u32],
}

/// The categories of one line.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LineSyntax {
    /// Ordered, non-overlapping, nonempty spans of adjacent different kinds;
    /// `plain` is omitted. Empty means the whole line is plain.
    Spans(Vec<SyntaxSpan>),
    /// The line, or an earlier one, exceeded a bound; it stays plain.
    LimitReached,
}

/// The result of one [`SyntaxAnnotator::annotate`] call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Annotation {
    /// Requested lines the source has, each with its categories. `complete`
    /// is false when the time budget ended first; the missing lines can be
    /// requested again and resume from a checkpoint.
    Lines {
        lines: BTreeMap<u32, LineSyntax>,
        complete: bool,
    },
    /// The source is not valid UTF-8.
    InvalidEncoding,
    /// The source exceeds [`SyntaxLimits::source_bytes`].
    TooLarge,
    Canceled,
}

#[derive(Clone)]
struct Checkpoint {
    state: ParseState,
    stack: ScopeStack,
    used: u64,
}

type CheckpointKey = (Vec<u8>, SyntaxLanguage);

/// Parses sources with resumable checkpoints. Single-threaded by design:
/// construct it on the worker that uses it.
pub struct SyntaxAnnotator {
    catalog: Arc<SyntaxCatalog>,
    limits: SyntaxLimits,
    checkpoints: HashMap<CheckpointKey, BTreeMap<u32, Checkpoint>>,
    retained: usize,
    clock: u64,
}

impl std::fmt::Debug for SyntaxAnnotator {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SyntaxAnnotator")
            .field("checkpoints", &self.retained)
            .finish_non_exhaustive()
    }
}

impl SyntaxAnnotator {
    pub fn new(catalog: Arc<SyntaxCatalog>, limits: SyntaxLimits) -> Self {
        Self {
            catalog,
            limits,
            checkpoints: HashMap::new(),
            retained: 0,
            clock: 0,
        }
    }

    pub fn catalog(&self) -> &Arc<SyntaxCatalog> {
        &self.catalog
    }

    pub const fn limits(&self) -> &SyntaxLimits {
        &self.limits
    }

    /// Checkpoints currently retained.
    pub const fn retained_checkpoints(&self) -> usize {
        self.retained
    }

    /// Forgets every checkpoint.
    pub fn clear(&mut self) {
        self.checkpoints.clear();
        self.retained = 0;
    }

    /// Annotates the requested lines of one source version, parsing from its
    /// start or the nearest earlier checkpoint. Cancellation and the time
    /// budget are checked between lines; one grammar call is not preempted.
    pub fn annotate(
        &mut self,
        request: AnnotationRequest<'_>,
        cancellation: &dyn Cancellation,
    ) -> Annotation {
        if request.source.len() > self.limits.source_bytes {
            return Annotation::TooLarge;
        }
        let Ok(text) = std::str::from_utf8(request.source) else {
            return Annotation::InvalidEncoding;
        };
        let catalog = Arc::clone(&self.catalog);
        let Some(syntax) = catalog.set.syntaxes().get(request.language.0) else {
            return Annotation::Lines {
                lines: BTreeMap::new(),
                complete: true,
            };
        };
        let lines = SourceLines::new(request.source);
        let wanted = request
            .lines
            .iter()
            .copied()
            .filter(|line| *line >= 1 && (*line as usize) <= lines.len())
            .collect::<BTreeSet<_>>();
        let (Some(&first), Some(&last)) = (wanted.first(), wanted.last()) else {
            return Annotation::Lines {
                lines: BTreeMap::new(),
                complete: true,
            };
        };
        let key = (request.source_key.to_vec(), request.language);
        let (mut line_number, mut state, mut stack) = self.resume(&key, first).map_or_else(
            || (1, ParseState::new(syntax), ScopeStack::new()),
            |(line, checkpoint)| (line, checkpoint.state, checkpoint.stack),
        );
        let started = Instant::now();
        let interval = self.limits.checkpoint_interval.max(1);
        let slice = self.limits.slice_lines.max(1);
        let mut result = BTreeMap::new();
        let mut offset = line_offset(&lines, line_number);
        while line_number <= last {
            if line_number % slice == 0 {
                if cancellation.is_cancelled() {
                    return Annotation::Canceled;
                }
                if started.elapsed() >= self.limits.request_time {
                    return Annotation::Lines {
                        lines: result,
                        complete: false,
                    };
                }
            }
            if line_number > 1 && (line_number - 1) % interval == 0 {
                self.store(&key, line_number, &state, &stack);
            }
            let complete = lines.complete(line_number).unwrap_or_default();
            if complete.len() > self.limits.line_bytes {
                limit_remaining(&mut result, &wanted, line_number);
                break;
            }
            let line_text = &text[offset..offset + complete.len()];
            offset += complete.len();
            let content = lines
                .line(line_number)
                .map_or(0, |(content, _)| content.len());
            let Ok(operations) = state.parse_line(line_text, &catalog.set) else {
                limit_remaining(&mut result, &wanted, line_number);
                break;
            };
            let want = wanted.contains(&line_number);
            let Ok(line) = self.line_syntax(&operations, &mut stack, want, content) else {
                limit_remaining(&mut result, &wanted, line_number);
                break;
            };
            if let Some(line) = line {
                result.insert(line_number, line);
            }
            line_number += 1;
        }
        Annotation::Lines {
            lines: result,
            complete: true,
        }
    }

    /// Applies one line's scope operations to `stack`. When the line is
    /// `wanted`, also returns its categories clipped to `content` bytes.
    /// Fails when the grammar produced an inconsistent operation.
    fn line_syntax(
        &self,
        operations: &[(usize, syntect::parsing::ScopeStackOp)],
        stack: &mut ScopeStack,
        wanted: bool,
        content: usize,
    ) -> Result<Option<LineSyntax>, InconsistentScopes> {
        let mut spans: Vec<SyntaxSpan> = Vec::new();
        let mut position = 0;
        for (index, operation) in operations {
            if wanted && *index > position {
                self.push_span(&mut spans, position, (*index).min(content), stack);
                position = *index;
            }
            stack.apply(operation).map_err(|_| InconsistentScopes)?;
        }
        if !wanted {
            return Ok(None);
        }
        if position < content {
            self.push_span(&mut spans, position, content, stack);
        }
        spans.retain(|span| span.kind != SyntaxTokenKind::Plain);
        Ok(Some(if spans.len() > self.limits.spans_per_line {
            LineSyntax::LimitReached
        } else {
            LineSyntax::Spans(spans)
        }))
    }

    fn push_span(&self, spans: &mut Vec<SyntaxSpan>, start: usize, end: usize, stack: &ScopeStack) {
        if start >= end {
            return;
        }
        let kind = self.catalog.classify(stack.as_slice());
        let (Ok(start), Ok(end)) = (u32::try_from(start), u32::try_from(end)) else {
            return;
        };
        if let Some(previous) = spans.last_mut() {
            if previous.kind == kind && previous.end == start {
                previous.end = end;
                return;
            }
        }
        spans.push(SyntaxSpan { start, end, kind });
    }

    /// The latest checkpoint at or before `line`.
    fn resume(&mut self, key: &CheckpointKey, line: u32) -> Option<(u32, Checkpoint)> {
        self.clock += 1;
        let clock = self.clock;
        let (start, checkpoint) = self
            .checkpoints
            .get_mut(key)?
            .range_mut(..=line)
            .next_back()?;
        checkpoint.used = clock;
        Some((*start, checkpoint.clone()))
    }

    fn store(&mut self, key: &CheckpointKey, line: u32, state: &ParseState, stack: &ScopeStack) {
        self.clock += 1;
        let clock = self.clock;
        let source = self.checkpoints.entry(key.clone()).or_default();
        if let Some(existing) = source.get_mut(&line) {
            existing.used = clock;
            return;
        }
        source.insert(
            line,
            Checkpoint {
                state: state.clone(),
                stack: stack.clone(),
                used: clock,
            },
        );
        self.retained += 1;
        if source.len() > self.limits.checkpoints_per_source {
            if let Some(oldest) = least_used(source) {
                source.remove(&oldest);
                self.retained -= 1;
            }
        }
        while self.retained > self.limits.checkpoints {
            let Some((victim, line)) = self
                .checkpoints
                .iter()
                .filter_map(|(key, lines)| {
                    lines
                        .iter()
                        .min_by_key(|(_, checkpoint)| checkpoint.used)
                        .map(|(line, checkpoint)| (checkpoint.used, key.clone(), *line))
                })
                .min_by_key(|(used, _, _)| *used)
                .map(|(_, key, line)| (key, line))
            else {
                break;
            };
            if let Some(lines) = self.checkpoints.get_mut(&victim) {
                lines.remove(&line);
                if lines.is_empty() {
                    self.checkpoints.remove(&victim);
                }
            }
            self.retained -= 1;
        }
    }
}

/// A grammar popped more scopes than it pushed.
struct InconsistentScopes;

fn least_used(lines: &BTreeMap<u32, Checkpoint>) -> Option<u32> {
    lines
        .iter()
        .min_by_key(|(_, checkpoint)| checkpoint.used)
        .map(|(line, _)| *line)
}

fn line_offset(lines: &SourceLines<'_>, line: u32) -> usize {
    (1..line)
        .map(|number| lines.complete(number).map_or(0, <[u8]>::len))
        .sum()
}

fn limit_remaining(result: &mut BTreeMap<u32, LineSyntax>, wanted: &BTreeSet<u32>, from: u32) {
    for line in wanted.range(from..) {
        result.insert(*line, LineSyntax::LimitReached);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt::Write as _;
    use std::sync::atomic::AtomicBool;
    use std::sync::OnceLock;

    fn catalog() -> Arc<SyntaxCatalog> {
        static CATALOG: OnceLock<Arc<SyntaxCatalog>> = OnceLock::new();
        Arc::clone(CATALOG.get_or_init(|| Arc::new(SyntaxCatalog::bundled())))
    }

    fn language(path: &str) -> SyntaxLanguage {
        catalog().resolve(path.as_bytes(), b"").unwrap()
    }

    /// Default limits without the per-request time budget, which a slow debug build on a
    /// shared CI host can exhaust before a complete parse.
    fn untimed_limits() -> SyntaxLimits {
        SyntaxLimits {
            request_time: Duration::from_mins(10),
            ..SyntaxLimits::default()
        }
    }

    fn annotate(path: &str, source: &str, lines: &[u32]) -> BTreeMap<u32, LineSyntax> {
        let mut annotator = SyntaxAnnotator::new(catalog(), untimed_limits());
        match annotator.annotate(
            AnnotationRequest {
                source_key: source.as_bytes(),
                source: source.as_bytes(),
                language: language(path),
                lines,
            },
            &AtomicBool::new(false),
        ) {
            Annotation::Lines { lines, complete } => {
                assert!(complete);
                lines
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    /// The kind covering `needle`'s first byte on `line`.
    fn kind_of(
        source: &str,
        result: &BTreeMap<u32, LineSyntax>,
        line: u32,
        needle: &str,
    ) -> SyntaxTokenKind {
        let text = source.lines().nth(line as usize - 1).unwrap();
        let at = u32::try_from(text.find(needle).unwrap()).unwrap();
        let LineSyntax::Spans(spans) = &result[&line] else {
            panic!("line {line} is limited");
        };
        spans
            .iter()
            .find(|span| span.start <= at && at < span.end)
            .map_or(SyntaxTokenKind::Plain, |span| span.kind)
    }

    #[test]
    fn required_languages_resolve_from_names_suffixes_and_first_lines() {
        let catalog = catalog();
        let name = |path: &[u8], source: &[u8]| {
            catalog
                .resolve(path, source)
                .and_then(|language| catalog.language_name(language))
                .map(str::to_owned)
        };
        for (path, expected) in [
            ("lib/main.dart", "Dart"),
            ("src/lib.rs", "Rust"),
            ("ui/Main.qml", "QML"),
            ("web/app.js", "JavaScript (Babel)"),
            ("web/app.ts", "TypeScript"),
            ("web/App.tsx", "TypeScriptReact"),
            ("tool/run.py", "Python"),
            ("package.json", "JSON"),
            ("Cargo.toml", "TOML"),
            (".github/ci.yml", "YAML"),
            ("config.yaml", "YAML"),
            ("scripts/build.sh", "Bourne Again Shell (bash)"),
            ("index.html", "HTML"),
            ("style.css", "CSS"),
            ("README.md", "Markdown"),
            ("docker/Dockerfile", "Dockerfile"),
            ("Containerfile", "Dockerfile"),
            ("Makefile", "Makefile"),
            ("GNUmakefile", "Makefile"),
            (".gitignore", "Git Ignore"),
            ("LIB/MAIN.DART", "Dart"),
            ("types/app.d.ts", "TypeScript"),
        ] {
            assert_eq!(
                name(path.as_bytes(), b"").as_deref(),
                Some(expected),
                "{path}"
            );
        }
        assert_eq!(
            name(b"bin/tool", b"#!/usr/bin/env python3\nprint()\n").as_deref(),
            Some("Python")
        );
        assert_eq!(name(b"\xff\xfe/dir/main.rs", b"").as_deref(), Some("Rust"));
        assert_eq!(name(b"notes.unknown-extension", b"hello\n"), None);
        assert_eq!(name(b"notes.txt", b"hello\n"), None);
    }

    #[test]
    fn categories_follow_scope_precedence() {
        let source = "/// A doc comment naming self.\nfn main() {\n    let value: u32 = 0x2A + call(\"text {x}\");\n    #[allow(dead_code)]\n    const LIMIT: usize = 3;\n}\nstruct Point<'a>(&'a str);\n";
        let result = annotate("main.rs", source, &[1, 2, 3, 4, 5, 7]);
        assert_eq!(
            kind_of(source, &result, 1, "self"),
            SyntaxTokenKind::Comment
        );
        assert_eq!(kind_of(source, &result, 2, "fn"), SyntaxTokenKind::Keyword);
        assert_eq!(
            kind_of(source, &result, 2, "main"),
            SyntaxTokenKind::Function
        );
        assert_eq!(kind_of(source, &result, 3, "let"), SyntaxTokenKind::Keyword);
        // The bundled Rust grammar scopes primitive types as storage.
        assert_eq!(kind_of(source, &result, 3, "u32"), SyntaxTokenKind::Keyword);
        assert_eq!(kind_of(source, &result, 3, "0x2A"), SyntaxTokenKind::Number);
        assert_eq!(kind_of(source, &result, 3, "+"), SyntaxTokenKind::Operator);
        assert_eq!(
            kind_of(source, &result, 3, "\"text"),
            SyntaxTokenKind::String
        );
        assert_eq!(kind_of(source, &result, 3, "{x}"), SyntaxTokenKind::String);
        assert_eq!(
            kind_of(source, &result, 3, ";"),
            SyntaxTokenKind::Punctuation
        );
        assert_eq!(
            kind_of(source, &result, 4, "allow"),
            SyntaxTokenKind::Attribute
        );
        assert_eq!(
            kind_of(source, &result, 5, "LIMIT"),
            SyntaxTokenKind::Constant
        );
        assert_eq!(kind_of(source, &result, 3, "value"), SyntaxTokenKind::Plain);
        assert_eq!(kind_of(source, &result, 7, "Point"), SyntaxTokenKind::Type);
    }

    #[test]
    fn current_dart_constructs_have_categories() {
        let source = concat!(
            "final record = (name: 'a', count: 2);\n",
            "final greeting = 'Hello $name, ${count + 1}';\n",
            "final raw = r'\\d+';\n",
            "final block = '''\n",
            "triple $name\n",
            "''';\n",
            "switch (value) { case (int a, _) when a > 0: break; }\n",
            "@override\n",
            "class Widget extends Base {}\n",
        );
        let result = annotate("lib/a.dart", source, &[1, 2, 3, 4, 5, 6, 7, 8, 9]);
        assert_eq!(
            kind_of(source, &result, 1, "final"),
            SyntaxTokenKind::Keyword
        );
        assert_eq!(kind_of(source, &result, 1, "'a'"), SyntaxTokenKind::String);
        assert_eq!(kind_of(source, &result, 1, "2"), SyntaxTokenKind::Number);
        assert_eq!(
            kind_of(source, &result, 2, "$name"),
            SyntaxTokenKind::String
        );
        assert_eq!(kind_of(source, &result, 3, "\\d"), SyntaxTokenKind::String);
        // The second line of a triple-quoted string is only a string because
        // the parser carried its state from the line before.
        assert_eq!(
            kind_of(source, &result, 5, "triple"),
            SyntaxTokenKind::String
        );
        assert_eq!(
            kind_of(source, &result, 7, "switch"),
            SyntaxTokenKind::Keyword
        );
        assert_eq!(
            kind_of(source, &result, 7, "case"),
            SyntaxTokenKind::Keyword
        );
        // The bundled Dart grammar predates patterns: `when` stays plain.
        assert_eq!(kind_of(source, &result, 7, "when"), SyntaxTokenKind::Plain);
        assert_eq!(
            kind_of(source, &result, 8, "override"),
            SyntaxTokenKind::Attribute
        );
        assert_eq!(
            kind_of(source, &result, 9, "class"),
            SyntaxTokenKind::Keyword
        );
        assert_eq!(kind_of(source, &result, 9, "Widget"), SyntaxTokenKind::Type);
    }

    #[test]
    fn hidden_state_from_omitted_lines_reaches_a_distant_line() {
        let mut source = String::from("/* starts here\n");
        for _ in 0..300 {
            source.push_str("still inside the comment\n");
        }
        source.push_str("end */ fn after() {}\n");
        let last = 302;
        let result = annotate("a.rs", &source, &[last]);
        assert_eq!(result.len(), 1);
        assert_eq!(
            kind_of(&source, &result, last, "end"),
            SyntaxTokenKind::Comment
        );
        assert_eq!(
            kind_of(&source, &result, last, "fn"),
            SyntaxTokenKind::Keyword
        );
    }

    #[test]
    fn checkpoint_resumed_output_equals_a_cold_parse() {
        let mut source = String::new();
        for index in 0..2000 {
            if index % 97 == 0 {
                source.push_str("/* a block\ncomment */\n");
            }
            writeln!(source, "let value_{index} = \"s{index}\"; // {index}").unwrap();
        }
        let lines = [5, 600, 601, 1500, 2040];
        let cold = annotate("a.rs", &source, &lines);
        let mut annotator = SyntaxAnnotator::new(catalog(), untimed_limits());
        let request = |lines: &'static [u32]| AnnotationRequest {
            source_key: b"key",
            source: source.as_bytes(),
            language: language("a.rs"),
            lines,
        };
        let cancellation = AtomicBool::new(false);
        let Annotation::Lines { lines: first, .. } =
            annotator.annotate(request(&[2040]), &cancellation)
        else {
            panic!()
        };
        assert!(annotator.retained_checkpoints() > 0);
        let Annotation::Lines { lines: resumed, .. } =
            annotator.annotate(request(&[5, 600, 601, 1500, 2040]), &cancellation)
        else {
            panic!()
        };
        assert_eq!(first[&2040], cold[&2040]);
        assert_eq!(resumed, cold);
        // Checkpoints stay within their bounds.
        assert!(annotator.retained_checkpoints() <= annotator.limits().checkpoints_per_source);
    }

    #[test]
    fn old_and_new_sources_keep_independent_state() {
        let old = "fn a() {}\nlet x = 1;\n";
        let new = "/* opened\nlet x = 1;\n";
        let old_result = annotate("a.rs", old, &[2]);
        let new_result = annotate("a.rs", new, &[2]);
        assert_eq!(
            kind_of(old, &old_result, 2, "let"),
            SyntaxTokenKind::Keyword
        );
        assert_eq!(
            kind_of(new, &new_result, 2, "let"),
            SyntaxTokenKind::Comment
        );
    }

    #[test]
    fn spans_are_byte_ranges_of_the_line_without_its_terminator() {
        let source = "let s = \"é🙂\"; // ü\r\nlet t = 1;";
        let result = annotate("a.rs", source, &[1, 2]);
        for (line, content) in [(1, "let s = \"é🙂\"; // ü"), (2, "let t = 1;")] {
            let LineSyntax::Spans(spans) = &result[&line] else {
                panic!()
            };
            let mut previous = 0;
            for span in spans {
                assert!(span.start < span.end && span.start >= previous);
                assert!(span.end as usize <= content.len());
                assert!(content.is_char_boundary(span.start as usize));
                assert!(content.is_char_boundary(span.end as usize));
                assert_ne!(span.kind, SyntaxTokenKind::Plain);
                previous = span.end;
            }
        }
        assert_eq!(
            kind_of(source, &result, 1, "// ü"),
            SyntaxTokenKind::Comment
        );
        let LineSyntax::Spans(spans) = &result[&1] else {
            panic!()
        };
        assert_eq!(
            spans.last().unwrap().end as usize,
            "let s = \"é🙂\"; // ü".len()
        );
    }

    #[test]
    fn bounds_produce_explicit_plain_outcomes() {
        let mut annotator = SyntaxAnnotator::new(
            catalog(),
            SyntaxLimits {
                source_bytes: 64,
                line_bytes: 16,
                spans_per_line: 3,
                ..SyntaxLimits::default()
            },
        );
        let cancellation = AtomicBool::new(false);
        let rust = language("a.rs");
        let request = |source: &'static [u8], lines: &'static [u32]| AnnotationRequest {
            source_key: source,
            source,
            language: rust,
            lines,
        };
        assert_eq!(
            annotator.annotate(request(&[b'a'; 65], &[1]), &cancellation),
            Annotation::TooLarge
        );
        assert_eq!(
            annotator.annotate(request(b"let a = \xff;\n", &[1]), &cancellation),
            Annotation::InvalidEncoding
        );
        // A long line makes it and every later line plain; earlier lines keep spans.
        let Annotation::Lines { lines, .. } = annotator.annotate(
            request(b"let a;\nlet b = 1234567890;\nlet c;\n", &[1, 2, 3]),
            &cancellation,
        ) else {
            panic!()
        };
        assert!(matches!(lines[&1], LineSyntax::Spans(_)));
        assert_eq!(lines[&2], LineSyntax::LimitReached);
        assert_eq!(lines[&3], LineSyntax::LimitReached);
        // Too many spans make one line plain.
        let Annotation::Lines { lines, .. } =
            annotator.annotate(request(b"a(1,2);\n", &[1]), &cancellation)
        else {
            panic!()
        };
        assert_eq!(lines[&1], LineSyntax::LimitReached);
        // Lines the source does not have are left out.
        let Annotation::Lines { lines, complete } =
            annotator.annotate(request(b"let a;\n", &[0, 2]), &cancellation)
        else {
            panic!()
        };
        assert!(lines.is_empty() && complete);
    }

    #[test]
    fn cancellation_and_deadline_stop_between_lines() {
        let source = "let a = 1;\n".repeat(5000);
        let mut annotator = SyntaxAnnotator::new(catalog(), SyntaxLimits::default());
        let request = AnnotationRequest {
            source_key: b"k",
            source: source.as_bytes(),
            language: language("a.rs"),
            lines: &[4999],
        };
        assert_eq!(
            annotator.annotate(request, &AtomicBool::new(true)),
            Annotation::Canceled
        );
        let mut hurried = SyntaxAnnotator::new(
            catalog(),
            SyntaxLimits {
                request_time: Duration::ZERO,
                ..SyntaxLimits::default()
            },
        );
        assert_eq!(
            hurried.annotate(request, &AtomicBool::new(false)),
            Annotation::Lines {
                lines: BTreeMap::new(),
                complete: false
            }
        );
    }

    #[test]
    fn a_rename_between_languages_keeps_separate_checkpoints() {
        let source = "# heading\n".repeat(600);
        let mut annotator = SyntaxAnnotator::new(catalog(), SyntaxLimits::default());
        let cancellation = AtomicBool::new(false);
        let run = |annotator: &mut SyntaxAnnotator, path: &str| {
            let Annotation::Lines { lines, .. } = annotator.annotate(
                AnnotationRequest {
                    source_key: b"same-blob",
                    source: source.as_bytes(),
                    language: language(path),
                    lines: &[600],
                },
                &cancellation,
            ) else {
                panic!()
            };
            lines
        };
        let markdown = run(&mut annotator, "a.md");
        let python = run(&mut annotator, "a.py");
        assert_eq!(
            kind_of(&source, &markdown, 600, "heading"),
            SyntaxTokenKind::Keyword
        );
        assert_eq!(
            kind_of(&source, &python, 600, "heading"),
            SyntaxTokenKind::Comment
        );
    }

    #[test]
    fn grammar_notices_name_each_licence_once() {
        let notices = grammar_notices_markdown();
        assert!(notices.starts_with("# Syntax grammars"));
        let licenses = two_face::acknowledgement::listing();
        assert!(!licenses.for_syntaxes().is_empty());
        for license in licenses.for_syntaxes() {
            let path = license.rel_path.to_string_lossy().replace('\\', "/");
            assert_eq!(
                notices.matches(&format!("\n## {path}\n")).count(),
                1,
                "{path}"
            );
        }
    }

    #[test]
    fn catalog_fingerprint_is_stable() {
        assert_eq!(
            catalog().fingerprint(),
            SyntaxCatalog::bundled().fingerprint()
        );
        assert_eq!(SyntaxTokenKind::ALL.len(), 13);
    }
}

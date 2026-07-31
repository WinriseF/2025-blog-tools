use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
};

const PATCH_CACHE_LIMIT: usize = 32 * 1024 * 1024;
const PATCH_CACHE_MAX_ITEMS: usize = 3;
pub const PATCH_PREVIEW_LIMIT: usize = 2 * 1024 * 1024;
const METADATA_LINE_LIMIT: usize = 64 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SvnPatchBody {
    Retained(String),
    Deferred,
    TooLarge,
}

#[derive(Clone, Debug)]
pub struct SvnPatchFile {
    pub body: SvnPatchBody,
    pub additions: usize,
    pub deletions: usize,
    pub is_binary: bool,
}

#[derive(Debug)]
pub struct SvnPatchSet {
    pub files: HashMap<String, SvnPatchFile>,
    bytes: usize,
}

impl SvnPatchSet {
    pub fn parse(bytes: &[u8], normalize: impl Fn(&str) -> String) -> Self {
        let mut parser = SvnPatchParser::new(normalize);
        parser.push(bytes);
        parser.finish()
    }

    pub fn file(&self, path: &str) -> Option<&SvnPatchFile> {
        self.files.get(path)
    }
}

pub struct SvnPatchParser<F> {
    normalize: F,
    files: HashMap<String, SvnPatchFile>,
    current: Option<CurrentFile>,
    line: Vec<u8>,
    line_overflow: bool,
    line_has_nul: bool,
    retained_bytes: usize,
    file_limit: usize,
    total_limit: usize,
}

struct CurrentFile {
    path: String,
    patch: Vec<u8>,
    patch_too_large: bool,
    additions: usize,
    deletions: usize,
    is_binary: bool,
    in_hunk: bool,
}

impl<F: Fn(&str) -> String> SvnPatchParser<F> {
    pub fn new(normalize: F) -> Self {
        Self::with_limits(normalize, PATCH_PREVIEW_LIMIT, PATCH_CACHE_LIMIT)
    }

    fn with_limits(normalize: F, file_limit: usize, total_limit: usize) -> Self {
        Self {
            normalize,
            files: HashMap::new(),
            current: None,
            line: Vec::new(),
            line_overflow: false,
            line_has_nul: false,
            retained_bytes: 0,
            file_limit,
            total_limit,
        }
    }

    pub fn push(&mut self, bytes: &[u8]) {
        for segment in bytes.split_inclusive(|byte| *byte == b'\n') {
            self.line_has_nul |= segment.contains(&0);
            let line_limit = self.file_limit.saturating_add(1).max(METADATA_LINE_LIMIT);
            if self.line.len() < line_limit {
                let retained = (line_limit - self.line.len()).min(segment.len());
                self.line.extend_from_slice(&segment[..retained]);
                self.line_overflow |= retained < segment.len();
            } else {
                self.line_overflow = true;
            }
            if segment.last() == Some(&b'\n') {
                self.finish_line();
            }
        }
    }

    pub fn finish(mut self) -> SvnPatchSet {
        if !self.line.is_empty() || self.line_overflow {
            self.finish_line();
        }
        self.finish_file();
        SvnPatchSet {
            files: self.files,
            bytes: self.retained_bytes,
        }
    }

    fn finish_line(&mut self) {
        let line = std::mem::take(&mut self.line);
        let trimmed = trim_line_end(&line);
        if let Some(path) = trimmed.strip_prefix(b"Index: ") {
            let path = String::from_utf8_lossy(path);
            let path = (self.normalize)(&path);
            self.finish_file();
            self.current = Some(CurrentFile {
                path,
                patch: Vec::new(),
                patch_too_large: false,
                additions: 0,
                deletions: 0,
                is_binary: false,
                in_hunk: false,
            });
        }
        if let Some(current) = self.current.as_mut() {
            current.is_binary |= self.line_has_nul || marks_binary(trimmed);
            if current.is_binary {
                current.patch.clear();
            } else if self.line_overflow
                || current.patch.len().saturating_add(line.len()) > self.file_limit
            {
                current.patch.clear();
                current.patch_too_large = true;
            } else if !current.patch_too_large {
                current.patch.extend_from_slice(&line);
            }
            if trimmed.starts_with(b"@@") {
                current.in_hunk = true;
            } else if trimmed.starts_with(b"Property changes on:") {
                current.in_hunk = false;
            } else if current.in_hunk {
                if trimmed.starts_with(b"+") {
                    current.additions += 1;
                } else if trimmed.starts_with(b"-") {
                    current.deletions += 1;
                }
            }
        }
        self.line = line;
        self.line.clear();
        self.line_overflow = false;
        self.line_has_nul = false;
    }

    fn finish_file(&mut self) {
        let Some(current) = self.current.take() else {
            return;
        };
        let body = if current.is_binary {
            SvnPatchBody::Deferred
        } else if current.patch_too_large {
            SvnPatchBody::TooLarge
        } else {
            let patch = String::from_utf8(current.patch)
                .unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned());
            if patch.len() > self.file_limit {
                SvnPatchBody::TooLarge
            } else if self.retained_bytes.saturating_add(patch.len()) > self.total_limit {
                SvnPatchBody::Deferred
            } else {
                self.retained_bytes += patch.len();
                SvnPatchBody::Retained(patch)
            }
        };
        self.files.insert(
            current.path,
            SvnPatchFile {
                body,
                additions: current.additions,
                deletions: current.deletions,
                is_binary: current.is_binary,
            },
        );
    }
}

fn trim_line_end(mut line: &[u8]) -> &[u8] {
    if let Some(trimmed) = line.strip_suffix(b"\n") {
        line = trimmed;
    }
    if let Some(trimmed) = line.strip_suffix(b"\r") {
        line = trimmed;
    }
    line
}

fn marks_binary(line: &[u8]) -> bool {
    if line.starts_with(b"Cannot display:") {
        return true;
    }
    line.strip_prefix(b"svn:mime-type = ")
        .and_then(|mime| std::str::from_utf8(mime).ok())
        .is_some_and(|mime| !mime.trim().starts_with("text/"))
}

pub fn added_file_patch(path: &str, bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes).replace("\r\n", "\n");
    let mut patch = format!("Index: {path}\n--- /dev/null\n+++ b/{path}\n");
    if text.is_empty() {
        return patch;
    }
    patch.push_str(&format!("@@ -0,0 +1,{} @@\n", text.lines().count()));
    for line in text.split_inclusive('\n') {
        patch.push('+');
        patch.push_str(line);
    }
    if !text.is_empty() && !text.ends_with('\n') {
        patch.push('\n');
        patch.push_str("\\ No newline at end of file\n");
    }
    patch
}

pub struct SvnPatchCache {
    items: HashMap<(Option<u64>, Option<u64>), Arc<SvnPatchSet>>,
    order: VecDeque<(Option<u64>, Option<u64>)>,
    bytes: usize,
}

impl SvnPatchCache {
    pub fn new() -> Self {
        Self {
            items: HashMap::new(),
            order: VecDeque::new(),
            bytes: 0,
        }
    }

    pub fn get(&mut self, key: &(Option<u64>, Option<u64>)) -> Option<Arc<SvnPatchSet>> {
        let value = self.items.get(key).cloned();
        if value.is_some() {
            self.order.retain(|item| item != key);
            self.order.push_back(*key);
        }
        value
    }

    pub fn insert(&mut self, key: (Option<u64>, Option<u64>), value: Arc<SvnPatchSet>) {
        if value.bytes > PATCH_CACHE_LIMIT {
            return;
        }
        if let Some(previous) = self.items.remove(&key) {
            self.bytes = self.bytes.saturating_sub(previous.bytes);
            self.order.retain(|item| item != &key);
        }
        while self.items.len() >= PATCH_CACHE_MAX_ITEMS
            || self.bytes.saturating_add(value.bytes) > PATCH_CACHE_LIMIT
        {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if let Some(previous) = self.items.remove(&oldest) {
                self.bytes = self.bytes.saturating_sub(previous.bytes);
            }
        }
        self.bytes = self.bytes.saturating_add(value.bytes);
        self.order.push_back(key);
        self.items.insert(key, value);
    }

    pub fn clear(&mut self) {
        self.items.clear();
        self.order.clear();
        self.bytes = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::{SvnPatchBody, SvnPatchCache, SvnPatchParser, SvnPatchSet, added_file_patch};
    use std::sync::Arc;

    fn retained(body: &SvnPatchBody) -> &str {
        match body {
            SvnPatchBody::Retained(value) => value,
            SvnPatchBody::Deferred | SvnPatchBody::TooLarge => panic!("patch was not retained"),
        }
    }

    #[test]
    fn parses_hunks_properties_and_invalid_utf8_without_failing() {
        let mut diff = b"Index: src/main.rs\n@@ -1,3 +1,4 @@\n keep\n-old\n+new\n+++plus\nProperty changes on: src/main.rs\n+property\n".to_vec();
        diff.extend_from_slice(&[0xff, b'\n']);
        let patch = SvnPatchSet::parse(&diff, str::to_owned);
        let file = patch.file("src/main.rs").unwrap();

        assert_eq!((file.additions, file.deletions), (2, 1));
        assert!(!file.is_binary);
        assert!(retained(&file.body).contains('\u{fffd}'));
    }

    #[test]
    fn parses_chunk_boundaries_without_publishing_partial_counts() {
        let bytes = b"Index: src/main.rs\r\n@@ -1,2 +1,2 @@\r\n-old\r\n+new\r\n";
        let mut parser = SvnPatchParser::with_limits(str::to_owned, 1024, 1024);
        for chunk in bytes.chunks(3) {
            parser.push(chunk);
        }
        let patch = parser.finish();
        let file = patch.file("src/main.rs").unwrap();

        assert_eq!((file.additions, file.deletions), (1, 1));
        assert_eq!(retained(&file.body).as_bytes(), bytes);
    }

    #[test]
    fn discards_oversized_bodies_but_keeps_line_counts() {
        let bytes = b"Index: huge.txt\n@@ -1 +1 @@\n-old\n+abcdefghijklmnopqrstuvwxyz\n";
        let mut parser = SvnPatchParser::with_limits(str::to_owned, 32, 64);
        parser.push(bytes);
        let patch = parser.finish();
        let file = patch.file("huge.txt").unwrap();

        assert_eq!((file.additions, file.deletions), (1, 1));
        assert_eq!(file.body, SvnPatchBody::TooLarge);
    }

    #[test]
    fn defers_patch_text_after_the_global_memory_budget() {
        let bytes = b"Index: one.txt\n@@ -1 +1 @@\n-a\n+b\nIndex: two.txt\n@@ -1 +1 @@\n-c\n+d\n";
        let mut parser = SvnPatchParser::with_limits(str::to_owned, 1024, 50);
        parser.push(bytes);
        let patch = parser.finish();

        assert!(matches!(
            patch.file("one.txt").unwrap().body,
            SvnPatchBody::Retained(_)
        ));
        assert_eq!(patch.file("two.txt").unwrap().body, SvnPatchBody::Deferred);
        assert_eq!(
            (
                patch.file("two.txt").unwrap().additions,
                patch.file("two.txt").unwrap().deletions
            ),
            (1, 1)
        );
    }

    #[test]
    fn detects_svn_binary_patch_metadata() {
        let patch = SvnPatchSet::parse(
            b"Index: image.png\nCannot display: file marked as a binary type.\nsvn:mime-type = image/png\n",
            str::to_owned,
        );

        assert!(patch.file("image.png").unwrap().is_binary);

        let nul_patch = SvnPatchSet::parse(
            b"Index: raw.dat\n@@ -1 +1 @@\n-old\n+new\0value\n",
            str::to_owned,
        );
        assert!(nul_patch.file("raw.dat").unwrap().is_binary);
    }

    #[test]
    fn patch_cache_reuses_recent_revision_ranges() {
        let patch = Arc::new(SvnPatchSet::parse(
            b"Index: file.txt\n@@ -1 +1 @@\n-old\n+new\n",
            str::to_owned,
        ));
        let mut cache = SvnPatchCache::new();
        cache.insert((Some(1), Some(2)), Arc::clone(&patch));

        assert!(Arc::ptr_eq(
            &cache.get(&(Some(1), Some(2))).unwrap(),
            &patch
        ));
    }

    #[test]
    fn creates_added_patch() {
        let added = added_file_patch("new.txt", b"one\ntwo");
        assert!(added.contains("@@ -0,0 +1,2 @@\n+one\n+two\n"));
        assert!(!added_file_patch("empty.txt", b"").contains("@@"));
    }
}

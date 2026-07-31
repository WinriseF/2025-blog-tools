use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
};

const PATCH_CACHE_LIMIT: usize = 32 * 1024 * 1024;
const PATCH_CACHE_MAX_ITEMS: usize = 3;

#[derive(Clone, Debug)]
pub struct SvnPatchFile {
    pub patch: String,
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
        let text = String::from_utf8_lossy(bytes);
        let mut files = HashMap::new();
        let mut current_path: Option<String> = None;
        let mut current_patch = String::new();
        let mut additions = 0;
        let mut deletions = 0;
        let mut is_binary = false;
        let mut in_hunk = false;

        for segment in text.split_inclusive('\n') {
            let line = segment.trim_end_matches(['\r', '\n']);
            if let Some(path) = line.strip_prefix("Index: ") {
                finish_file(
                    &mut files,
                    current_path.take(),
                    std::mem::take(&mut current_patch),
                    additions,
                    deletions,
                    is_binary,
                );
                current_path = Some(normalize(path));
                additions = 0;
                deletions = 0;
                is_binary = false;
                in_hunk = false;
            }
            if current_path.is_some() {
                current_patch.push_str(segment);
            }
            if line.starts_with("@@") {
                in_hunk = true;
            } else if line.starts_with("Property changes on:") {
                in_hunk = false;
            } else if in_hunk {
                if line.starts_with('+') {
                    additions += 1;
                } else if line.starts_with('-') {
                    deletions += 1;
                }
            }
            if line.contains('\0')
                || line.starts_with("Cannot display:")
                || line
                    .strip_prefix("svn:mime-type = ")
                    .is_some_and(|mime| !mime.trim().starts_with("text/"))
            {
                is_binary = true;
            }
        }
        finish_file(
            &mut files,
            current_path,
            current_patch,
            additions,
            deletions,
            is_binary,
        );
        let bytes = files.values().map(|file| file.patch.len()).sum();
        Self { files, bytes }
    }

    pub fn file(&self, path: &str) -> Option<&SvnPatchFile> {
        self.files.get(path)
    }
}

fn finish_file(
    files: &mut HashMap<String, SvnPatchFile>,
    path: Option<String>,
    patch: String,
    additions: usize,
    deletions: usize,
    is_binary: bool,
) {
    if let Some(path) = path {
        files.insert(
            path,
            SvnPatchFile {
                patch,
                additions,
                deletions,
                is_binary,
            },
        );
    }
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

    pub fn get(
        &mut self,
        key: &(Option<u64>, Option<u64>),
    ) -> Option<Arc<SvnPatchSet>> {
        let value = self.items.get(key).cloned();
        if value.is_some() {
            self.order.retain(|item| item != key);
            self.order.push_back(*key);
        }
        value
    }

    pub fn insert(
        &mut self,
        key: (Option<u64>, Option<u64>),
        value: Arc<SvnPatchSet>,
    ) {
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
    use super::{SvnPatchCache, SvnPatchSet, added_file_patch};
    use std::sync::Arc;

    #[test]
    fn parses_hunks_properties_and_invalid_utf8_without_failing() {
        let mut diff = b"Index: src/main.rs\n@@ -1,3 +1,4 @@\n keep\n-old\n+new\n+++plus\nProperty changes on: src/main.rs\n+property\n".to_vec();
        diff.extend_from_slice(&[0xff, b'\n']);
        let patch = SvnPatchSet::parse(&diff, str::to_owned);
        let file = patch.file("src/main.rs").unwrap();

        assert_eq!((file.additions, file.deletions), (2, 1));
        assert!(!file.is_binary);
        assert!(file.patch.contains('\u{fffd}'));
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

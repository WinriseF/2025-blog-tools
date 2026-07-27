use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FileSelection {
    mode: FileSelectionMode,
    ranges: Vec<[u32; 2]>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum FileSelectionMode {
    Include,
    Exclude,
}

impl FileSelection {
    pub(super) fn resolve(self, total_files: usize) -> anyhow::Result<Vec<u32>> {
        let mut selected = vec![self.mode == FileSelectionMode::Exclude; total_files];
        let mut previous_end = None;
        for [start, end] in self.ranges {
            anyhow::ensure!(start <= end, "file selection range is invalid");
            anyhow::ensure!(
                (end as usize) < total_files,
                "file selection is out of range"
            );
            anyhow::ensure!(
                previous_end.is_none_or(|previous| start > previous),
                "file selection ranges overlap"
            );
            selected[start as usize..=end as usize].fill(self.mode == FileSelectionMode::Include);
            previous_end = Some(end);
        }
        Ok(selected
            .into_iter()
            .enumerate()
            .filter_map(|(file_id, selected)| selected.then_some(file_id as u32))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::{FileSelection, FileSelectionMode};

    #[test]
    fn resolves_compact_ranges() {
        let included = FileSelection {
            mode: FileSelectionMode::Include,
            ranges: vec![[1, 3], [7, 7]],
        }
        .resolve(9)
        .expect("include ranges");
        assert_eq!(included, vec![1, 2, 3, 7]);

        let excluded = FileSelection {
            mode: FileSelectionMode::Exclude,
            ranges: vec![[1, 3], [7, 7]],
        }
        .resolve(9)
        .expect("exclude ranges");
        assert_eq!(excluded, vec![0, 4, 5, 6, 8]);
    }

    #[test]
    fn rejects_invalid_ranges() {
        for ranges in [vec![[2, 1]], vec![[1, 3], [3, 4]], vec![[5, 5]]] {
            assert!(
                FileSelection {
                    mode: FileSelectionMode::Include,
                    ranges,
                }
                .resolve(5)
                .is_err()
            );
        }
    }
}

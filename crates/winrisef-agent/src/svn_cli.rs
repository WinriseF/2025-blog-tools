use std::{
    env, io,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

use quick_xml::de::from_str;
use serde::Deserialize;
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::{Child, Command},
    sync::OnceCell,
};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(45);
const DIFF_COMMAND_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_STDOUT_BYTES: usize = 16 * 1024 * 1024;
const MAX_DIFF_STREAM_BYTES: usize = 512 * 1024 * 1024;
const MAX_STDERR_BYTES: usize = 512 * 1024;
const STREAM_BUFFER_BYTES: usize = 64 * 1024;
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
static SVN_CLI: OnceCell<SvnCli> = OnceCell::const_new();

#[derive(Debug, Error)]
pub enum SvnError {
    #[error("SVN command-line client was not found")]
    NotInstalled,
    #[error("SVN working copy could not be read: {message}")]
    InvalidWorkingCopy { message: String },
    #[error("SVN command timed out")]
    Timeout,
    #[error("SVN command failed: {message}")]
    CommandFailed { message: String },
    #[error("SVN returned malformed XML")]
    InvalidXml,
    #[error("SVN output was not valid UTF-8")]
    InvalidUtf8,
    #[error("SVN path is invalid")]
    InvalidPath,
    #[error("SVN data exceeded the allowed limit")]
    OutputTooLarge,
    #[error("SVN diff exceeded the 512 MiB processing limit")]
    DiffTooLarge,
    #[error("SVN I/O failed: {0}")]
    Io(#[from] io::Error),
}

#[derive(Clone, Debug)]
pub struct SvnCli {
    path: PathBuf,
}

#[derive(Clone, Debug)]
pub struct SvnRepositoryInfo {
    pub root_path: PathBuf,
    pub display_name: String,
    pub relative_url: String,
    pub repository_root_url: String,
    pub working_revision: u64,
    pub depth: Option<String>,
}

#[derive(Clone, Debug)]
pub struct SvnStatusEntry {
    pub path: String,
    pub item: String,
    pub props: String,
    pub revision: Option<u64>,
    pub tree_conflicted: bool,
}

#[derive(Clone, Debug)]
pub struct SvnLogEntry {
    pub revision: u64,
    pub author: String,
    pub date: String,
    pub message: String,
}

#[derive(Clone, Debug)]
pub struct SvnDiffSummaryEntry {
    pub path: String,
    pub kind: String,
    pub item: String,
    pub props: String,
}

impl SvnCli {
    pub async fn discover() -> Result<Self, SvnError> {
        let cli = SVN_CLI
            .get_or_try_init(|| async {
                find_executable()
                    .map(|path| SvnCli { path })
                    .ok_or(SvnError::NotInstalled)
            })
            .await?;
        Ok(cli.clone())
    }

    pub async fn discover_working_copy(
        &self,
        selected_path: &Path,
    ) -> Result<Option<SvnRepositoryInfo>, SvnError> {
        let selected_path = canonical_existing_path(selected_path)?;
        let output = match self
            .run_in(
                &["info", "--xml", "--depth", "empty", "."],
                Some(&selected_path),
                MAX_STDOUT_BYTES,
            )
            .await
        {
            Ok(output) => output,
            Err(SvnError::CommandFailed { message }) if not_working_copy_error(&message) => {
                return Ok(None);
            }
            Err(SvnError::CommandFailed { message }) => {
                return Err(SvnError::CommandFailed { message });
            }
            Err(error) => return Err(error),
        };
        let document: InfoDocument = parse_xml(&output)?;
        let Some(mut entry) = document.entries.into_iter().next() else {
            return Ok(None);
        };
        let root_path = entry
            .wc_info
            .as_ref()
            .and_then(|info| info.wcroot_abspath.clone())
            .map(PathBuf::from)
            .unwrap_or_else(|| selected_path.clone());
        if root_path != selected_path {
            let root_output = self
                .run_in(
                    &["info", "--xml", "--depth", "empty", "."],
                    Some(&root_path),
                    MAX_STDOUT_BYTES,
                )
                .await?;
            if let Some(root_entry) = parse_xml::<InfoDocument>(&root_output)?
                .entries
                .into_iter()
                .next()
            {
                entry = root_entry;
            }
        }
        let repository = entry
            .repository
            .ok_or_else(|| SvnError::InvalidWorkingCopy {
                message: "working copy has no repository metadata".to_owned(),
            })?;
        let display_name = root_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("SVN working copy")
            .to_owned();
        Ok(Some(SvnRepositoryInfo {
            root_path,
            display_name,
            relative_url: entry.relative_url.unwrap_or_default(),
            repository_root_url: repository.root,
            working_revision: entry.revision.unwrap_or(0),
            depth: entry.wc_info.and_then(|info| info.depth),
        }))
    }

    pub async fn status(&self, root_path: &Path) -> Result<Vec<SvnStatusEntry>, SvnError> {
        let output = self
            .run_owned_in(
                vec![
                    "status".to_owned(),
                    "--xml".to_owned(),
                    "--verbose".to_owned(),
                    "--ignore-externals".to_owned(),
                    "--depth".to_owned(),
                    "infinity".to_owned(),
                    ".".to_owned(),
                ],
                root_path,
                MAX_STDOUT_BYTES,
            )
            .await?;
        let document: StatusDocument = parse_xml(&output)?;
        Ok(document
            .targets
            .into_iter()
            .flat_map(|target| target.entries)
            .map(|entry| {
                let status = entry.wc_status.unwrap_or_default();
                SvnStatusEntry {
                    path: entry.path,
                    item: status.item.unwrap_or_else(|| "normal".to_owned()),
                    props: normalize_props(status.props.as_deref().unwrap_or("none")),
                    revision: status.revision.and_then(|value| value.parse().ok()),
                    tree_conflicted: status.tree_conflicted,
                }
            })
            .collect())
    }

    pub async fn head_revision(&self, root_path: &Path) -> Result<u64, SvnError> {
        let output = self
            .run_owned_in(
                vec![
                    "info".to_owned(),
                    "--xml".to_owned(),
                    "-r".to_owned(),
                    "HEAD".to_owned(),
                    ".".to_owned(),
                ],
                root_path,
                MAX_STDOUT_BYTES,
            )
            .await?;
        let document: InfoDocument = parse_xml(&output)?;
        document
            .entries
            .into_iter()
            .next()
            .and_then(|entry| entry.revision)
            .ok_or_else(|| SvnError::InvalidWorkingCopy {
                message: "SVN HEAD revision was not returned".to_owned(),
            })
    }

    pub async fn log(
        &self,
        root_path: &Path,
        start_revision: u64,
        limit: usize,
    ) -> Result<Vec<SvnLogEntry>, SvnError> {
        let args = vec![
            "log".to_owned(),
            "--xml".to_owned(),
            "--limit".to_owned(),
            limit.to_string(),
            "-r".to_owned(),
            format!("{start_revision}:1"),
            ".".to_owned(),
        ];
        let output = self.run_owned_in(args, root_path, MAX_STDOUT_BYTES).await?;
        let document: LogDocument = parse_xml(&output)?;
        Ok(document
            .entries
            .into_iter()
            .map(|entry| SvnLogEntry {
                revision: entry.revision,
                author: entry.author.unwrap_or_default(),
                date: entry.date.unwrap_or_default(),
                message: entry.msg.unwrap_or_default(),
            })
            .collect())
    }

    pub async fn cat(
        &self,
        root_path: &Path,
        target: &str,
        revision: Option<u64>,
        limit: usize,
    ) -> Result<Vec<u8>, SvnError> {
        let mut args = vec!["cat".to_owned()];
        if let Some(revision) = revision {
            args.extend(["-r".to_owned(), revision.to_string()]);
        }
        args.push("--".to_owned());
        args.push(relative_target(target)?);
        self.run_owned_in(args, root_path, limit.min(MAX_STDOUT_BYTES))
            .await
    }

    pub async fn diff_summarize(
        &self,
        root_path: &Path,
        old_revision: Option<u64>,
        new_revision: Option<u64>,
    ) -> Result<Vec<SvnDiffSummaryEntry>, SvnError> {
        let mut args = vec![
            "diff".to_owned(),
            "--xml".to_owned(),
            "--summarize".to_owned(),
        ];
        if let Some(old_revision) = old_revision {
            args.push("-r".to_owned());
            args.push(match new_revision {
                Some(new_revision) => format!("{old_revision}:{new_revision}"),
                None => old_revision.to_string(),
            });
        }
        args.push(".".to_owned());
        let output = self.run_owned_in(args, root_path, MAX_STDOUT_BYTES).await?;
        let document: DiffDocument = parse_xml(&output)?;
        Ok(document
            .paths
            .map(|paths| paths.items)
            .unwrap_or_default()
            .into_iter()
            .map(|path| SvnDiffSummaryEntry {
                path: path.value,
                kind: path.kind.unwrap_or_else(|| "file".to_owned()),
                item: path.item.unwrap_or_else(|| "modified".to_owned()),
                props: path.props.unwrap_or_else(|| "none".to_owned()),
            })
            .collect())
    }

    pub async fn diff_patch_stream(
        &self,
        root_path: &Path,
        old_revision: Option<u64>,
        new_revision: Option<u64>,
        on_chunk: impl FnMut(&[u8]) -> Result<(), SvnError>,
    ) -> Result<(), SvnError> {
        let mut args = diff_patch_args(old_revision, new_revision);
        args.push(".".to_owned());
        let mut command = self.command();
        command.args(args);
        run_streaming_command(command, root_path, on_chunk).await
    }

    pub async fn diff_file_patch(
        &self,
        root_path: &Path,
        old_revision: Option<u64>,
        new_revision: Option<u64>,
        target: &str,
        peg_revision: Option<u64>,
        limit: usize,
    ) -> Result<Vec<u8>, SvnError> {
        let mut args = diff_patch_args(old_revision, new_revision);
        args.push("--".to_owned());
        args.push(relative_target_at(target, peg_revision)?);
        self.run_owned_in(args, root_path, limit.min(MAX_STDOUT_BYTES))
            .await
    }

    async fn run_owned_in(
        &self,
        args: Vec<String>,
        current_dir: &Path,
        stdout_limit: usize,
    ) -> Result<Vec<u8>, SvnError> {
        let mut command = self.command();
        command.args(args);
        run_command(command, Some(current_dir), stdout_limit).await
    }

    async fn run_in(
        &self,
        args: &[&str],
        current_dir: Option<&Path>,
        stdout_limit: usize,
    ) -> Result<Vec<u8>, SvnError> {
        let mut command = self.command();
        command.args(args);
        run_command(command, current_dir, stdout_limit).await
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.path);
        command.args(["--non-interactive", "--no-auth-cache"]);
        #[cfg(windows)]
        command.as_std_mut().creation_flags(CREATE_NO_WINDOW);
        command
    }
}

fn diff_patch_args(old_revision: Option<u64>, new_revision: Option<u64>) -> Vec<String> {
    let mut args = vec!["diff".to_owned(), "--git".to_owned()];
    if let Some(old_revision) = old_revision {
        args.push("-r".to_owned());
        args.push(match new_revision {
            Some(new_revision) => format!("{old_revision}:{new_revision}"),
            None => old_revision.to_string(),
        });
    }
    args
}

async fn run_command(
    mut command: Command,
    current_dir: Option<&Path>,
    stdout_limit: usize,
) -> Result<Vec<u8>, SvnError> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(current_dir) = current_dir {
        command.current_dir(current_dir);
    }
    let mut child = command.spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("missing SVN stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("missing SVN stderr"))?;
    let result = tokio::time::timeout(COMMAND_TIMEOUT, async {
        tokio::try_join!(
            async { child.wait().await.map_err(SvnError::Io) },
            read_limited(stdout, stdout_limit),
            read_limited(stderr, MAX_STDERR_BYTES),
        )
    })
    .await;
    let (status, stdout, stderr) = match result {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => {
            terminate(&mut child).await;
            return Err(error);
        }
        Err(_) => {
            terminate(&mut child).await;
            return Err(SvnError::Timeout);
        }
    };
    if !status.success() {
        return Err(SvnError::CommandFailed {
            message: command_error_message(&stderr),
        });
    }
    Ok(stdout)
}

async fn run_streaming_command(
    mut command: Command,
    current_dir: &Path,
    on_chunk: impl FnMut(&[u8]) -> Result<(), SvnError>,
) -> Result<(), SvnError> {
    command
        .current_dir(current_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("missing SVN stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("missing SVN stderr"))?;
    let result = tokio::time::timeout(DIFF_COMMAND_TIMEOUT, async {
        tokio::try_join!(
            async { child.wait().await.map_err(SvnError::Io) },
            read_streamed(stdout, MAX_DIFF_STREAM_BYTES, on_chunk),
            read_limited(stderr, MAX_STDERR_BYTES),
        )
    })
    .await;
    let (status, (), stderr) = match result {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => {
            terminate(&mut child).await;
            return Err(error);
        }
        Err(_) => {
            terminate(&mut child).await;
            return Err(SvnError::Timeout);
        }
    };
    if !status.success() {
        return Err(SvnError::CommandFailed {
            message: command_error_message(&stderr),
        });
    }
    Ok(())
}

async fn terminate(child: &mut Child) {
    let _ = child.kill().await;
}

fn find_executable() -> Option<PathBuf> {
    let name = if cfg!(windows) { "svn.exe" } else { "svn" };
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|path| path.join(name))
        .find_map(|path| path.is_file().then(|| path.canonicalize().ok()).flatten())
}

fn canonical_existing_path(path: &Path) -> Result<PathBuf, SvnError> {
    path.canonicalize().map_err(|_| SvnError::InvalidPath)
}

fn relative_target(value: &str) -> Result<String, SvnError> {
    relative_target_at(value, None)
}

fn relative_target_at(value: &str, peg_revision: Option<u64>) -> Result<String, SvnError> {
    if value.is_empty() || value.contains('\0') || value.contains('\n') || value.contains('\r') {
        return Err(SvnError::InvalidPath);
    }
    let target = if cfg!(windows) {
        value.replace('\\', "/")
    } else {
        value.to_owned()
    };
    let path = Path::new(&target);
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        })
    {
        return Err(SvnError::InvalidPath);
    }
    Ok(match peg_revision {
        Some(revision) => format!("{target}@{revision}"),
        None => format!("{target}@"),
    })
}

fn normalize_props(value: &str) -> String {
    if value == "normal" {
        "none".to_owned()
    } else {
        value.to_owned()
    }
}

fn command_error_message(stderr: &[u8]) -> String {
    let message = String::from_utf8_lossy(stderr)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .take(8)
        .collect::<Vec<_>>()
        .join("\n");
    if message.is_empty() {
        "SVN command failed".to_owned()
    } else {
        message.chars().take(2048).collect()
    }
}

fn not_working_copy_error(message: &str) -> bool {
    message.contains("E155007")
}

async fn read_limited<R: AsyncRead + Unpin>(
    mut reader: R,
    limit: usize,
) -> Result<Vec<u8>, SvnError> {
    let mut result = Vec::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            return Ok(result);
        }
        if result.len().saturating_add(count) > limit {
            return Err(SvnError::OutputTooLarge);
        }
        result.extend_from_slice(&buffer[..count]);
    }
}

async fn read_streamed<R: AsyncRead + Unpin>(
    mut reader: R,
    limit: usize,
    mut on_chunk: impl FnMut(&[u8]) -> Result<(), SvnError>,
) -> Result<(), SvnError> {
    let mut total = 0_usize;
    let mut buffer = [0_u8; STREAM_BUFFER_BYTES];
    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            return Ok(());
        }
        total = total.checked_add(count).ok_or(SvnError::DiffTooLarge)?;
        if total > limit {
            return Err(SvnError::DiffTooLarge);
        }
        on_chunk(&buffer[..count])?;
    }
}

fn parse_xml<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T, SvnError> {
    let text = std::str::from_utf8(bytes).map_err(|_| SvnError::InvalidUtf8)?;
    from_str(text).map_err(|_| SvnError::InvalidXml)
}

#[derive(Debug, Deserialize)]
pub struct InfoDocument {
    #[serde(rename = "entry", default)]
    pub entries: Vec<InfoEntry>,
}

#[derive(Debug, Deserialize)]
pub struct InfoEntry {
    #[serde(rename = "@revision")]
    pub revision: Option<u64>,
    #[serde(rename = "relative-url")]
    pub relative_url: Option<String>,
    #[serde(rename = "repository")]
    pub repository: Option<InfoRepository>,
    #[serde(rename = "wc-info")]
    pub wc_info: Option<InfoWorkingCopy>,
}

#[derive(Debug, Deserialize)]
pub struct InfoRepository {
    pub root: String,
}

#[derive(Debug, Deserialize)]
pub struct InfoWorkingCopy {
    #[serde(rename = "wcroot-abspath")]
    pub wcroot_abspath: Option<String>,
    pub depth: Option<String>,
}

#[derive(Debug, Deserialize)]
struct StatusDocument {
    #[serde(rename = "target", default)]
    targets: Vec<StatusTarget>,
}

#[derive(Debug, Deserialize)]
struct StatusTarget {
    #[serde(rename = "entry", default)]
    entries: Vec<StatusEntry>,
}

#[derive(Debug, Deserialize)]
struct StatusEntry {
    #[serde(rename = "@path")]
    path: String,
    #[serde(rename = "wc-status")]
    wc_status: Option<WorkingCopyStatus>,
}

#[derive(Debug, Default, Deserialize)]
struct WorkingCopyStatus {
    #[serde(rename = "@item")]
    item: Option<String>,
    #[serde(rename = "@props")]
    props: Option<String>,
    #[serde(rename = "@revision")]
    revision: Option<String>,
    #[serde(rename = "@tree-conflicted", default)]
    tree_conflicted: bool,
}

#[derive(Debug, Deserialize)]
struct LogDocument {
    #[serde(rename = "logentry", default)]
    entries: Vec<LogEntry>,
}

#[derive(Debug, Deserialize)]
struct LogEntry {
    #[serde(rename = "@revision")]
    revision: u64,
    author: Option<String>,
    date: Option<String>,
    msg: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DiffDocument {
    paths: Option<DiffPaths>,
}

#[derive(Debug, Deserialize)]
struct DiffPaths {
    #[serde(rename = "path", default)]
    items: Vec<DiffPath>,
}

#[derive(Debug, Deserialize)]
struct DiffPath {
    #[serde(rename = "@kind")]
    kind: Option<String>,
    #[serde(rename = "@item")]
    item: Option<String>,
    #[serde(rename = "@props")]
    props: Option<String>,
    #[serde(rename = "$text", default)]
    value: String,
}

#[cfg(test)]
mod tests {
    use super::{SvnError, read_limited, read_streamed, relative_target, relative_target_at};
    use tokio::io::AsyncReadExt;

    #[test]
    fn escapes_relative_svn_targets_and_rejects_traversal() {
        assert_eq!(
            relative_target("folder/file@name.txt").unwrap(),
            "folder/file@name.txt@"
        );
        let platform_path = relative_target("folder\\file.txt").unwrap();
        assert_eq!(
            platform_path,
            if cfg!(windows) {
                "folder/file.txt@"
            } else {
                "folder\\file.txt@"
            }
        );
        assert!(relative_target("../outside.txt").is_err());
        assert!(relative_target("/outside.txt").is_err());
        assert_eq!(
            relative_target_at("folder/file@name.txt", Some(42)).unwrap(),
            "folder/file@name.txt@42"
        );
    }

    #[tokio::test]
    async fn stops_reading_at_the_command_specific_limit() {
        assert_eq!(read_limited(&b"1234"[..], 4).await.unwrap(), b"1234");
        assert!(matches!(
            read_limited(&b"1234"[..], 3).await,
            Err(SvnError::OutputTooLarge)
        ));
    }

    #[tokio::test]
    async fn streams_without_retaining_the_complete_command_output() {
        let mut streamed = Vec::new();
        read_streamed(&b"1234"[..], 4, |chunk| {
            streamed.extend_from_slice(chunk);
            Ok(())
        })
        .await
        .unwrap();
        assert_eq!(streamed, b"1234");
        assert!(matches!(
            read_streamed(&b"1234"[..], 3, |_| Ok(())).await,
            Err(SvnError::DiffTooLarge)
        ));
    }

    #[tokio::test]
    async fn accepts_streams_larger_than_the_previous_sixteen_mib_limit() {
        let expected = 17 * 1024 * 1024;
        let reader = tokio::io::repeat(b'x').take(expected as u64);
        let mut received = 0;

        read_streamed(reader, super::MAX_DIFF_STREAM_BYTES, |chunk| {
            received += chunk.len();
            Ok(())
        })
        .await
        .unwrap();

        assert_eq!(received, expected);
    }
}

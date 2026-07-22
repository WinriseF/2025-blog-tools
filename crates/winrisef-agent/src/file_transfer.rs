use std::{
    collections::HashMap,
    fs::{File, OpenOptions},
    io,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, UNIX_EPOCH},
};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::{
    auth::{constant_time_equal, now_ms},
    certificate::format_hex,
};

pub const NATIVE_FILE_VERSION: u16 = 1;
pub const FILE_IO_BLOCK_BYTES: usize = 4 * 1024 * 1024;
pub const FILE_HTTP_SEGMENT_BYTES: u64 = 30 * 1024 * 1024;
pub const FILE_HTTP_PARALLELISM: usize = 6;
pub const FILE_WEBTRANSPORT_CONNECTIONS: usize = 6;
pub const FILE_WEBTRANSPORT_LANES_PER_CONNECTION: usize = 4;
pub const FILE_WEBTRANSPORT_EXTENT_BYTES: u64 = 64 * 1024 * 1024;

const MAX_SELECTED_SOURCES: usize = 32;
const TRANSFER_HARD_TTL: Duration = Duration::from_secs(12 * 60 * 60);
const TRANSFER_IDLE_TTL: Duration = Duration::from_secs(120);
const PROGRESS_INTERVAL_BYTES: u64 = 32 * 1024 * 1024;
const PROGRESS_INTERVAL_MS: u64 = 250;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum NativeDataPlane {
    #[serde(rename = "native-lna-http")]
    LnaHttp,
    #[serde(rename = "native-webtransport")]
    WebTransport,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum NativeFileDirection {
    AgentToBrowser,
    BrowserToAgent,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SelectedFileMetadata {
    pub source_id: String,
    pub name: String,
    pub mime: String,
    pub size: u64,
    pub last_modified: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum TransferAuthorization {
    LnaHttp { token: String },
    WebTransport { tokens: Vec<String> },
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeTransferGrant {
    pub transfer_id: String,
    pub attachment_id: String,
    pub owner_device_id: String,
    pub authorization: TransferAuthorization,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type")]
pub enum NativeTransferEvent {
    #[serde(rename = "transfer-progress")]
    Progress {
        #[serde(rename = "transferId")]
        transfer_id: String,
        #[serde(rename = "attachmentId")]
        attachment_id: String,
        bytes: u64,
    },
    #[serde(rename = "transfer-confirming")]
    Confirming {
        #[serde(rename = "transferId")]
        transfer_id: String,
        #[serde(rename = "attachmentId")]
        attachment_id: String,
        bytes: u64,
    },
    #[serde(rename = "transfer-complete")]
    Complete {
        #[serde(rename = "transferId")]
        transfer_id: String,
        #[serde(rename = "attachmentId")]
        attachment_id: String,
    },
    #[serde(rename = "transfer-failed")]
    Failed {
        #[serde(rename = "transferId")]
        transfer_id: String,
        #[serde(rename = "attachmentId")]
        attachment_id: String,
        error: String,
    },
    #[serde(rename = "transfer-cancelled")]
    Cancelled {
        #[serde(rename = "transferId")]
        transfer_id: String,
        #[serde(rename = "attachmentId")]
        attachment_id: String,
    },
}

#[derive(Clone)]
pub struct FileTransferManager {
    inner: Arc<ManagerInner>,
}

struct ManagerInner {
    selected: Mutex<HashMap<String, SelectedSource>>,
    active: Mutex<Option<Arc<ActiveTransfer>>>,
    events: broadcast::Sender<NativeTransferEvent>,
}

#[derive(Clone)]
struct SelectedSource {
    metadata: SelectedFileMetadata,
    file: Arc<File>,
}

#[derive(Clone)]
enum TransferResource {
    Send {
        source_id: String,
        file: Arc<File>,
    },
    Receive {
        file: Arc<File>,
        part_path: PathBuf,
        final_path: PathBuf,
    },
}

struct TransferSpec<'a> {
    attachment_id: &'a str,
    owner_device_id: &'a str,
    peer_device_id: &'a str,
    direction: NativeFileDirection,
    data_plane: NativeDataPlane,
    total_bytes: u64,
    resource: TransferResource,
}

struct ActiveTransfer {
    transfer_id: String,
    attachment_id: String,
    peer_device_id: String,
    direction: NativeFileDirection,
    data_plane: NativeDataPlane,
    total_bytes: u64,
    hard_expires_at: u64,
    lna_token: Option<[u8; 32]>,
    webtransport_tokens: Vec<[u8; 16]>,
    webtransport_consumed: Mutex<Vec<bool>>,
    segments: SegmentTracker,
    resource: TransferResource,
    last_activity_ms: AtomicU64,
    last_progress_ms: AtomicU64,
    last_progress_bytes: AtomicU64,
    cancelled: AtomicBool,
}

struct SegmentTracker {
    total_bytes: u64,
    segment_bytes: u64,
    states: Mutex<Vec<SegmentState>>,
    completed_bytes: AtomicU64,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum SegmentState {
    Pending,
    Active,
    Complete,
}

pub struct SegmentLease {
    transfer: Arc<ActiveTransfer>,
    index: usize,
    offset: u64,
    len: u64,
    committed: bool,
}

#[derive(Clone)]
pub struct WebTransportConnectionLease {
    transfer: Arc<ActiveTransfer>,
    connection_index: usize,
}

impl FileTransferManager {
    pub fn new() -> Self {
        let (events, _) = broadcast::channel(128);
        Self {
            inner: Arc::new(ManagerInner {
                selected: Mutex::new(HashMap::new()),
                active: Mutex::new(None),
                events,
            }),
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<NativeTransferEvent> {
        self.inner.events.subscribe()
    }

    pub async fn select_files(&self) -> anyhow::Result<Vec<SelectedFileMetadata>> {
        let selected = tokio::task::spawn_blocking(|| {
            let paths = rfd::FileDialog::new().pick_files().unwrap_or_default();
            paths
                .into_iter()
                .map(open_selected_source)
                .collect::<anyhow::Result<Vec<_>>>()
        })
        .await
        .context("file selection task panicked")??;
        let mut sources = self
            .inner
            .selected
            .lock()
            .map_err(|_| anyhow::anyhow!("selected file registry is unavailable"))?;
        anyhow::ensure!(
            sources.len().saturating_add(selected.len()) <= MAX_SELECTED_SOURCES,
            "too many files are waiting for transfer"
        );
        let mut metadata = Vec::with_capacity(selected.len());
        for source in selected {
            metadata.push(source.metadata.clone());
            sources.insert(source.metadata.source_id.clone(), source);
        }
        Ok(metadata)
    }

    pub fn create_send_transfer(
        &self,
        source_id: &str,
        attachment_id: &str,
        owner_device_id: &str,
        peer_device_id: &str,
        data_plane: NativeDataPlane,
    ) -> anyhow::Result<NativeTransferGrant> {
        self.ensure_idle()?;
        let source = self
            .inner
            .selected
            .lock()
            .map_err(|_| anyhow::anyhow!("selected file registry is unavailable"))?
            .get(source_id)
            .cloned()
            .context("the selected file is no longer available")?;
        self.install_transfer(TransferSpec {
            attachment_id,
            owner_device_id,
            peer_device_id,
            direction: NativeFileDirection::AgentToBrowser,
            data_plane,
            total_bytes: source.metadata.size,
            resource: TransferResource::Send {
                source_id: source_id.to_owned(),
                file: source.file,
            },
        })
    }

    pub async fn prepare_receive_transfer(
        &self,
        attachment_id: &str,
        owner_device_id: &str,
        peer_device_id: &str,
        name: &str,
        total_bytes: u64,
        data_plane: NativeDataPlane,
    ) -> anyhow::Result<Option<NativeTransferGrant>> {
        self.ensure_idle()?;
        anyhow::ensure!(total_bytes > 0, "file size must be positive");
        let suggested_name = name.to_owned();
        let destination = tokio::task::spawn_blocking(move || {
            rfd::FileDialog::new().set_file_name(&suggested_name).save_file()
        })
        .await
        .context("save file dialog task panicked")?;
        let Some(final_path) = destination else {
            return Ok(None);
        };
        let transfer_id = random_hex::<16>()?;
        let part_path = temporary_path(&final_path, &transfer_id)?;
        let file = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&part_path)
            .context("failed to create the temporary destination file")?;
        if let Err(error) = file.set_len(total_bytes) {
            let _ = std::fs::remove_file(&part_path);
            return Err(error).context("failed to reserve the destination file length");
        }
        self.install_transfer_with_id(
            transfer_id,
            TransferSpec {
                attachment_id,
                owner_device_id,
                peer_device_id,
                direction: NativeFileDirection::BrowserToAgent,
                data_plane,
                total_bytes,
                resource: TransferResource::Receive {
                    file: Arc::new(file),
                    part_path,
                    final_path,
                },
            },
        )
        .map(Some)
    }

    pub fn begin_lna_segment(
        &self,
        transfer_id: &str,
        token: &[u8; 32],
        direction: NativeFileDirection,
        offset: u64,
        len: u64,
    ) -> anyhow::Result<SegmentLease> {
        let transfer = self.active_transfer(transfer_id)?;
        transfer.validate_lna(token, direction)?;
        let index = transfer.segments.reserve(offset, len)?;
        transfer.touch()?;
        Ok(SegmentLease {
            transfer,
            index,
            offset,
            len,
            committed: false,
        })
    }

    pub fn begin_webtransport_connection(
        &self,
        transfer_id: &str,
        token: &[u8; 16],
        connection_index: usize,
        direction: NativeFileDirection,
        peer_device_id: &str,
    ) -> anyhow::Result<WebTransportConnectionLease> {
        let transfer = self.active_transfer(transfer_id)?;
        transfer.consume_webtransport_token(token, connection_index, direction, peer_device_id)?;
        transfer.touch()?;
        Ok(WebTransportConnectionLease {
            transfer,
            connection_index,
        })
    }

    pub fn begin_webtransport_extent(
        &self,
        connection: &WebTransportConnectionLease,
        lane_index: usize,
        offset: u64,
        len: u64,
    ) -> anyhow::Result<SegmentLease> {
        anyhow::ensure!(
            lane_index < FILE_WEBTRANSPORT_LANES_PER_CONNECTION,
            "lane index is invalid"
        );
        let extent_index = offset / FILE_WEBTRANSPORT_EXTENT_BYTES;
        let expected_lane = connection.connection_index * FILE_WEBTRANSPORT_LANES_PER_CONNECTION + lane_index;
        anyhow::ensure!(
            extent_index % (FILE_WEBTRANSPORT_CONNECTIONS * FILE_WEBTRANSPORT_LANES_PER_CONNECTION) as u64
                == expected_lane as u64,
            "extent was assigned to the wrong WebTransport lane"
        );
        let transfer = Arc::clone(&connection.transfer);
        let index = transfer.segments.reserve(offset, len)?;
        transfer.touch()?;
        Ok(SegmentLease {
            transfer,
            index,
            offset,
            len,
            committed: false,
        })
    }

    pub fn commit_segment(&self, mut lease: SegmentLease) -> anyhow::Result<u64> {
        let completed = lease.transfer.segments.complete(lease.index, lease.len)?;
        lease.committed = true;
        let now = now_ms()?;
        let previous_bytes = lease.transfer.last_progress_bytes.load(Ordering::Relaxed);
        let previous_ms = lease.transfer.last_progress_ms.load(Ordering::Relaxed);
        if completed == lease.transfer.total_bytes
            || completed.saturating_sub(previous_bytes) >= PROGRESS_INTERVAL_BYTES
            || now.saturating_sub(previous_ms) >= PROGRESS_INTERVAL_MS
        {
            lease
                .transfer
                .last_progress_bytes
                .store(completed, Ordering::Relaxed);
            lease.transfer.last_progress_ms.store(now, Ordering::Relaxed);
            self.publish(NativeTransferEvent::Progress {
                transfer_id: lease.transfer.transfer_id.clone(),
                attachment_id: lease.transfer.attachment_id.clone(),
                bytes: completed,
            });
        }
        Ok(completed)
    }

    pub fn complete_receive(&self, transfer_id: &str, token: &[u8; 32]) -> anyhow::Result<()> {
        let transfer = self.active_transfer(transfer_id)?;
        transfer.validate_lna(token, NativeFileDirection::BrowserToAgent)?;
        self.complete_receive_transfer(transfer)
    }

    pub fn complete_webtransport_receive(
        &self,
        connection: &WebTransportConnectionLease,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            connection.connection_index == 0
                && connection.transfer.direction == NativeFileDirection::BrowserToAgent,
            "only connection zero may complete a browser upload"
        );
        connection.transfer.validate_time()?;
        self.complete_receive_transfer(Arc::clone(&connection.transfer))
    }

    fn complete_receive_transfer(&self, transfer: Arc<ActiveTransfer>) -> anyhow::Result<()> {
        anyhow::ensure!(transfer.segments.is_complete(), "file coverage is incomplete");
        let TransferResource::Receive {
            file,
            part_path,
            final_path,
        } = &transfer.resource
        else {
            anyhow::bail!("transfer does not have a writable destination");
        };
        self.publish(NativeTransferEvent::Confirming {
            transfer_id: transfer.transfer_id.clone(),
            attachment_id: transfer.attachment_id.clone(),
            bytes: transfer.total_bytes,
        });
        file.sync_all().context("failed to flush the received file")?;
        winrisef_platform::atomic_replace(part_path, final_path)
            .context("failed to atomically finish the received file")?;
        self.take_active(&transfer.transfer_id)?;
        self.publish(NativeTransferEvent::Complete {
            transfer_id: transfer.transfer_id.clone(),
            attachment_id: transfer.attachment_id.clone(),
        });
        Ok(())
    }

    pub fn finish_send_transfer(&self, transfer_id: &str) -> anyhow::Result<()> {
        let transfer = self.take_active(transfer_id)?;
        let TransferResource::Send { source_id, .. } = &transfer.resource else {
            anyhow::bail!("transfer is not an Agent source transfer");
        };
        self.release_source(source_id)?;
        Ok(())
    }

    pub fn cancel_transfer(&self, transfer_id: &str) -> anyhow::Result<()> {
        let transfer = self.take_active(transfer_id)?;
        transfer.cancelled.store(true, Ordering::Release);
        cleanup_resource(&transfer.resource);
        if let TransferResource::Send { source_id, .. } = &transfer.resource {
            self.release_source(source_id)?;
        }
        self.publish(NativeTransferEvent::Cancelled {
            transfer_id: transfer.transfer_id.clone(),
            attachment_id: transfer.attachment_id.clone(),
        });
        Ok(())
    }

    pub fn fail_transfer(&self, transfer_id: &str, error: impl Into<String>) {
        let Ok(transfer) = self.take_active(transfer_id) else {
            return;
        };
        transfer.cancelled.store(true, Ordering::Release);
        cleanup_resource(&transfer.resource);
        if let TransferResource::Send { source_id, .. } = &transfer.resource {
            let _ = self.release_source(source_id);
        }
        self.publish(NativeTransferEvent::Failed {
            transfer_id: transfer.transfer_id.clone(),
            attachment_id: transfer.attachment_id.clone(),
            error: error.into(),
        });
    }

    pub fn release_source(&self, source_id: &str) -> anyhow::Result<()> {
        self.inner
            .selected
            .lock()
            .map_err(|_| anyhow::anyhow!("selected file registry is unavailable"))?
            .remove(source_id);
        Ok(())
    }

    pub fn cancel_all(&self) {
        let transfer = self.inner.active.lock().ok().and_then(|mut active| active.take());
        if let Some(transfer) = transfer {
            transfer.cancelled.store(true, Ordering::Release);
            cleanup_resource(&transfer.resource);
        }
        if let Ok(mut selected) = self.inner.selected.lock() {
            selected.clear();
        }
    }

    fn install_transfer(&self, spec: TransferSpec<'_>) -> anyhow::Result<NativeTransferGrant> {
        self.install_transfer_with_id(random_hex::<16>()?, spec)
    }

    fn install_transfer_with_id(
        &self,
        transfer_id: String,
        spec: TransferSpec<'_>,
    ) -> anyhow::Result<NativeTransferGrant> {
        let TransferSpec {
            attachment_id,
            owner_device_id,
            peer_device_id,
            direction,
            data_plane,
            total_bytes,
            resource,
        } = spec;
        anyhow::ensure!(total_bytes > 0, "file size must be positive");
        let now = now_ms()?;
        let hard_expires_at = now.saturating_add(TRANSFER_HARD_TTL.as_millis() as u64);
        let (lna_token, webtransport_tokens, authorization) = match data_plane {
            NativeDataPlane::LnaHttp => {
                let token = random_bytes::<32>()?;
                (
                    Some(token),
                    Vec::new(),
                    TransferAuthorization::LnaHttp {
                        token: format_hex(&token),
                    },
                )
            }
            NativeDataPlane::WebTransport => {
                let mut tokens = Vec::with_capacity(FILE_WEBTRANSPORT_CONNECTIONS);
                for _ in 0..FILE_WEBTRANSPORT_CONNECTIONS {
                    tokens.push(random_bytes::<16>()?);
                }
                let encoded = tokens.iter().map(|token| format_hex(token)).collect();
                (
                    None,
                    tokens,
                    TransferAuthorization::WebTransport { tokens: encoded },
                )
            }
        };
        let transfer = Arc::new(ActiveTransfer {
            transfer_id: transfer_id.clone(),
            attachment_id: attachment_id.to_owned(),
            peer_device_id: peer_device_id.to_owned(),
            direction,
            data_plane,
            total_bytes,
            hard_expires_at,
            lna_token,
            webtransport_tokens,
            webtransport_consumed: Mutex::new(vec![false; FILE_WEBTRANSPORT_CONNECTIONS]),
            segments: SegmentTracker::new(
                total_bytes,
                match data_plane {
                    NativeDataPlane::LnaHttp => FILE_HTTP_SEGMENT_BYTES,
                    NativeDataPlane::WebTransport => FILE_WEBTRANSPORT_EXTENT_BYTES,
                },
            )?,
            resource,
            last_activity_ms: AtomicU64::new(now),
            last_progress_ms: AtomicU64::new(now),
            last_progress_bytes: AtomicU64::new(0),
            cancelled: AtomicBool::new(false),
        });
        let mut active = self
            .inner
            .active
            .lock()
            .map_err(|_| anyhow::anyhow!("active transfer registry is unavailable"))?;
        anyhow::ensure!(active.is_none(), "another native file transfer is active");
        *active = Some(transfer);
        Ok(NativeTransferGrant {
            transfer_id,
            attachment_id: attachment_id.to_owned(),
            owner_device_id: owner_device_id.to_owned(),
            authorization,
        })
    }

    fn ensure_idle(&self) -> anyhow::Result<()> {
        let active = self
            .inner
            .active
            .lock()
            .map_err(|_| anyhow::anyhow!("active transfer registry is unavailable"))?;
        anyhow::ensure!(active.is_none(), "another native file transfer is active");
        Ok(())
    }

    fn active_transfer(&self, transfer_id: &str) -> anyhow::Result<Arc<ActiveTransfer>> {
        let transfer = self
            .inner
            .active
            .lock()
            .map_err(|_| anyhow::anyhow!("active transfer registry is unavailable"))?
            .clone()
            .context("native file transfer is not active")?;
        anyhow::ensure!(
            transfer.transfer_id == transfer_id,
            "transfer identifier does not match"
        );
        Ok(transfer)
    }

    fn take_active(&self, transfer_id: &str) -> anyhow::Result<Arc<ActiveTransfer>> {
        let mut active = self
            .inner
            .active
            .lock()
            .map_err(|_| anyhow::anyhow!("active transfer registry is unavailable"))?;
        let transfer = active.as_ref().context("native file transfer is not active")?;
        anyhow::ensure!(
            transfer.transfer_id == transfer_id,
            "transfer identifier does not match"
        );
        active.take().context("native file transfer is not active")
    }

    fn publish(&self, event: NativeTransferEvent) {
        let _ = self.inner.events.send(event);
    }
}

impl ActiveTransfer {
    fn validate_lna(&self, token: &[u8; 32], direction: NativeFileDirection) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.data_plane == NativeDataPlane::LnaHttp,
            "transfer does not use LNA HTTP"
        );
        anyhow::ensure!(self.direction == direction, "transfer direction does not match");
        anyhow::ensure!(!self.cancelled.load(Ordering::Acquire), "transfer was cancelled");
        let expected = self.lna_token.context("LNA transfer token is unavailable")?;
        anyhow::ensure!(constant_time_equal(&expected, token), "transfer token is invalid");
        self.validate_time()
    }

    fn consume_webtransport_token(
        &self,
        token: &[u8; 16],
        connection_index: usize,
        direction: NativeFileDirection,
        peer_device_id: &str,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.data_plane == NativeDataPlane::WebTransport,
            "transfer does not use WebTransport"
        );
        anyhow::ensure!(self.direction == direction, "transfer direction does not match");
        anyhow::ensure!(
            self.peer_device_id == peer_device_id,
            "transfer peer does not match"
        );
        anyhow::ensure!(!self.cancelled.load(Ordering::Acquire), "transfer was cancelled");
        let expected = self
            .webtransport_tokens
            .get(connection_index)
            .context("WebTransport connection index is invalid")?;
        anyhow::ensure!(
            constant_time_equal(expected, token),
            "WebTransport connection token is invalid"
        );
        self.validate_time()?;
        let mut consumed = self
            .webtransport_consumed
            .lock()
            .map_err(|_| anyhow::anyhow!("WebTransport authorization state is unavailable"))?;
        let state = consumed
            .get_mut(connection_index)
            .context("WebTransport connection index is invalid")?;
        anyhow::ensure!(!*state, "WebTransport connection token was already consumed");
        *state = true;
        Ok(())
    }

    fn validate_time(&self) -> anyhow::Result<()> {
        let now = now_ms()?;
        anyhow::ensure!(now <= self.hard_expires_at, "transfer authorization expired");
        let idle = now.saturating_sub(self.last_activity_ms.load(Ordering::Acquire));
        anyhow::ensure!(
            idle <= TRANSFER_IDLE_TTL.as_millis() as u64,
            "transfer authorization became idle"
        );
        Ok(())
    }

    fn touch(&self) -> anyhow::Result<()> {
        self.last_activity_ms.store(now_ms()?, Ordering::Release);
        Ok(())
    }
}

impl SegmentTracker {
    fn new(total_bytes: u64, segment_bytes: u64) -> anyhow::Result<Self> {
        anyhow::ensure!(
            total_bytes > 0 && segment_bytes > 0,
            "invalid segment coverage shape"
        );
        let count = usize::try_from(total_bytes.div_ceil(segment_bytes))
            .context("file contains too many segments")?;
        Ok(Self {
            total_bytes,
            segment_bytes,
            states: Mutex::new(vec![SegmentState::Pending; count]),
            completed_bytes: AtomicU64::new(0),
        })
    }

    fn reserve(&self, offset: u64, len: u64) -> anyhow::Result<usize> {
        anyhow::ensure!(
            offset.is_multiple_of(self.segment_bytes),
            "segment offset is not aligned"
        );
        anyhow::ensure!(offset < self.total_bytes, "segment offset is outside the file");
        let expected = self.segment_bytes.min(self.total_bytes - offset);
        anyhow::ensure!(len == expected, "segment length is invalid");
        let index = usize::try_from(offset / self.segment_bytes).context("segment index is too large")?;
        let mut states = self
            .states
            .lock()
            .map_err(|_| anyhow::anyhow!("segment coverage is unavailable"))?;
        let state = states
            .get_mut(index)
            .context("segment index is outside the file")?;
        anyhow::ensure!(*state == SegmentState::Pending, "segment was already requested");
        *state = SegmentState::Active;
        Ok(index)
    }

    fn complete(&self, index: usize, len: u64) -> anyhow::Result<u64> {
        let mut states = self
            .states
            .lock()
            .map_err(|_| anyhow::anyhow!("segment coverage is unavailable"))?;
        let state = states
            .get_mut(index)
            .context("segment index is outside the file")?;
        anyhow::ensure!(*state == SegmentState::Active, "segment is not active");
        *state = SegmentState::Complete;
        Ok(self.completed_bytes.fetch_add(len, Ordering::AcqRel) + len)
    }

    fn release(&self, index: usize) {
        if let Ok(mut states) = self.states.lock()
            && let Some(state) = states.get_mut(index)
            && *state == SegmentState::Active
        {
            *state = SegmentState::Pending;
        }
    }

    fn is_complete(&self) -> bool {
        self.completed_bytes.load(Ordering::Acquire) == self.total_bytes
    }
}

impl SegmentLease {
    pub const fn offset(&self) -> u64 {
        self.offset
    }

    pub const fn len(&self) -> u64 {
        self.len
    }

    pub fn read_exact_at(&self, mut buffer: &mut [u8], mut offset: u64) -> anyhow::Result<()> {
        let TransferResource::Send { file, .. } = &self.transfer.resource else {
            anyhow::bail!("transfer source is not readable");
        };
        while !buffer.is_empty() {
            let read = positional_read(file, buffer, offset).context("failed to read the selected file")?;
            anyhow::ensure!(read > 0, "selected file ended during transfer");
            offset += read as u64;
            buffer = &mut buffer[read..];
        }
        Ok(())
    }

    pub fn write_all_at(&self, mut buffer: &[u8], mut offset: u64) -> anyhow::Result<()> {
        let TransferResource::Receive { file, .. } = &self.transfer.resource else {
            anyhow::bail!("transfer destination is not writable");
        };
        while !buffer.is_empty() {
            let written =
                positional_write(file, buffer, offset).context("failed to write the destination file")?;
            anyhow::ensure!(written > 0, "destination file accepted an empty write");
            offset += written as u64;
            buffer = &buffer[written..];
        }
        Ok(())
    }

    pub fn touch(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.transfer.cancelled.load(Ordering::Acquire),
            "transfer was cancelled"
        );
        self.transfer.touch()
    }
}

impl WebTransportConnectionLease {
    pub const fn connection_index(&self) -> usize {
        self.connection_index
    }

    pub fn total_bytes(&self) -> u64 {
        self.transfer.total_bytes
    }

    pub fn transfer_id(&self) -> &str {
        &self.transfer.transfer_id
    }
}

impl Drop for SegmentLease {
    fn drop(&mut self) {
        if !self.committed {
            self.transfer.segments.release(self.index);
        }
    }
}

fn open_selected_source(path: PathBuf) -> anyhow::Result<SelectedSource> {
    let file = File::open(&path).context("failed to open a selected file")?;
    let metadata = file.metadata().context("failed to read selected file metadata")?;
    anyhow::ensure!(
        metadata.is_file() && metadata.len() > 0,
        "selected item is not a non-empty file"
    );
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("selected-file")
        .to_owned();
    let last_modified = metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |duration| duration.as_millis().try_into().unwrap_or(u64::MAX));
    Ok(SelectedSource {
        metadata: SelectedFileMetadata {
            source_id: random_hex::<16>()?,
            mime: mime_guess::from_path(&path)
                .first_or_octet_stream()
                .essence_str()
                .to_owned(),
            name,
            size: metadata.len(),
            last_modified,
        },
        file: Arc::new(file),
    })
}

fn temporary_path(final_path: &Path, transfer_id: &str) -> anyhow::Result<PathBuf> {
    let parent = final_path
        .parent()
        .context("destination has no parent directory")?;
    let name = final_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("received-file");
    Ok(parent.join(format!(".{name}.winrisef-{transfer_id}.part")))
}

fn cleanup_resource(resource: &TransferResource) {
    if let TransferResource::Receive { part_path, .. } = resource {
        let _ = std::fs::remove_file(part_path);
    }
}

fn random_bytes<const N: usize>() -> anyhow::Result<[u8; N]> {
    let mut bytes = [0_u8; N];
    getrandom::fill(&mut bytes)
        .map_err(|error| anyhow::anyhow!("failed to create secure transfer credentials: {error}"))?;
    Ok(bytes)
}

fn random_hex<const N: usize>() -> anyhow::Result<String> {
    Ok(format_hex(&random_bytes::<N>()?))
}

#[cfg(windows)]
fn positional_read(file: &File, buffer: &mut [u8], offset: u64) -> io::Result<usize> {
    use std::os::windows::fs::FileExt;
    file.seek_read(buffer, offset)
}

#[cfg(windows)]
fn positional_write(file: &File, buffer: &[u8], offset: u64) -> io::Result<usize> {
    use std::os::windows::fs::FileExt;
    file.seek_write(buffer, offset)
}

#[cfg(unix)]
fn positional_read(file: &File, buffer: &mut [u8], offset: u64) -> io::Result<usize> {
    use std::os::unix::fs::FileExt;
    file.read_at(buffer, offset)
}

#[cfg(unix)]
fn positional_write(file: &File, buffer: &[u8], offset: u64) -> io::Result<usize> {
    use std::os::unix::fs::FileExt;
    file.write_at(buffer, offset)
}

#[cfg(test)]
mod tests {
    use super::SegmentTracker;

    #[test]
    fn segment_tracker_requires_exact_complete_coverage() {
        let tracker = SegmentTracker::new(70, 30).unwrap();
        let first = tracker.reserve(0, 30).unwrap();
        tracker.complete(first, 30).unwrap();
        let second = tracker.reserve(30, 30).unwrap();
        tracker.complete(second, 30).unwrap();
        assert!(!tracker.is_complete());
        let third = tracker.reserve(60, 10).unwrap();
        tracker.complete(third, 10).unwrap();
        assert!(tracker.is_complete());
    }

    #[test]
    fn native_file_fixture_matches_rust_constants() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../../protocol-fixtures/native-file-v1.json")).unwrap();
        assert_eq!(fixture["lanSessionVersion"], 12);
        assert_eq!(fixture["bridgeVersion"], 3);
        assert_eq!(fixture["fileVersion"], super::NATIVE_FILE_VERSION);
        assert_eq!(fixture["lnaHttp"]["segmentBytes"], super::FILE_HTTP_SEGMENT_BYTES);
        assert_eq!(fixture["lnaHttp"]["parallelism"], super::FILE_HTTP_PARALLELISM);
        assert_eq!(fixture["lnaHttp"]["ioBlockBytes"], super::FILE_IO_BLOCK_BYTES);
        assert_eq!(
            fixture["webTransport"]["connections"],
            super::FILE_WEBTRANSPORT_CONNECTIONS
        );
        assert_eq!(
            fixture["webTransport"]["lanesPerConnection"],
            super::FILE_WEBTRANSPORT_LANES_PER_CONNECTION
        );
        assert_eq!(
            fixture["webTransport"]["extentBytes"],
            super::FILE_WEBTRANSPORT_EXTENT_BYTES
        );
    }
}

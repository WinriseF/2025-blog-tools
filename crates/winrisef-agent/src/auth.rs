use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::Context;

const TICKET_TTL: Duration = Duration::from_secs(120);
const MAX_PENDING_TICKETS: usize = 64;

#[derive(Clone)]
pub struct TicketAuthority {
    inner: Arc<AuthorityInner>,
}

struct AuthorityInner {
    launch_token: [u8; 16],
    launch_expires_at_ms: u64,
    launch_used: AtomicBool,
    tickets: Mutex<Vec<TicketRecord>>,
}

struct TicketRecord {
    token: [u8; 16],
    expires_at_ms: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct PeerTicket {
    pub token: [u8; 16],
    pub expires_at_ms: u64,
}

impl TicketAuthority {
    pub fn new(launch_token: [u8; 16], launch_expires_at_ms: u64) -> Self {
        tracing::debug!(launch_expires_at_ms, "initialized ticket authority");
        Self {
            inner: Arc::new(AuthorityInner {
                launch_token,
                launch_expires_at_ms,
                launch_used: AtomicBool::new(false),
                tickets: Mutex::new(Vec::new()),
            }),
        }
    }

    pub fn consume_launch_token(&self, token: &[u8; 16]) -> anyhow::Result<bool> {
        let now = now_ms()?;
        if now > self.inner.launch_expires_at_ms {
            tracing::warn!(
                now_ms = now,
                expires_at_ms = self.inner.launch_expires_at_ms,
                "launch token expired"
            );
            return Ok(false);
        }
        if !constant_time_equal(token, &self.inner.launch_token) {
            tracing::warn!("launch token did not match the active authority");
            return Ok(false);
        }
        let consumed = self
            .inner
            .launch_used
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok();
        tracing::info!(consumed, "processed one-time launch token");
        Ok(consumed)
    }

    pub fn launch_is_consumed(&self) -> bool {
        self.inner.launch_used.load(Ordering::Acquire)
    }

    pub fn issue_ticket(&self) -> anyhow::Result<PeerTicket> {
        let now = now_ms()?;
        let expires_at_ms = now.saturating_add(TICKET_TTL.as_millis() as u64);
        let mut token = [0_u8; 16];
        getrandom::fill(&mut token)
            .map_err(|error| anyhow::anyhow!("failed to create a peer ticket: {error}"))?;
        let mut tickets = self
            .inner
            .tickets
            .lock()
            .map_err(|_| anyhow::anyhow!("peer ticket store is unavailable"))?;
        tickets.retain(|ticket| ticket.expires_at_ms >= now);
        let pending_before_issue = tickets.len();
        anyhow::ensure!(
            tickets.len() < MAX_PENDING_TICKETS,
            "too many pending peer tickets"
        );
        tickets.push(TicketRecord { token, expires_at_ms });
        tracing::info!(
            expires_at_ms,
            pending_before_issue,
            pending_after_issue = tickets.len(),
            "issued one-time peer ticket"
        );
        Ok(PeerTicket { token, expires_at_ms })
    }

    pub fn consume_ticket(&self, token: &[u8; 16]) -> anyhow::Result<bool> {
        let now = now_ms()?;
        let mut tickets = self
            .inner
            .tickets
            .lock()
            .map_err(|_| anyhow::anyhow!("peer ticket store is unavailable"))?;
        let mut matched = None;
        for (index, ticket) in tickets.iter().enumerate() {
            if constant_time_equal(token, &ticket.token) {
                matched = Some(index);
            }
        }
        let Some(index) = matched else {
            tickets.retain(|ticket| ticket.expires_at_ms >= now);
            tracing::warn!(pending_after_cleanup = tickets.len(), "peer ticket was not found");
            return Ok(false);
        };
        let ticket = tickets.remove(index);
        tickets.retain(|ticket| ticket.expires_at_ms >= now);
        let valid = ticket.expires_at_ms >= now;
        tracing::info!(
            valid,
            pending_after_consume = tickets.len(),
            "consumed one-time peer ticket"
        );
        Ok(valid)
    }
}

pub fn random_token() -> anyhow::Result<[u8; 16]> {
    let mut token = [0_u8; 16];
    getrandom::fill(&mut token)
        .map_err(|error| anyhow::anyhow!("failed to create a launch token: {error}"))?;
    tracing::trace!(
        token_bytes = token.len(),
        "generated cryptographically random token"
    );
    Ok(token)
}

pub fn now_ms() -> anyhow::Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX))
}

pub fn parse_hex<const N: usize>(value: &str, label: &str) -> anyhow::Result<[u8; N]> {
    anyhow::ensure!(
        value.len() == N * 2,
        "{label} must contain {} hexadecimal characters",
        N * 2
    );
    let mut bytes = [0_u8; N];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)
            .with_context(|| format!("{label} is not hexadecimal"))?;
    }
    Ok(bytes)
}

pub fn constant_time_equal<const N: usize>(left: &[u8; N], right: &[u8; N]) -> bool {
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (left, right)| difference | (left ^ right))
        == 0
}

use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
};

use serde::Serialize;
use tokio::sync::{mpsc, watch};

use crate::{
    file_http::FILE_HTTP_BASE_PATH, file_webtransport::FILE_WEBTRANSPORT_PATH,
    lna_http::LNA_HTTP_BASE_PATH, server::BENCHMARK_PATH,
};

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct NetworkEndpoints {
    pub http_ips: Vec<IpAddr>,
    pub webtransport_ips: Vec<IpAddr>,
    pub network_epoch: u64,
}

impl NetworkEndpoints {
    pub fn has_public_ipv6(&self) -> bool {
        self.webtransport_ips.iter().any(is_public_ipv6)
    }

    pub fn published(
        &self,
        port: u16,
        public_ipv6_state: PublicIpv6State,
    ) -> PublishedNetworkEndpoints {
        let allow_public_ipv6 = public_ipv6_state == PublicIpv6State::Available;
        let webtransport_ips = self
            .webtransport_ips
            .iter()
            .copied()
            .filter(|ip| allow_public_ipv6 || !is_public_ipv6(ip))
            .collect::<Vec<_>>();
        let mut hasher = DefaultHasher::new();
        self.http_ips.hash(&mut hasher);
        webtransport_ips.hash(&mut hasher);
        let network_epoch = hasher.finish();
        PublishedNetworkEndpoints {
            network_epoch: format!("{network_epoch:016x}"),
            benchmark_endpoints: webtransport_ips
                .iter()
                .map(|ip| https_endpoint(*ip, port, BENCHMARK_PATH))
                .collect(),
            lna_http_endpoints: self
                .http_ips
                .iter()
                .map(|ip| http_endpoint(*ip, port, LNA_HTTP_BASE_PATH))
                .collect(),
            file_http_endpoints: self
                .http_ips
                .iter()
                .map(|ip| http_endpoint(*ip, port, FILE_HTTP_BASE_PATH))
                .collect(),
            file_web_transport_endpoints: webtransport_ips
                .iter()
                .map(|ip| https_endpoint(*ip, port, FILE_WEBTRANSPORT_PATH))
                .collect(),
            public_ipv6_state,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PublicIpv6State {
    #[default]
    NotPresent,
    Authorizing,
    Available,
    Unavailable,
}

impl PublicIpv6State {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotPresent => "not-present",
            Self::Authorizing => "authorizing",
            Self::Available => "available",
            Self::Unavailable => "unavailable",
        }
    }
}

#[derive(Clone)]
pub struct EndpointPolicy {
    public_ipv6_state: watch::Sender<PublicIpv6State>,
}

impl EndpointPolicy {
    pub fn new(public_ipv6_state: PublicIpv6State) -> Self {
        let (public_ipv6_state, _) = watch::channel(public_ipv6_state);
        Self { public_ipv6_state }
    }

    pub fn public_ipv6_state(&self) -> PublicIpv6State {
        *self.public_ipv6_state.borrow()
    }

    pub fn set_public_ipv6_state(&self, state: PublicIpv6State) {
        if self.public_ipv6_state() == state {
            return;
        }
        self.public_ipv6_state.send_replace(state);
        tracing::info!(?state, "public IPv6 endpoint availability changed");
    }

    pub fn subscribe(&self) -> watch::Receiver<PublicIpv6State> {
        self.public_ipv6_state.subscribe()
    }

    pub fn published(&self, port: u16) -> PublishedNetworkEndpoints {
        discover().published(port, self.public_ipv6_state())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PublishedNetworkEndpoints {
    pub network_epoch: String,
    pub benchmark_endpoints: Vec<String>,
    pub lna_http_endpoints: Vec<String>,
    pub file_http_endpoints: Vec<String>,
    pub file_web_transport_endpoints: Vec<String>,
    pub public_ipv6_state: PublicIpv6State,
}

impl NetworkEndpoints {
    fn from_ips(mut ips: Vec<IpAddr>) -> Self {
        ips.sort_unstable();
        ips.dedup();
        let http_ips = ips
            .iter()
            .copied()
            .filter(is_private_http_address)
            .collect();
        let webtransport_ips = ips
            .into_iter()
            .filter(is_webtransport_address)
            .collect::<Vec<_>>();
        let mut hasher = DefaultHasher::new();
        webtransport_ips.hash(&mut hasher);
        let network_epoch = hasher.finish();
        Self {
            http_ips,
            webtransport_ips,
            network_epoch,
        }
    }
}

pub fn discover() -> NetworkEndpoints {
    let ips = platform_addresses().unwrap_or_else(|error| {
        tracing::warn!(error = ?error, "could not enumerate preferred unicast addresses");
        Vec::new()
    });
    let endpoints = NetworkEndpoints::from_ips(ips);
    tracing::debug!(
        private_http_count = endpoints.http_ips.len(),
        webtransport_count = endpoints.webtransport_ips.len(),
        has_public_ipv6 = endpoints.webtransport_ips.iter().any(is_public_ipv6),
        network_epoch = endpoints.network_epoch,
        "discovered publishable Agent network endpoints"
    );
    endpoints
}

pub struct NetworkChangeWatcher {
    receiver: mpsc::UnboundedReceiver<()>,
    #[cfg(target_os = "windows")]
    registration: Option<WindowsAddressChangeRegistration>,
    fallback_poll: tokio::time::Interval,
}

impl NetworkChangeWatcher {
    pub async fn changed(&mut self) {
        #[cfg(target_os = "windows")]
        if self.registration.is_some() {
            let _ = self.receiver.recv().await;
            return;
        }
        self.fallback_poll.tick().await;
    }
}

pub fn watch_changes() -> NetworkChangeWatcher {
    let (sender, receiver) = mpsc::unbounded_channel();
    #[cfg(target_os = "windows")]
    let registration = WindowsAddressChangeRegistration::register(sender).map_err(|error| {
        tracing::warn!(error = ?error, "NotifyUnicastIpAddressChange unavailable; polling endpoints instead");
        error
    }).ok();
    #[cfg(not(target_os = "windows"))]
    drop(sender);
    let mut fallback_poll = tokio::time::interval(std::time::Duration::from_secs(2));
    fallback_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    NetworkChangeWatcher {
        receiver,
        #[cfg(target_os = "windows")]
        registration,
        fallback_poll,
    }
}

#[cfg(target_os = "windows")]
struct WindowsAddressChangeRegistration {
    handle: windows_sys::Win32::Foundation::HANDLE,
    context: *mut mpsc::UnboundedSender<()>,
}

#[cfg(target_os = "windows")]
unsafe impl Send for WindowsAddressChangeRegistration {}

#[cfg(target_os = "windows")]
impl WindowsAddressChangeRegistration {
    fn register(sender: mpsc::UnboundedSender<()>) -> anyhow::Result<Self> {
        use std::ptr;
        use windows_sys::Win32::{
            Foundation::{ERROR_SUCCESS, HANDLE},
            NetworkManagement::IpHelper::NotifyUnicastIpAddressChange,
            Networking::WinSock::AF_UNSPEC,
        };

        let context = Box::into_raw(Box::new(sender));
        let mut handle: HANDLE = ptr::null_mut();
        let result = unsafe {
            NotifyUnicastIpAddressChange(
                AF_UNSPEC,
                Some(address_change_callback),
                context.cast(),
                false,
                &mut handle,
            )
        };
        if result != ERROR_SUCCESS {
            unsafe { drop(Box::from_raw(context)) };
            anyhow::bail!("NotifyUnicastIpAddressChange failed with Windows error {result}");
        }
        Ok(Self { handle, context })
    }
}

#[cfg(target_os = "windows")]
impl Drop for WindowsAddressChangeRegistration {
    fn drop(&mut self) {
        use windows_sys::Win32::{
            Foundation::ERROR_SUCCESS, NetworkManagement::IpHelper::CancelMibChangeNotify2,
        };

        let result = unsafe { CancelMibChangeNotify2(self.handle) };
        if result == ERROR_SUCCESS {
            unsafe { drop(Box::from_raw(self.context)) };
        } else {
            tracing::warn!(
                windows_error = result,
                "could not cancel address-change notification; retaining callback context"
            );
        }
    }
}

#[cfg(target_os = "windows")]
unsafe extern "system" fn address_change_callback(
    context: *const core::ffi::c_void,
    _row: *const windows_sys::Win32::NetworkManagement::IpHelper::MIB_UNICASTIPADDRESS_ROW,
    _notification: windows_sys::Win32::NetworkManagement::IpHelper::MIB_NOTIFICATION_TYPE,
) {
    let sender = unsafe { &*context.cast::<mpsc::UnboundedSender<()>>() };
    let _ = sender.send(());
}

fn http_endpoint(ip: IpAddr, port: u16, path: &str) -> String {
    match ip {
        IpAddr::V4(ip) => format!("http://{ip}:{port}{path}"),
        IpAddr::V6(ip) => format!("http://[{ip}]:{port}{path}"),
    }
}

fn https_endpoint(ip: IpAddr, port: u16, path: &str) -> String {
    match ip {
        IpAddr::V4(ip) => format!("https://{ip}:{port}{path}"),
        IpAddr::V6(ip) => format!("https://[{ip}]:{port}{path}"),
    }
}

pub fn is_private_http_address(address: &IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => address.is_private() || is_cgnat(*address),
        IpAddr::V6(address) => is_ula(*address),
    }
}

pub fn is_webtransport_address(address: &IpAddr) -> bool {
    is_private_http_address(address) || matches!(address, IpAddr::V6(address) if is_gua(*address))
}

fn is_public_ipv6(address: &IpAddr) -> bool {
    matches!(address, IpAddr::V6(address) if is_gua(*address))
}

fn is_cgnat(address: Ipv4Addr) -> bool {
    let octets = address.octets();
    octets[0] == 100 && (64..=127).contains(&octets[1])
}

fn is_ula(address: Ipv6Addr) -> bool {
    address.segments()[0] & 0xfe00 == 0xfc00
}

fn is_gua(address: Ipv6Addr) -> bool {
    let segments = address.segments();
    segments[0] & 0xe000 == 0x2000
        && !(segments[0] == 0x2001 && segments[1] == 0x0db8)
        && address.to_ipv4_mapped().is_none()
}

#[cfg(target_os = "windows")]
fn platform_addresses() -> anyhow::Result<Vec<IpAddr>> {
    use std::ptr;

    use windows_sys::Win32::{
        Foundation::{ERROR_BUFFER_OVERFLOW, ERROR_SUCCESS},
        NetworkManagement::{
            IpHelper::{
                GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_MULTICAST,
                GetAdaptersAddresses, IP_ADAPTER_ADDRESSES_LH,
            },
            Ndis::IfOperStatusUp,
        },
        Networking::WinSock::{AF_UNSPEC, IpDadStatePreferred},
    };

    const RECOMMENDED_BUFFER_BYTES: u32 = 15 * 1024;
    let flags = GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER;
    let mut size = RECOMMENDED_BUFFER_BYTES;
    let mut buffer = vec![0_u64; (size as usize).div_ceil(8)];
    let mut result = unsafe {
        GetAdaptersAddresses(
            AF_UNSPEC as u32,
            flags,
            ptr::null(),
            buffer.as_mut_ptr().cast(),
            &mut size,
        )
    };
    if result == ERROR_BUFFER_OVERFLOW {
        buffer.resize((size as usize).div_ceil(8), 0);
        result = unsafe {
            GetAdaptersAddresses(
                AF_UNSPEC as u32,
                flags,
                ptr::null(),
                buffer.as_mut_ptr().cast(),
                &mut size,
            )
        };
    }
    anyhow::ensure!(
        result == ERROR_SUCCESS,
        "GetAdaptersAddresses failed with Windows error {result}"
    );

    let mut ips = Vec::new();
    let mut adapter = buffer.as_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
    while !adapter.is_null() {
        let current = unsafe { &*adapter };
        if current.OperStatus == IfOperStatusUp {
            let mut unicast = current.FirstUnicastAddress;
            while !unicast.is_null() {
                let address = unsafe { &*unicast };
                if address.DadState == IpDadStatePreferred
                    && let Some(ip) = unsafe { socket_address_to_ip(address.Address.lpSockaddr) }
                    && is_webtransport_address(&ip)
                {
                    ips.push(ip);
                }
                unicast = address.Next;
            }
        }
        adapter = current.Next;
    }
    anyhow::ensure!(!buffer.is_empty(), "adapter address buffer was empty");
    Ok(ips)
}

#[cfg(target_os = "windows")]
unsafe fn socket_address_to_ip(
    address: *mut windows_sys::Win32::Networking::WinSock::SOCKADDR,
) -> Option<IpAddr> {
    use windows_sys::Win32::Networking::WinSock::{AF_INET, AF_INET6, SOCKADDR_IN, SOCKADDR_IN6};

    let address = unsafe { address.as_ref()? };
    match address.sa_family {
        AF_INET => {
            let address = unsafe { &*std::ptr::from_ref(address).cast::<SOCKADDR_IN>() };
            let octets = unsafe { address.sin_addr.S_un.S_addr.to_ne_bytes() };
            Some(IpAddr::V4(Ipv4Addr::from(octets)))
        }
        AF_INET6 => {
            let address = unsafe { &*std::ptr::from_ref(address).cast::<SOCKADDR_IN6>() };
            let octets = unsafe { address.sin6_addr.u.Byte };
            Some(IpAddr::V6(Ipv6Addr::from(octets)))
        }
        _ => None,
    }
}

#[cfg(not(target_os = "windows"))]
fn platform_addresses() -> anyhow::Result<Vec<IpAddr>> {
    anyhow::bail!("native adapter enumeration is currently supported only on Windows")
}

#[cfg(test)]
mod tests {
    use super::{
        NetworkEndpoints, PublicIpv6State, is_private_http_address, is_webtransport_address,
    };
    use std::net::IpAddr;

    fn ip(value: &str) -> IpAddr {
        value.parse().unwrap()
    }

    #[test]
    fn separates_http_and_webtransport_address_spaces() {
        let endpoints = NetworkEndpoints::from_ips(vec![
            ip("192.168.1.4"),
            ip("100.64.2.3"),
            ip("fd00::4"),
            ip("2408:8207:1234::8"),
            ip("fe80::1"),
            ip("2001:db8::1"),
            ip("::ffff:192.168.1.4"),
        ]);
        assert_eq!(
            endpoints.http_ips,
            vec![ip("100.64.2.3"), ip("192.168.1.4"), ip("fd00::4")]
        );
        assert_eq!(
            endpoints.webtransport_ips,
            vec![
                ip("100.64.2.3"),
                ip("192.168.1.4"),
                ip("2408:8207:1234::8"),
                ip("fd00::4")
            ]
        );
    }

    #[test]
    fn rejects_non_routable_or_unsafe_addresses() {
        for value in [
            "0.0.0.0",
            "127.0.0.1",
            "169.254.1.1",
            "224.0.0.1",
            "::",
            "::1",
            "fe80::1",
            "ff02::1",
            "2001:db8::1",
            "::ffff:10.0.0.1",
        ] {
            assert!(!is_private_http_address(&ip(value)), "{value}");
            assert!(!is_webtransport_address(&ip(value)), "{value}");
        }
    }

    #[test]
    fn withholds_public_ipv6_until_firewall_authorization_completes() {
        let endpoints =
            NetworkEndpoints::from_ips(vec![ip("192.168.1.4"), ip("2408:8207:1234::8")]);
        let pending = endpoints.published(17691, PublicIpv6State::Authorizing);
        assert_eq!(pending.public_ipv6_state, PublicIpv6State::Authorizing);
        assert_eq!(pending.benchmark_endpoints.len(), 1);
        assert!(pending.benchmark_endpoints[0].contains("192.168.1.4"));

        let available = endpoints.published(17691, PublicIpv6State::Available);
        assert_eq!(available.benchmark_endpoints.len(), 2);
        assert!(
            available
                .benchmark_endpoints
                .iter()
                .any(|endpoint| endpoint.contains("2408:8207:1234::8"))
        );
        assert_ne!(pending.network_epoch, available.network_epoch);
    }
}

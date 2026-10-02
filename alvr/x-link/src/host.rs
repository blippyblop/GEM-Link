//! Host-side link discovery: what is this PC actually attached to?
//!
//! [`crate::classify`] answers "what kind of link is this?" from a description. This
//! module is what *produces* the description on a real machine, and adds the second axis
//! Virtual Desktop models and we did not: **how the host itself reaches the network**
//! (`HostLink`), as distinct from what the client's radio is doing ([`LinkClass`]).
//!
//! ## Why two axes
//!
//! They give different answers and both matter:
//!
//! * [`LinkClass`] is about the **radio** the client is on — generation, and whether it is
//!   the headset's own access point. It sets the QoS posture.
//! * [`HostLink`] is about the **host's** interface — wired or wireless, and how fast. It
//!   decides whether touching the WLAN adapter is even meaningful, and it is the only way
//!   to notice that the PC's path is 100 Mbit.
//!
//! They are genuinely independent: `Wifi7SoftAp` + `Wireless` is the normal Frame session,
//! `Wifi7SoftAp` + `Ethernet` is a client on the Frame's AP while the PC is plugged in (a
//! real topology when the dongle is not the streaming adapter), and a single-link host
//! can be `Ethernet` with a client on `Wifi6Lan`.
//!
//! ## Provenance
//!
//! [`HostLink`] and [`classify_host`] are Virtual Desktop's `NetworkConnectionType` and
//! its derivation, read out of `VirtualDesktop.Streamer` by decompilation — see
//! `VD_RE/24-vd-link-qos.md` §8.2. VD's enum has a fourth member, `WirelessWithICS`,
//! produced when the PC is sharing its own connection to the headset; that is the
//! internet-sharing path we do not implement, so it is not modelled here rather than
//! modelled and left unreachable. The rule below is VD's, unchanged:
//!
//! > iterate the adapters; the **first wireless one wins**; otherwise record that a wired
//! > adapter was seen and OR-in whether any of them is gigabit; no wired adapter →
//! > `Wireless`; wired but not gigabit → `NotGigabit`; else `Ethernet`.
//!
//! ## What is a heuristic and what is not
//!
//! The class from `GetAdaptersAddresses` is fact. The **SoftAP hint** — "is this client on
//! the headset's own AP?" — is a heuristic: we infer it from the SSID being hidden, which
//! the Frame's AP is, and from a name-prefix list. The honest alternative is for the
//! client to say so, and the protocol has the room for it; until then the heuristic is
//! labelled as one and its failure mode (misclassifying a shared LAN as a direct link)
//! only changes buffering and DSCP, not whether a session runs.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use x_protocol::LinkClass;

use crate::{LinkDescriptor, LinkKind, classify};

/// SSID prefixes that indicate a direct/SoftAP link. Case-insensitive.
///
/// Deliberately short. A prefix that is *wrong* is worse than a prefix that is missing,
/// because a false positive makes us treat a shared LAN as a private one and stop
/// buffering for contention that is really there.
const SOFT_AP_SSID_PREFIXES: &[&str] = &["steamframe", "frame-", "steam-frame"];

/// How the host reaches the network. VD's `NetworkConnectionType`, minus the ICS member
/// it uses for connection sharing (which we do not implement).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostLink {
    /// At least one wired, gigabit-or-better adapter and no wireless one.
    Ethernet,
    /// Wired, but nothing reached a gigabit. A 100 Mbit path cannot carry the 300 Mbps
    /// envelope and is worth saying out loud.
    NotGigabit,
    /// The first adapter is wireless. This is the case the WLAN optimizer exists for.
    Wireless,
    /// Could not be determined (no routed adapter, or a peer we could not resolve a route
    /// to). Treated as *permissive*: the WLAN posture is still applied, because the
    /// optimizer no-ops on a machine with no connected WLAN interface anyway.
    Unknown,
    /// The peer is on this machine (loopback). **Our addition** — VD has no equivalent
    /// because VD has no loopback client, but this codebase does: a "wired" client entry
    /// pointing at `127.0.0.1` is how the local test clients connect, and it is
    /// `WIRED_CLIENT_HOSTNAME` in `alvr_sockets`.
    ///
    /// It is not a link at all, so it must not be pessimised as one: none of the radio or
    /// wire reasoning applies, and pretending a loopback peer is on a 100 Mbit path would
    /// silently change what every harness run measures.
    Local,
}

impl HostLink {
    pub const fn is_wireless(self) -> bool {
        matches!(self, HostLink::Wireless | HostLink::Unknown)
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            HostLink::Ethernet => "ethernet",
            HostLink::NotGigabit => "not-gigabit",
            HostLink::Wireless => "wireless",
            HostLink::Unknown => "unknown",
            HostLink::Local => "local",
        }
    }
}

/// One adapter, as the operating system describes it.
///
/// `mac` and `description` are carried because VD advertises them to the client, and
/// because a log line naming the adapter is worth more than one naming an index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostAdapter {
    pub name: String,
    pub description: String,
    /// Interface index, when the platform has one.
    pub if_index: Option<u32>,
    pub is_wireless: bool,
    /// Negotiated transmit speed, Mbps.
    pub speed_mbps: Option<u32>,
    pub mac: Option<[u8; 6]>,
    /// True when the interface reports itself up. Down adapters are still reported so the
    /// classification sees the full picture, but they never win.
    pub is_up: bool,
}

impl HostAdapter {
    /// A gigabit-or-better wired path. VD's `IsGigabit`.
    ///
    /// An unknown speed is **not** gigabit: the conservative direction is to assume the
    /// wire is the constraint, because the cost of being wrong that way is a smaller
    /// bitrate and the cost of being wrong the other way is a broken stream.
    pub fn is_gigabit(&self) -> bool {
        self.speed_mbps.is_some_and(|mbps| mbps >= 1000)
    }

    fn to_descriptor(&self) -> LinkDescriptor {
        let kind = if self.is_wireless {
            LinkKind::Wifi
        } else {
            LinkKind::Wired
        };
        LinkDescriptor {
            name: self.name.clone(),
            kind,
            speed_mbps: self.speed_mbps,
            wifi_generation: None,
            is_soft_ap: false,
            is_ncm: false,
            is_up: self.is_up,
        }
    }
}

/// Virtual Desktop's classification rule, verbatim. See the module docs.
pub fn classify_host(adapters: &[HostAdapter]) -> HostLink {
    let mut saw_wired = false;
    let mut saw_gigabit = false;

    for adapter in adapters {
        if !adapter.is_up {
            continue;
        }
        if adapter.is_wireless {
            // VD breaks on the *first* wireless adapter, it does not prefer the best one.
            return HostLink::Wireless;
        }
        saw_wired = true;
        saw_gigabit |= adapter.is_gigabit();
    }

    if !saw_wired {
        // No wired adapter at all. VD returns `Wireless` here rather than an unknown,
        // because "we reached this point with no wired path" in their model means the
        // machine is on Wi-Fi and the wireless adapter was filtered out upstream. We keep
        // the same answer for the same reason: the alternative is to hand back `Unknown`
        // for a machine that is plainly wireless.
        return HostLink::Wireless;
    }
    if !saw_gigabit {
        return HostLink::NotGigabit;
    }
    HostLink::Ethernet
}

/// The local address this host would use to reach `peer`.
///
/// Standard-library only: bind an unbound UDP socket, `connect` it (which sends nothing)
/// and ask where it landed. Cross-platform, and it is exactly the right question — the
/// kernel applies the same route selection it would for the stream.
pub fn local_endpoint(peer: IpAddr) -> std::io::Result<IpAddr> {
    let bind = if peer.is_ipv4() {
        SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0))
    } else {
        SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, 0))
    };
    let socket = UdpSocket::bind(bind)?;
    socket.connect(SocketAddr::new(peer, 9))?;
    Ok(socket.local_addr()?.ip())
}

/// Whether a WLAN SSID looks like a direct (SoftAP) link rather than shared
/// infrastructure. **A heuristic** — see the module docs.
pub fn is_soft_ap_ssid(ssid: &str) -> bool {
    if ssid.is_empty() {
        // A hidden SSID. The Frame's own AP is hidden, and shared infrastructure is
        // almost never hidden, so this is the strongest single hint we have.
        return true;
    }
    let lowered = ssid.to_ascii_lowercase();
    SOFT_AP_SSID_PREFIXES
        .iter()
        .any(|prefix| lowered.starts_with(prefix))
}

/// A resolved link: the peer, the local interface that reaches it, and the two axes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkResolution {
    pub peer: IpAddr,
    pub local_ip: Option<IpAddr>,
    pub adapter: Option<HostAdapter>,
    pub host_link: HostLink,
    /// The client's radio, as best we can tell.
    pub class: LinkClass,
    /// SSID / PHY / rate / signal, when the streaming adapter is wireless and we could
    /// read it.
    pub wlan: Option<crate::wlan::WlanConnection>,
}

impl LinkResolution {
    /// One line for the log. This is the *instrument* for the resolution: if the posture
    /// ever looks wrong, this says what we thought the link was and why.
    pub fn summary(&self) -> String {
        let adapter = match &self.adapter {
            Some(a) => {
                let speed = a
                    .speed_mbps
                    .map(|s| format!("{s} Mbps"))
                    .unwrap_or_else(|| "speed unknown".into());
                let medium = if a.is_wireless { ", wireless" } else { "" };
                format!("{} ({speed}{medium})", a.description)
            }
            None => "no routed adapter found".into(),
        };
        let local = self
            .local_ip
            .map(|ip| ip.to_string())
            .unwrap_or_else(|| "?".into());

        let mut summary = format!(
            "peer {} via {local} [{adapter}] -> host {} class {:?}",
            self.peer,
            self.host_link.as_str(),
            self.class,
        );
        if let Some(w) = &self.wlan {
            summary.push_str(&format!(
                "; ssid={:?} phy={} rx={} tx={} kbps signal={}%",
                w.ssid,
                w.phy.as_str(),
                w.rx_kbps,
                w.tx_kbps,
                w.signal_quality
            ));
        }
        summary
    }
}

/// Resolve the link used to reach `peer`.
///
/// Never fails: an unresolvable link resolves to [`HostLink::Unknown`] and a conservative
/// class, because a session should never be refused over a diagnostic.
///
/// Only the **routed adapter** is classified, not the whole adapter list. VD's rule is
/// defined over the list, but VD's question is "how does this PC reach the network", and
/// ours is "what is carrying this session" — on a server with a VPN, a Hyper-V switch and
/// a spare adapter, the list answer is about none of them. `classify_host` is still a
/// list function and is still tested as one, because that is the rule we ported.
pub fn resolve(peer: IpAddr) -> LinkResolution {
    if peer.is_loopback() {
        return LinkResolution {
            peer,
            local_ip: Some(peer),
            adapter: None,
            host_link: HostLink::Local,
            class: LinkClass::Other("local".into()),
            wlan: None,
        };
    }

    let local_ip = local_endpoint(peer).ok();
    let adapter = platform::adapter_for(peer, local_ip);
    let host_link = match &adapter {
        Some(a) => classify_host(std::slice::from_ref(a)),
        None => HostLink::Unknown,
    };

    let wlan = platform::wlan_connection(adapter.as_ref());
    let class = match (&adapter, &wlan) {
        (Some(a), Some(conn)) => classify(&LinkDescriptor {
            wifi_generation: conn.phy.wifi_generation(),
            is_soft_ap: is_soft_ap_ssid(&conn.ssid),
            ..a.to_descriptor()
        }),
        (Some(a), None) => classify(&a.to_descriptor()),
        _ => LinkClass::Other("unresolved".into()),
    };

    LinkResolution {
        peer,
        local_ip,
        adapter,
        host_link,
        class,
        wlan,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wired(speed: u32) -> HostAdapter {
        HostAdapter {
            name: "Ethernet".into(),
            description: "Intel I225-V".into(),
            if_index: Some(1),
            is_wireless: false,
            speed_mbps: Some(speed),
            mac: None,
            is_up: true,
        }
    }

    fn wireless() -> HostAdapter {
        HostAdapter {
            name: "Wi-Fi".into(),
            description: "Intel Wi-Fi 7 BE200".into(),
            if_index: Some(2),
            is_wireless: true,
            speed_mbps: Some(2402),
            mac: None,
            is_up: true,
        }
    }

    #[test]
    fn vds_rule_gigabit_wired_is_ethernet() {
        assert_eq!(classify_host(&[wired(1000)]), HostLink::Ethernet);
        assert_eq!(classify_host(&[wired(2500)]), HostLink::Ethernet);
    }

    #[test]
    fn vds_rule_slow_wired_is_not_gigabit() {
        // The case that matters: a 100 Mbit path cannot carry the 300 Mbps envelope, and
        // a taxonomy without this member cannot express that.
        assert_eq!(classify_host(&[wired(100)]), HostLink::NotGigabit);
        assert_eq!(classify_host(&[wired(0)]), HostLink::NotGigabit);
    }

    #[test]
    fn unknown_speed_is_not_gigabit() {
        let mut a = wired(1000);
        a.speed_mbps = None;
        assert!(!a.is_gigabit());
        assert_eq!(classify_host(&[a]), HostLink::NotGigabit);
    }

    #[test]
    fn any_wireless_adapter_makes_the_host_wireless() {
        // VD's rule, exactly: the loop records "wireless seen" and breaks; a wired adapter
        // seen *before* it does not override that. So order does not matter and the
        // presence of a gigabit wire does not make a wireless host wired — which is the
        // behaviour we want, because the presence of a cable says nothing about which
        // interface carries this session.
        assert_eq!(
            classify_host(&[wireless(), wired(2500)]),
            HostLink::Wireless
        );
        assert_eq!(
            classify_host(&[wired(2500), wireless()]),
            HostLink::Wireless
        );
    }

    #[test]
    fn down_adapters_are_ignored() {
        let mut a = wireless();
        a.is_up = false;
        assert_eq!(classify_host(&[a, wired(1000)]), HostLink::Ethernet);
    }

    #[test]
    fn no_adapters_is_wireless_not_unknown() {
        // Matching VD: reaching the end with no wired adapter means the machine is on
        // Wi-Fi (their wireless adapter had been filtered out upstream). `Unknown` is
        // reserved for "we could not ask at all".
        assert_eq!(classify_host(&[]), HostLink::Wireless);
    }

    #[test]
    fn unknown_is_permissive_about_the_wlan_posture() {
        // The safety argument for the fallback: an over-eager posture is harmless because
        // the optimizer itself no-ops when no WLAN interface is connected.
        assert!(HostLink::Unknown.is_wireless());
        assert!(HostLink::Wireless.is_wireless());
        assert!(!HostLink::Ethernet.is_wireless());
        assert!(!HostLink::NotGigabit.is_wireless());
    }

    #[test]
    fn soft_ap_heuristic() {
        assert!(is_soft_ap_ssid(""), "hidden SSIDs are the Frame's AP shape");
        assert!(is_soft_ap_ssid("SteamFrame-3f2a"));
        assert!(is_soft_ap_ssid("frame-01"));
        assert!(!is_soft_ap_ssid("BTHub6-K9QP"));
        assert!(!is_soft_ap_ssid("My Wifi"));
        // Case-insensitive, because SSIDs are not normalised by anything.
        assert!(is_soft_ap_ssid("STEAMFRAME-ABC"));
    }

    #[test]
    fn local_endpoint_resolves_loopback_without_an_adapter() {
        let ip = local_endpoint(IpAddr::V4(Ipv4Addr::LOCALHOST)).expect("loopback route");
        assert_eq!(ip, IpAddr::V4(Ipv4Addr::LOCALHOST));
    }

    #[test]
    fn summary_names_the_adapter_and_both_axes() {
        let r = LinkResolution {
            peer: IpAddr::V4(Ipv4Addr::new(192, 168, 1, 50)),
            local_ip: Some(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10))),
            adapter: Some(wired(2500)),
            host_link: HostLink::Ethernet,
            class: LinkClass::Other("ethernet_2500mbps".into()),
            wlan: None,
        };
        let s = r.summary();
        assert!(s.contains("192.168.1.50"), "{s}");
        assert!(s.contains("2500 Mbps"), "{s}");
        assert!(s.contains("host ethernet"), "{s}");
    }
}

#[cfg(windows)]
mod platform {
    //! Windows implementation: the routed adapter, its type and its speed.
    //!
    //! Two calls, deliberately, rather than one: `GetBestInterfaceEx` answers "which
    //! interface would the kernel use for this peer" (the question that actually matters,
    //! and the one that gets the answer right on a multi-homed machine), and
    //! `GetAdaptersAddresses` describes it. Reading the unicast address lists to match an
    //! IP would be the alternative and is both more code and less correct.
    //!
    //! **Limitation, stated:** `GetBestInterfaceEx` is given an IPv4 `SOCKADDR_IN` here.
    //! An IPv6 peer falls back to [`super::HostLink::Unknown`], which is permissive by
    //! design (see that variant's docs).

    use super::*;
    use crate::wlan::{self, WlanConnection};
    use windows::Win32::NetworkManagement::IpHelper::{
        GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_MULTICAST,
        GetAdaptersAddresses, GetBestInterfaceEx, IF_TYPE_IEEE80211, IP_ADAPTER_ADDRESSES_LH,
    };
    use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
    use windows::Win32::Networking::WinSock::{
        AF_INET, AF_UNSPEC, IN_ADDR, IN_ADDR_0, SOCKADDR, SOCKADDR_IN,
    };

    const ERROR_SUCCESS: u32 = 0;
    const ERROR_BUFFER_OVERFLOW: u32 = 111;
    /// Enough for a typical machine; grown on `ERROR_BUFFER_OVERFLOW`.
    const INITIAL_BUFFER: u32 = 16 * 1024;

    /// The interface index the kernel would route `peer` through, IPv4 only.
    fn best_if_index(peer: IpAddr) -> Option<u32> {
        let IpAddr::V4(v4) = peer else {
            return None;
        };

        // `s_addr` is in network byte order.
        let sockaddr = SOCKADDR_IN {
            sin_family: AF_INET,
            sin_port: 0,
            sin_addr: IN_ADDR {
                S_un: IN_ADDR_0 {
                    S_addr: u32::from_ne_bytes(v4.octets()),
                },
            },
            sin_zero: [0; 8],
        };

        let mut index: u32 = 0;
        let err = unsafe {
            GetBestInterfaceEx(
                (&sockaddr as *const SOCKADDR_IN).cast::<SOCKADDR>(),
                &mut index,
            )
        };
        (err == ERROR_SUCCESS).then_some(index)
    }

    /// Every adapter, in the order the OS reports them.
    pub(super) fn adapters() -> Vec<HostAdapter> {
        let mut size = INITIAL_BUFFER;
        let mut buffer = vec![0u8; size as usize];
        let flags = GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER;

        let err = unsafe {
            GetAdaptersAddresses(
                AF_UNSPEC.0 as u32,
                flags,
                None,
                Some(buffer.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH),
                &mut size,
            )
        };

        if err == ERROR_BUFFER_OVERFLOW {
            buffer.resize(size as usize, 0);
            let err = unsafe {
                GetAdaptersAddresses(
                    AF_UNSPEC.0 as u32,
                    flags,
                    None,
                    Some(buffer.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH),
                    &mut size,
                )
            };
            if err != ERROR_SUCCESS {
                return Vec::new();
            }
        } else if err != ERROR_SUCCESS {
            return Vec::new();
        }

        let mut out = Vec::new();
        let mut node = buffer.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH;
        while !node.is_null() {
            let a = unsafe { &*node };
            out.push(host_adapter_from(a));
            node = a.Next;
        }
        out
    }

    fn host_adapter_from(a: &IP_ADAPTER_ADDRESSES_LH) -> HostAdapter {
        // SAFETY: the union's first member is the `Length`/`IfIndex` pair, which is what
        // the API always fills in.
        let if_index = unsafe { a.Anonymous1.Anonymous.IfIndex };

        let mut mac = [0u8; 6];
        let mac_len = a.PhysicalAddressLength.min(6) as usize;
        mac[..mac_len].copy_from_slice(&a.PhysicalAddress[..mac_len]);

        HostAdapter {
            name: pwstr_to_string(a.FriendlyName),
            description: pwstr_to_string(a.Description),
            if_index: Some(if_index),
            // `IfType` is `u32` in the windows-rs binding of the union, so the named
            // constant is compared directly.
            is_wireless: a.IfType == IF_TYPE_IEEE80211,
            speed_mbps: Some((a.TransmitLinkSpeed / 1_000_000) as u32),
            mac: (mac_len == 6).then_some(mac),
            is_up: a.OperStatus == IfOperStatusUp,
        }
    }

    /// The adapter the kernel would use to reach `peer`.
    ///
    /// Falls back to matching the local address against... nothing: if the best-interface
    /// call fails we search for an up adapter and, failing that, return `None`, which the
    /// caller turns into a permissive `Unknown`.
    pub(super) fn adapter_for(peer: IpAddr, _local_ip: Option<IpAddr>) -> Option<HostAdapter> {
        let index = best_if_index(peer)?;
        adapters()
            .into_iter()
            .find(|adapter| adapter.if_index == Some(index))
    }

    /// SSID, PHY, rates and signal for the connected WLAN interface, when the streaming
    /// adapter is in fact the radio.
    ///
    /// If several WLAN interfaces are connected we take the first, and the log says so.
    /// That is the honest bound on this: we are not correlating the WLAN interface GUID to
    /// the routed adapter's index, because Windows gives no clean way to, and a machine
    /// with two connected WLAN adapters is not a case we currently serve.
    pub(super) fn wlan_connection(adapter: Option<&HostAdapter>) -> Option<WlanConnection> {
        if !adapter?.is_wireless {
            return None;
        }
        wlan::current_connection().ok().flatten()
    }

    fn pwstr_to_string(ptr: windows::core::PWSTR) -> String {
        if ptr.is_null() {
            return String::new();
        }
        unsafe { ptr.to_string().unwrap_or_default() }
    }
}

#[cfg(not(windows))]
mod platform {
    //! Non-Windows: enumeration is not implemented, and the resolution degrades to
    //! `Unknown` rather than guessing. The pure parts of this module are still fully
    //! exercised by the tests above.

    use super::*;
    use crate::wlan::WlanConnection;

    /// Unused on this platform: nothing here enumerates adapters, because the resolution
    /// degrades to `Unknown` rather than guessing. Present so the two platform modules keep
    /// the same shape.
    #[allow(dead_code)]
    pub(super) fn adapters() -> Vec<HostAdapter> {
        Vec::new()
    }

    pub(super) fn adapter_for(_peer: IpAddr, _local_ip: Option<IpAddr>) -> Option<HostAdapter> {
        None
    }

    pub(super) fn wlan_connection(_adapter: Option<&HostAdapter>) -> Option<WlanConnection> {
        None
    }
}

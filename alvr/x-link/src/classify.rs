//! Link classification: what kind of wire is this, really?
//!
//! `x-protocol` already carries a [`LinkClass`] and negotiates on it
//! (`link_classes` on both capability sets, `latency_preference()` for ordering), but
//! nothing ever *produced* one from the machine. This module is that producer.
//!
//! The classification is deliberately a pure function of a [`LinkDescriptor`] rather
//! than a pile of `GetAdaptersAddresses` / `nl80211` calls: enumerating interfaces is
//! platform code that belongs with the platform it enumerates, while *deciding what the
//! link is* is policy and belongs here where it can be tested. Callers build a
//! descriptor from whatever their OS gives them; the interesting cases are testable on
//! every platform.
//!
//! The four classes we can currently distinguish are exactly the four the protocol
//! negotiates, plus a catch-all:
//!
//! | Class | Meaning | Why it is its own class |
//! |---|---|---|
//! | [`LinkClass::UsbNcm`] | USB-C NCM gadget — a real NIC over the cable | no airtime contention at all; the latency floor of the product |
//! | [`LinkClass::Wifi7SoftAp`] | the *headset's own* 6 GHz AP (the Frame topology) | one AP we control, on a clean band, with the headset's own governor already pinned |
//! | [`LinkClass::Wifi7Lan`] | Wi-Fi 7 through shared infrastructure | same PHY, contended airtime, someone else's AP |
//! | [`LinkClass::Wifi6Lan`] | Wi-Fi 6 through shared infrastructure | contended *and* a narrower channel |
//!
//! ## Why SoftAP is not just "Wi-Fi 7"
//!
//! The Frame creates its own hidden SoftAP and the PC's dongle joins it
//! (`analysis/frame-analysis/wireless-stack.md`). The AP is
//! ours, the headset runs `softapmanager` on it (power save off, CQM watchdogs, pinned
//! GI/LTF), and there is no third party on the channel. That is a materially different
//! link from "Wi-Fi 7 to the house router", which is why they negotiate separately —
//! and why the QoS posture of the two differs ([`crate::qos`]).

use x_protocol::LinkClass;

/// The kind of interface, as the operating system describes it. Kept coarse on purpose:
/// everything finer (generation, band, AP identity) lives in the descriptor's own fields
/// so that no OS has to be able to report all of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkKind {
    /// A physical or virtual Ethernet NIC, including USB tethering interfaces.
    Wired,
    /// 802.11.
    Wifi,
    /// Loopback, tunnels, VPNs and anything else that is not a real link.
    Virtual,
    Unknown,
}

/// Everything we can learn about an interface, in OS-neutral terms.
///
/// Every field is optional-ish on purpose: an implementation that cannot see the Wi-Fi
/// generation should still be able to classify a wired link correctly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkDescriptor {
    /// Interface name as the OS reports it (`wlan0`, `Ethernet 2`, `enp5s0`…). Used only
    /// for logging and for the `Other` classification's label.
    pub name: String,
    pub kind: LinkKind,
    /// Negotiated/nominal link speed, Mbps, if known.
    pub speed_mbps: Option<u32>,
    /// 802.11 generation (6, 7, …) if this is Wi-Fi and the OS can tell us.
    pub wifi_generation: Option<u8>,
    /// True when this station is associated to the *headset's own* SoftAP rather than to
    /// shared infrastructure. The caller has to know this — it is normally established at
    /// discovery time, not by the OS.
    pub is_soft_ap: bool,
    /// True when this interface is a USB NCM gadget (the wired tethered path).
    pub is_ncm: bool,
    /// True when the interface is up and has a usable address.
    pub is_up: bool,
}

impl LinkDescriptor {
    /// Convenience constructor for the common "just a name and a kind" case.
    pub fn new(name: impl Into<String>, kind: LinkKind) -> Self {
        Self {
            name: name.into(),
            kind,
            speed_mbps: None,
            wifi_generation: None,
            is_soft_ap: false,
            is_ncm: false,
            is_up: true,
        }
    }
}

/// Decide which negotiated [`LinkClass`] this link is.
///
/// Ordering of the checks is the policy:
///
/// 1. **NCM beats everything.** If the interface says it is an NCM gadget it is wired,
///    whatever else the OS thinks. The wired path is definition-of-done #6 and the
///    lowest-latency profile, so misclassifying it as Wi-Fi would cost the user the
///    feature.
/// 2. **The headset's own SoftAP** next — it is the Frame's normal topology and it gets
///    its own QoS posture.
/// 3. **Then Wi-Fi by generation** against shared infrastructure.
/// 4. **Then anything else**, labelled by kind so the log says something useful.
pub fn classify(link: &LinkDescriptor) -> LinkClass {
    if link.is_ncm {
        return LinkClass::UsbNcm;
    }

    match link.kind {
        LinkKind::Wifi => {
            if link.is_soft_ap {
                // The Frame's own AP is Wi-Fi 7 (6 GHz, 80/160 MHz) per the firmware
                // analysis. A SoftAP on older silicon has never been seen in the field,
                // so it is not given a class of its own — but it is not silently
                // promoted to Wi-Fi 7 either.
                return if link.wifi_generation.unwrap_or(0) >= 7 {
                    LinkClass::Wifi7SoftAp
                } else {
                    LinkClass::Other(format!("softap_wifi{}", link.wifi_generation.unwrap_or(0)))
                };
            }

            match link.wifi_generation {
                Some(g) if g >= 7 => LinkClass::Wifi7Lan,
                Some(6) => LinkClass::Wifi6Lan,
                Some(g) => LinkClass::Other(format!("wifi{g}_lan")),
                // A Wi-Fi interface whose generation we cannot see is closer to the
                // older shared-medium case than to the Frame's clean AP, and the
                // conservative posture is the one that buffers more and asks for less.
                None => LinkClass::Other("wifi_lan_unknown".into()),
            }
        }
        LinkKind::Wired => match link.speed_mbps {
            Some(mbps) => LinkClass::Other(format!("ethernet_{mbps}mbps")),
            None => LinkClass::Other("ethernet".into()),
        },
        LinkKind::Virtual => LinkClass::Other(format!("virtual:{}", link.name)),
        LinkKind::Unknown => LinkClass::Other(format!("unknown:{}", link.name)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wifi(generation: u8) -> LinkDescriptor {
        LinkDescriptor {
            wifi_generation: Some(generation),
            ..LinkDescriptor::new("wlan0", LinkKind::Wifi)
        }
    }

    #[test]
    fn ncm_wins_over_everything_else() {
        // A tether interface may also look like "wired" with a Wi-Fi generation borrowed
        // from somewhere; the NCM flag is authoritative.
        let link = LinkDescriptor {
            is_ncm: true,
            kind: LinkKind::Wifi,
            wifi_generation: Some(7),
            ..LinkDescriptor::new("usb0", LinkKind::Wifi)
        };
        assert_eq!(classify(&link), LinkClass::UsbNcm);
    }

    #[test]
    fn soft_ap_is_its_own_class_on_wifi7() {
        let link = LinkDescriptor {
            is_soft_ap: true,
            ..wifi(7)
        };
        assert_eq!(classify(&link), LinkClass::Wifi7SoftAp);
    }

    #[test]
    fn soft_ap_on_older_silicon_is_not_promoted() {
        let link = LinkDescriptor {
            is_soft_ap: true,
            ..wifi(6)
        };
        assert_eq!(
            classify(&link),
            LinkClass::Other("softap_wifi6".into()),
            "must not be reported as Wi-Fi 7 SoftAP just because it is an AP"
        );
    }

    #[test]
    fn lan_is_split_by_generation() {
        assert_eq!(classify(&wifi(7)), LinkClass::Wifi7Lan);
        assert_eq!(classify(&wifi(6)), LinkClass::Wifi6Lan);
        assert_eq!(
            classify(&wifi(5)),
            LinkClass::Other("wifi5_lan".into()),
            "older Wi-Fi is not a class we tune for, but it is not Wi-Fi 6 either"
        );
    }

    #[test]
    fn unknown_generation_is_conservative() {
        let link = LinkDescriptor::new("wlan0", LinkKind::Wifi);
        assert_eq!(classify(&link), LinkClass::Other("wifi_lan_unknown".into()));
    }

    #[test]
    fn plain_ethernet_is_not_ncm() {
        let link = LinkDescriptor {
            speed_mbps: Some(1000),
            ..LinkDescriptor::new("Ethernet", LinkKind::Wired)
        };
        assert_eq!(
            classify(&link),
            LinkClass::Other("ethernet_1000mbps".into())
        );
    }

    #[test]
    fn every_class_classifies_to_a_link_class_the_protocol_can_negotiate() {
        // The point of the module: whatever we hand it, the output is a `LinkClass`.
        // This is a compile-and-totality check, not a policy check.
        let cases = [
            LinkDescriptor::new("lo", LinkKind::Virtual),
            LinkDescriptor::new("tun0", LinkKind::Unknown),
            LinkDescriptor::new("eth0", LinkKind::Wired),
            wifi(6),
            wifi(7),
        ];
        for case in cases {
            let _: LinkClass = classify(&case);
        }
    }
}

//! Per-link-class QoS posture: what the server asks of the link while a session runs.
//!
//! One [`LinkQosProfile`] per negotiated [`LinkClass`]. The profile is *policy*, and the
//! policy here is deliberately small: three things we can actually act on today, and no
//! knob we cannot justify.
//!
//! ## Provenance of every number in this file
//!
//! The project rule (§8c of the handoff) is to establish what ALVR and VD already do
//! before inventing anything, so each field says where it comes from:
//!
//! * **`wlan`** — from Virtual Desktop's `libVirtualDesktopNet.dll`, read out by
//!   disassembly (`VD_RE/24-vd-link-qos.md` §2).
//!   VD holds media-streaming-mode on and background-scan off for the whole session, on
//!   every connected WLAN interface, re-asserted every 11 s.
//! * **`dscp`** — the IETF recommendation for real-time media is EF (RFC 4594), and
//!   ALVR already has the machinery (`sockets::set_dscp`, `DscpTos::ExpeditedForwarding`)
//!   but ships with `connection.dscp = None`. Marking is only *read* by the network if
//!   something along the path honours it, which on a two-node SoftAP is at best the
//!   adapter's own queueing — so this is a hint, not a guarantee.
//! * **`jitter_buffer_frames`** — upstream ALVR's `max_buffering_frames` default is
//!   `2.0`, one number for every link. This splits it by class in the direction the
//!   physics points: a wired NCM gadget has no reordering to absorb, a shared Wi-Fi
//!   medium has some.
//! * **`expected_throughput_mbps`** — a *planning* figure, not a measurement. For the
//!   Frame classes it is the device analysis
//!   (`analysis/frame-analysis/wireless-stack.md`): Wi-Fi 7,
//!   6 GHz, 80/160 MHz, with the headset's own governor deliberately trading peak rate for
//!   stability. For Wi-Fi 6 it is the same PHY family one generation down. It exists so
//!   that a *client* can be told what the link is expected to carry; the actual rate
//!   control stays with the existing adaptive-bitrate loop.
//!
//! Deliberately **not** in the profile: resolution, codec, foveation geometry, and
//! anything else that is the client's call or the encoder's. A link profile that reached
//! into those would be the "policy engine" mistake in reverse.

use crate::wlan::WlanPosture;
use x_protocol::LinkClass;

/// Differentiated Services Code Point for media datagrams.
///
/// Mirrors the useful subset of ALVR's `DscpTos` so that the two can be mapped without
/// pulling the whole settings schema in here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dscp {
    /// Leave the OS default. Correct when the path has nothing that honours marking.
    BestEffort,
    /// Class selector, 1–7 (diffserv backwards compatibility).
    ClassSelector(u8),
    /// Assured forwarding: class 1–4 and drop precedence 1–3.
    AssuredForwarding {
        /// AF class, 1–4.
        class: u8,
        /// Drop precedence, 1–3.
        drop_precedence: u8,
    },
    /// Expedited forwarding — the low-latency, low-loss, low-jitter class.
    ExpeditedForwarding,
}

impl Dscp {
    /// The 6-bit DSCP value, shifted into the top of an 8-bit DS field — i.e. exactly what
    /// goes into `IP_TOS` / `IPV6_TCLASS`.
    ///
    /// ALVR's `sockets::set_dscp` builds the same numbers from `DscpTos` and then shifts
    /// left by two when calling `set_tos_v4`; this returns the already-shifted byte so the
    /// two are directly comparable.
    ///
    /// **One deliberate divergence from upstream.** ALVR computes assured forwarding as
    /// `(class << 3) | drop_probability`, using `DropProbability::{Low = 0x01, Medium =
    /// 0x10, High = 0x11}` as raw bytes — so `Medium` is 16, not 2, and it lands on top of
    /// the class bits. The IETF value is `class * 8 + drop * 2`, i.e. the precedence needs
    /// its own `<< 1`. Every AF marking ALVR emits today is therefore wrong; we compute it
    /// correctly here and this is the note that says why the two disagree.
    pub const fn ds_field(self) -> u8 {
        let dscp = match self {
            Dscp::BestEffort => 0b000000,
            Dscp::ClassSelector(precedence) => (precedence & 0b111) << 3,
            Dscp::AssuredForwarding {
                class,
                drop_precedence,
            } => ((class & 0b111) << 3) | ((drop_precedence & 0b11) << 1),
            Dscp::ExpeditedForwarding => 0b101110,
        };
        dscp << 2
    }
}

/// What the server asks of one link class for the duration of a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinkQosProfile {
    /// The posture the WLAN optimizer should hold, if this class is Wi-Fi at all.
    pub wlan: WlanPosture,
    /// DS field for media datagrams.
    pub dscp: Dscp,
    /// Suggested client jitter-buffer depth, in frames.
    pub jitter_buffer_frames: u8,
    /// Planning figure for sustained throughput, Mbps (not a measurement).
    pub expected_throughput_mbps: u32,
    /// True when this medium is shared with traffic we do not control, which is the
    /// condition that makes queueing (and therefore marking and buffering) matter.
    pub contended: bool,
}

/// The posture for a link class.
///
/// The wired class holds *no* WLAN posture at all ([`WlanPosture::off`]): an NCM gadget
/// means the radio is not on this path, so touching it would be nonsense — and worse, on a
/// machine that is *also* on Wi-Fi it would degrade the connection the user is not
/// streaming over.
pub fn profile_for(class: &LinkClass) -> LinkQosProfile {
    match class {
        // Wired USB-C NCM: no airtime, no reordering, no scanning to disable. The
        // whole point of the wired path is that none of this machinery is needed.
        LinkClass::UsbNcm => LinkQosProfile {
            wlan: WlanPosture::off(),
            dscp: Dscp::ExpeditedForwarding,
            jitter_buffer_frames: 1,
            expected_throughput_mbps: 1000,
            contended: false,
        },

        // The headset's own 6 GHz AP: one AP, ours, clean band — but still a radio, so
        // the adapter still gets put in streaming posture. This is the class VD's
        // optimizer was written for.
        //
        // `contended` is true, but for a reason unlike the LAN classes: the contention is
        // *ours*. One radio carries the video down and the tracking, audio and control
        // traffic up, and the headset's SoftAP governor deliberately trades peak rate for
        // stability on the shared channel. "No third party on the channel" is not the same
        // as "no contention".
        LinkClass::Wifi7SoftAp => LinkQosProfile {
            wlan: WlanPosture::streaming(),
            dscp: Dscp::ExpeditedForwarding,
            jitter_buffer_frames: 2,
            expected_throughput_mbps: 900,
            contended: true,
        },

        // Wi-Fi 7 through somebody else's AP: same PHY, shared airtime whose schedule we
        // cannot see. Buffer more, expect less, mark harder.
        LinkClass::Wifi7Lan => LinkQosProfile {
            wlan: WlanPosture::streaming(),
            dscp: Dscp::ExpeditedForwarding,
            jitter_buffer_frames: 3,
            expected_throughput_mbps: 600,
            contended: true,
        },

        // Wi-Fi 6 via infrastructure: narrower channels and no 6 GHz clean band in the
        // common case.
        LinkClass::Wifi6Lan => LinkQosProfile {
            wlan: WlanPosture::streaming(),
            dscp: Dscp::ExpeditedForwarding,
            jitter_buffer_frames: 3,
            expected_throughput_mbps: 250,
            contended: true,
        },

        // Anything we could not classify is treated as the worst case we support:
        // contended, modest, buffered. Under-promising here is free; over-promising
        // produces exactly the "the stream looked fine and then it did not" report that
        // the gates exist to catch.
        LinkClass::Other(_) => LinkQosProfile {
            wlan: WlanPosture::streaming(),
            dscp: Dscp::AssuredForwarding {
                class: 3,
                drop_precedence: 0b10,
            },
            jitter_buffer_frames: 3,
            expected_throughput_mbps: 150,
            contended: true,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [LinkClass; 5] = [
        LinkClass::UsbNcm,
        LinkClass::Wifi7SoftAp,
        LinkClass::Wifi7Lan,
        LinkClass::Wifi6Lan,
        LinkClass::Other(String::new()),
    ];

    #[test]
    fn every_link_class_has_a_profile() {
        // Totality. A new variant in x-protocol must break this test, not produce a
        // silently-defaulted posture at runtime.
        for class in &ALL {
            let _ = profile_for(class);
        }
    }

    #[test]
    fn wired_holds_no_wlan_posture() {
        let wired = profile_for(&LinkClass::UsbNcm);
        assert_eq!(
            wired.wlan,
            WlanPosture::off(),
            "the wired path must not touch the radio"
        );
    }

    #[test]
    fn only_wired_is_uncontended() {
        for class in ALL.iter().filter(|c| **c != LinkClass::UsbNcm) {
            assert!(
                profile_for(class).contended,
                "{class:?} is a radio and must be treated as contended"
            );
        }
    }

    #[test]
    fn buffering_never_decreases_as_the_link_gets_worse() {
        // The one monotonicity worth asserting: latency preference order in x-protocol
        // is UsbNcm(0) < Wifi7SoftAp(1) < Wifi7Lan(2) < Wifi6Lan(3) < Other(4), and the
        // buffer depth must not go *down* as we move down that order.
        let ordered = [
            LinkClass::UsbNcm,
            LinkClass::Wifi7SoftAp,
            LinkClass::Wifi7Lan,
            LinkClass::Wifi6Lan,
        ];
        let mut previous = 0;
        for class in &ordered {
            let depth = profile_for(class).jitter_buffer_frames;
            assert!(
                depth >= previous,
                "{class:?} buffers {depth} frames after {previous} — the ladder went backwards"
            );
            previous = depth;
        }
        assert!(
            profile_for(&LinkClass::Other("x".into())).jitter_buffer_frames >= previous,
            "the unclassified catch-all must be at least as buffered as the worst known class"
        );
    }

    #[test]
    fn planned_throughput_does_not_exceed_the_physically_lower_class() {
        // Sanity, not physics: a Wi-Fi 6 infrastructure link must not be planned above
        // Wi-Fi 7 infrastructure, and no radio above the wire.
        let wired = profile_for(&LinkClass::UsbNcm).expected_throughput_mbps;
        let w7 = profile_for(&LinkClass::Wifi7Lan).expected_throughput_mbps;
        let w6 = profile_for(&LinkClass::Wifi6Lan).expected_throughput_mbps;
        assert!(w6 < w7, "wi-fi 6 planned above wi-fi 7");
        assert!(w7 < wired, "a radio planned above the wire");
    }

    #[test]
    fn dscp_values_match_the_ietf_classes() {
        // The byte is the 6-bit DSCP in the top bits and two zero bits below, so the
        // literal reads as "DSCP, then shift".
        assert_eq!(Dscp::BestEffort.ds_field(), 0b0000_0000);
        assert_eq!(Dscp::ClassSelector(5).ds_field(), 0b1010_0000); // CS5 = DSCP 40
        assert_eq!(Dscp::ExpeditedForwarding.ds_field(), 0b1011_1000); // EF  = DSCP 46
        assert_eq!(
            Dscp::AssuredForwarding {
                class: 3,
                drop_precedence: 2
            }
            .ds_field(),
            0b0111_0000, // AF32 = 3 * 8 + 2 * 2 = 28, shifted left by two
            "assured forwarding needs the drop precedence shifted into its own bit — \
             upstream ALVR omits that shift and every AF mark it emits is wrong"
        );
    }
}

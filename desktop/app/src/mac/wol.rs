// SPDX-License-Identifier: GPL-3.0-or-later
//! Reads this Mac's Ethernet and Wi-Fi adapters for the `pc.wake_info`
//! message. Adapters come from SystemConfiguration ([`super::sc`]), IPv4
//! addresses from `getifaddrs`, and the Wake for network access setting from
//! `pmset -g`. Nothing here changes any setting. Addresses, adapter names and
//! `pmset` output are never logged.
//!
//! `pmset` reports one global value per power source (`womp`, "wake on
//! ethernet magic packet"), not one per adapter, so only a wired adapter can
//! be `Enabled` or `Disabled`. A Wi-Fi adapter is always `Unknown`.

#![allow(unsafe_code)]

use std::{
    net::Ipv4Addr,
    process::{Command, Stdio},
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use nectarlink_core::{PcWakeInfo, format_mac, ipv4_broadcast, parse_mac};

/// The protocol allows at most this many addresses in one `pc.wake_info`
/// (`wake::MAX_ADAPTERS` in the protocol crate). `caps_the_adapter_count` checks
/// the limit in `filter_adapters`, and `build_info_is_valid` checks that
/// `build_info` stays valid even when given more adapters than this.
const MAX_ADAPTERS: usize = 8;

/// A `pmset` answer is reused for this long, so a window focus or a network
/// change does not start a process each time.
const PMSET_TTL: Duration = Duration::from_secs(60);
/// `pmset` gets this long before it is killed.
const PMSET_TIMEOUT: Duration = Duration::from_secs(2);

/// Whether the Mac is set to wake for network access.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeSupport {
    Enabled,
    Disabled,
    Unknown,
}

impl WakeSupport {
    pub fn as_str(self) -> &'static str {
        match self {
            WakeSupport::Enabled => "enabled",
            WakeSupport::Disabled => "disabled",
            WakeSupport::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WakeAdapter {
    pub label: String,
    pub wired: bool,
    pub mac: String,
    pub broadcasts: Vec<String>,
    pub support: WakeSupport,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WakeStatus {
    pub info: PcWakeInfo,
    pub adapters: Vec<WakeAdapter>,
}

impl WakeStatus {
    /// The preferred adapter: the first wired one, else the first adapter.
    pub fn primary(&self) -> Option<&WakeAdapter> {
        self.adapters.iter().find(|a| a.wired).or_else(|| self.adapters.first())
    }

    pub fn state_str(&self) -> &'static str {
        match self.primary() {
            Some(a) => a.support.as_str(),
            None => "none",
        }
    }
}

/// One adapter as read from the system, before any filtering.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RawAdapter {
    bsd_name: String,
    label: String,
    wired: bool,
    hardware_address: String,
    /// The interface is up and running (cable in, or joined to a network).
    up: bool,
    /// IPv4 address and prefix length of every address on the interface.
    ipv4: Vec<(Ipv4Addr, u8)>,
}

/// Whether a BSD interface name belongs to a virtual port: tunnels, VPNs,
/// bridges, VM networks, AirDrop and Apple's own internal links.
fn is_virtual_port_name(name: &str) -> bool {
    const PREFIXES: [&str; 17] = [
        "lo", "utun", "awdl", "llw", "bridge", "gif", "stf", "ap", "anpi", "p2p", "vmnet", "vmenet", "vnic",
        "vboxnet", "feth", "tap", "ppp",
    ];
    let lower = name.trim().to_ascii_lowercase();
    PREFIXES.iter().any(|p| lower.starts_with(p))
}

/// Reads a hardware address written with one or two hex digits per byte, in
/// either case, separated by `:` or `-`, and returns it as lowercase
/// `aa:bb:cc:dd:ee:ff`. Rejects what `parse_mac` rejects (zero, multicast).
fn normalize_mac(text: &str) -> Option<String> {
    let text = text.trim();
    let sep = if text.contains(':') { ':' } else { '-' };
    let mut bytes = [0u8; 6];
    let mut parts = text.split(sep);
    for slot in &mut bytes {
        let part = parts.next()?;
        if part.is_empty() || part.len() > 2 || !part.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        *slot = u8::from_str_radix(part, 16).ok()?;
    }
    if parts.next().is_some() {
        return None;
    }
    let mac = format_mac(&bytes);
    parse_mac(&mac).map(|b| format_mac(&b))
}

/// A locally administered address (second bit of the first byte). Macs use
/// these as private Wi-Fi addresses, and virtual ports use them too. A magic
/// packet for one is not trusted to wake the Mac, so these are left out.
fn is_locally_administered(mac: &str) -> bool {
    parse_mac(mac).is_some_and(|b| b[0] & 0x02 != 0)
}

/// The prefix length of a contiguous netmask, `None` for a mask with holes.
fn prefix_len(mask: u32) -> Option<u8> {
    let ones = mask.leading_ones();
    (mask == u32::MAX.checked_shl(32 - ones).unwrap_or(0)).then_some(ones as u8)
}

/// Reads `womp` from `pmset -g` output: the first line that starts with
/// `womp` and has a `0` or `1` as its value. `None` when it is missing or
/// anything else.
fn parse_pmset_womp(text: &str) -> Option<bool> {
    for line in text.lines() {
        let mut words = line.split_whitespace();
        if words.next() != Some("womp") {
            continue;
        }
        return match words.next() {
            Some("1") => Some(true),
            Some("0") => Some(false),
            _ => None,
        };
    }
    None
}

/// Filters, normalizes, de-duplicates and sorts the raw adapters. Wired comes
/// first, then adapters with an IPv4 broadcast, then by wake support.
fn filter_adapters(raw: Vec<RawAdapter>, womp: Option<bool>) -> Vec<WakeAdapter> {
    let mut adapters: Vec<WakeAdapter> = Vec::new();
    for r in raw {
        if !r.up || is_virtual_port_name(&r.bsd_name) {
            continue;
        }
        let Some(mac) = normalize_mac(&r.hardware_address) else { continue };
        if is_locally_administered(&mac) || adapters.iter().any(|a| a.mac == mac) {
            continue;
        }
        let mut broadcasts = Vec::new();
        for (ip, prefix) in &r.ipv4 {
            if let Some(b) = ipv4_broadcast(*ip, *prefix) {
                let b = b.to_string();
                if !broadcasts.contains(&b) {
                    broadcasts.push(b);
                }
            }
        }
        let support = match (r.wired, womp) {
            (true, Some(true)) => WakeSupport::Enabled,
            (true, Some(false)) => WakeSupport::Disabled,
            _ => WakeSupport::Unknown,
        };
        let label = if r.label.trim().is_empty() { r.bsd_name.clone() } else { r.label.trim().to_owned() };
        adapters.push(WakeAdapter { label, wired: r.wired, mac, broadcasts, support });
    }
    adapters.sort_by_key(|a| {
        (
            !a.wired,
            a.broadcasts.is_empty(),
            match a.support {
                WakeSupport::Enabled => 0u8,
                WakeSupport::Unknown => 1,
                WakeSupport::Disabled => 2,
            },
        )
    });
    adapters.truncate(MAX_ADAPTERS);
    adapters
}

/// The `pc.wake_info` body for these adapters.
fn build_info(adapters: &[WakeAdapter]) -> PcWakeInfo {
    let mut macs = Vec::new();
    let mut broadcasts = Vec::new();
    for a in adapters {
        if !macs.contains(&a.mac) {
            macs.push(a.mac.clone());
        }
        for b in &a.broadcasts {
            if !broadcasts.contains(b) {
                broadcasts.push(b.clone());
            }
        }
    }
    PcWakeInfo { macs, broadcasts }.sanitized()
}

/// Whether this Mac may advertise `pc.wake`: at least one valid address.
/// The capability is advisory only; core never checks it.
fn wake_cap_allowed(status: &WakeStatus) -> bool {
    status.info.is_valid() && !status.info.macs.is_empty()
}

/// Whether the last read that found an adapter allowed `pc.wake`. A read
/// that finds no adapter (Wi-Fi dropping, a network change in progress)
/// leaves it as it was, the same rule `refresh_wake` uses for the addresses.
static WAKE_CAP: AtomicBool = AtomicBool::new(false);

/// Whether `compute_capabilities` should advertise `pc.wake` now.
pub fn wake_cap_ready() -> bool {
    WAKE_CAP.load(Ordering::Relaxed)
}

/// A cached `pmset` answer, with the clock passed in so tests need no sleeping.
#[derive(Debug, Default)]
struct PmsetCache {
    read: Option<(Instant, Option<bool>)>,
}

impl PmsetCache {
    /// The cached answer when it is younger than `PMSET_TTL`.
    fn get(&self, now: Instant) -> Option<Option<bool>> {
        self.read.filter(|(at, _)| now.saturating_duration_since(*at) < PMSET_TTL).map(|(_, v)| v)
    }

    fn set(&mut self, now: Instant, value: Option<bool>) {
        self.read = Some((now, value));
    }
}

static PMSET: Mutex<PmsetCache> = Mutex::new(PmsetCache { read: None });

/// Runs `pmset -g` with a timeout and returns its output, `None` on any failure.
fn run_pmset() -> Option<String> {
    use std::io::Read;
    let mut child = Command::new("/usr/bin/pmset")
        .arg("-g")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    // The output is a few hundred bytes, so the pipe never fills before exit.
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                let mut text = String::new();
                stdout.read_to_string(&mut text).ok()?;
                return Some(text);
            }
            Ok(None) if started.elapsed() < PMSET_TIMEOUT => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// The global Wake for network access value, cached for `PMSET_TTL`.
fn womp() -> Option<bool> {
    let now = Instant::now();
    if let Some(v) = PMSET.lock().unwrap_or_else(|e| e.into_inner()).get(now) {
        return v;
    }
    let value = run_pmset().and_then(|t| parse_pmset_womp(&t));
    PMSET.lock().unwrap_or_else(|e| e.into_inner()).set(now, value);
    value
}

/// Up state and IPv4 addresses of one interface, as `getifaddrs` lists it.
struct IfState {
    name: String,
    up: bool,
    ipv4: Vec<(Ipv4Addr, u8)>,
}

/// Up state and IPv4 addresses of every interface, by BSD name.
fn read_ifaddrs() -> Vec<IfState> {
    let mut out: Vec<IfState> = Vec::new();
    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: `head` is a valid out pointer.
    if unsafe { libc::getifaddrs(&raw mut head) } != 0 {
        return out;
    }
    let up_mask = (libc::IFF_UP | libc::IFF_RUNNING) as u32;
    let mut cur = head;
    // SAFETY: the list returned by `getifaddrs` is walked through its `ifa_next`
    // pointers while it is alive, and freed once with `freeifaddrs` below.
    // Names are NUL-terminated; an address is read as `sockaddr_in` only when
    // its family is AF_INET.
    unsafe {
        while let Some(ifa) = cur.as_ref() {
            cur = ifa.ifa_next;
            if ifa.ifa_name.is_null() {
                continue;
            }
            let name = std::ffi::CStr::from_ptr(ifa.ifa_name).to_string_lossy().into_owned();
            let idx = match out.iter().position(|i| i.name == name) {
                Some(i) => i,
                None => {
                    out.push(IfState { name, up: false, ipv4: Vec::new() });
                    out.len() - 1
                }
            };
            if ifa.ifa_flags & up_mask == up_mask {
                out[idx].up = true;
            }
            if ifa.ifa_addr.is_null() || ifa.ifa_netmask.is_null() {
                continue;
            }
            if i32::from((*ifa.ifa_addr).sa_family) != libc::AF_INET {
                continue;
            }
            let ip =
                Ipv4Addr::from(u32::from_be((*ifa.ifa_addr.cast::<libc::sockaddr_in>()).sin_addr.s_addr));
            let mask = u32::from_be((*ifa.ifa_netmask.cast::<libc::sockaddr_in>()).sin_addr.s_addr);
            if let Some(prefix) = prefix_len(mask) {
                out[idx].ipv4.push((ip, prefix));
            }
        }
        libc::freeifaddrs(head);
    }
    out
}

fn read_raw_adapters() -> Vec<RawAdapter> {
    let state = read_ifaddrs();
    super::sc::network_interfaces()
        .into_iter()
        .map(|i| {
            let found = state.iter().find(|s| s.name == i.bsd_name);
            RawAdapter {
                up: found.is_some_and(|s| s.up),
                ipv4: found.map(|s| s.ipv4.clone()).unwrap_or_default(),
                bsd_name: i.bsd_name,
                label: i.label,
                wired: i.wired,
                hardware_address: i.hardware_address,
            }
        })
        .collect()
}

/// Reads the Ethernet and Wi-Fi adapters that are up and this Mac's wake
/// setting. Needs no main thread and no run loop, so it can run before
/// `Node::start`.
pub fn query_wake_status() -> WakeStatus {
    let raw = read_raw_adapters();
    // Skip the `pmset` read when there is no wired adapter to apply it to.
    let womp = if raw.iter().any(|r| r.wired && r.up) { womp() } else { None };
    let adapters = filter_adapters(raw, womp);
    let info = build_info(&adapters);
    let status = WakeStatus { info, adapters };
    if !status.adapters.is_empty() {
        WAKE_CAP.store(wake_cap_allowed(&status), Ordering::Relaxed);
    }
    status
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(bsd: &str, wired: bool, mac: &str) -> RawAdapter {
        RawAdapter {
            bsd_name: bsd.into(),
            label: format!("label {bsd}"),
            wired,
            hardware_address: mac.into(),
            up: true,
            ipv4: vec![(Ipv4Addr::new(192, 168, 1, 20), 24)],
        }
    }

    #[test]
    fn parses_womp() {
        let out = "System-wide power settings:\nCurrently in use:\n standby         1\n womp            1\n powernap        0\n";
        assert_eq!(parse_pmset_womp(out), Some(true));
        assert_eq!(parse_pmset_womp(" womp\t0\n"), Some(false));
        assert_eq!(parse_pmset_womp("womp 1 (extra)"), Some(true));
        // Missing, empty, odd or translated output is unknown.
        assert_eq!(parse_pmset_womp(""), None);
        assert_eq!(parse_pmset_womp(" standby 1\n"), None);
        assert_eq!(parse_pmset_womp(" womp\n"), None);
        assert_eq!(parse_pmset_womp(" womp  yes\n"), None);
        assert_eq!(parse_pmset_womp(" nowomp 1\n"), None);
        // The first womp line wins.
        assert_eq!(parse_pmset_womp(" womp 0\n womp 1\n"), Some(false));
    }

    #[test]
    fn virtual_port_names() {
        for name in [
            "lo0", "utun3", "awdl0", "llw0", "bridge0", "gif0", "stf0", "ap1", "anpi0", "p2p0", "vmnet8",
            "vmenet0", "vnic0", "vboxnet0", "feth0", "tap0", "ppp0", "UTUN1",
        ] {
            assert!(is_virtual_port_name(name), "{name}");
        }
        for name in ["en0", "en1", "en12"] {
            assert!(!is_virtual_port_name(name), "{name}");
        }
    }

    #[test]
    fn lenient_mac_parse() {
        assert_eq!(normalize_mac("a4:83:e7:1:2:3").as_deref(), Some("a4:83:e7:01:02:03"));
        assert_eq!(normalize_mac("A4:83:E7:0A:0B:0C").as_deref(), Some("a4:83:e7:0a:0b:0c"));
        assert_eq!(normalize_mac("a4-83-e7-0a-0b-0c").as_deref(), Some("a4:83:e7:0a:0b:0c"));
        assert_eq!(normalize_mac("  a4:83:e7:0a:0b:0c \n").as_deref(), Some("a4:83:e7:0a:0b:0c"));
        for bad in [
            "",
            "a4:83:e7:0a:0b",
            "a4:83:e7:0a:0b:0c:0d",
            "a4:83:e7:0a:0b:zz",
            "a4:83:e7::0b:0c",
            "a4:83:e7:100:0b:0c",
            "00:00:00:00:00:00",
            "0:0:0:0:0:0",
            "01:00:5e:00:00:01",
        ] {
            assert_eq!(normalize_mac(bad), None, "{bad}");
        }
    }

    #[test]
    fn prefix_from_mask() {
        assert_eq!(prefix_len(0xffff_ff00), Some(24));
        assert_eq!(prefix_len(0xffff_ffff), Some(32));
        assert_eq!(prefix_len(0xff00_0000), Some(8));
        assert_eq!(prefix_len(0), Some(0));
        assert_eq!(prefix_len(0xff00_ff00), None);
        assert_eq!(prefix_len(0x0000_00ff), None);
    }

    #[test]
    fn filters_adapters() {
        let mut down = raw("en3", true, "a4:83:e7:00:00:03");
        down.up = false;
        let adapters = filter_adapters(
            vec![
                raw("en0", false, "a4:83:e7:00:00:01"),
                raw("en1", true, "a4:83:e7:00:00:02"),
                // The same address again, zero, multicast, locally administered.
                raw("en2", true, "A4:83:E7:0:0:2"),
                raw("en4", true, "00:00:00:00:00:00"),
                raw("en5", true, "01:00:5e:00:00:01"),
                raw("en6", true, "a6:83:e7:00:00:06"),
                // Down, virtual by name, no address.
                down,
                raw("utun2", true, "a4:83:e7:00:00:07"),
                raw("en7", true, ""),
            ],
            Some(true),
        );
        let macs: Vec<_> = adapters.iter().map(|a| a.mac.as_str()).collect();
        assert_eq!(macs, ["a4:83:e7:00:00:02", "a4:83:e7:00:00:01"]);
        assert_eq!(adapters[0].broadcasts, ["192.168.1.255"]);
    }

    #[test]
    fn caps_the_adapter_count() {
        let many: Vec<_> = (0..MAX_ADAPTERS + 4)
            .map(|i| raw(&format!("en{i}"), true, &format!("a4:83:e7:00:00:{:02x}", i + 1)))
            .collect();
        assert_eq!(filter_adapters(many, None).len(), MAX_ADAPTERS);
    }

    #[test]
    fn wired_adapters_sort_first_and_get_the_wake_setting() {
        let adapters = filter_adapters(
            vec![raw("en0", false, "a4:83:e7:00:00:01"), raw("en5", true, "a4:83:e7:00:00:05")],
            Some(false),
        );
        assert!(adapters[0].wired);
        assert_eq!(adapters[0].support, WakeSupport::Disabled);
        assert_eq!(adapters[1].support, WakeSupport::Unknown, "Wi-Fi is never enabled or disabled");
        let status = WakeStatus { info: build_info(&adapters), adapters };
        assert_eq!(status.state_str(), "disabled");
        // An unknown parse stays unknown for a wired adapter.
        let adapters = filter_adapters(vec![raw("en5", true, "a4:83:e7:00:00:05")], None);
        assert_eq!(adapters[0].support, WakeSupport::Unknown);
        let adapters = filter_adapters(vec![raw("en5", true, "a4:83:e7:00:00:05")], Some(true));
        assert_eq!(adapters[0].support, WakeSupport::Enabled);
    }

    #[test]
    fn primary_prefers_a_wired_adapter_even_when_unsorted() {
        let wifi = WakeAdapter {
            label: "Wi-Fi".into(),
            wired: false,
            mac: "a4:83:e7:00:00:01".into(),
            broadcasts: vec![],
            support: WakeSupport::Unknown,
        };
        let wired = WakeAdapter { label: "Ethernet".into(), wired: true, ..wifi.clone() };
        let status = WakeStatus { info: PcWakeInfo::default(), adapters: vec![wifi.clone(), wired] };
        assert_eq!(status.primary().map(|a| a.label.as_str()), Some("Ethernet"));
        let status = WakeStatus { info: PcWakeInfo::default(), adapters: vec![wifi] };
        assert_eq!(status.primary().map(|a| a.label.as_str()), Some("Wi-Fi"));
        assert_eq!(WakeStatus::default().state_str(), "none");
    }

    #[test]
    fn build_info_is_valid() {
        // Hand-built, so `build_info` sees more adapters than the limit.
        let many: Vec<_> = (0..MAX_ADAPTERS + 4)
            .map(|i| WakeAdapter {
                label: format!("en{i}"),
                wired: true,
                mac: format!("a4:83:e7:00:00:{:02x}", i + 1),
                broadcasts: vec!["192.168.1.255".into()],
                support: WakeSupport::Unknown,
            })
            .collect();
        let info = build_info(&many);
        assert!(info.is_valid());
        assert_eq!(info.macs.len(), MAX_ADAPTERS);
        assert_eq!(info.broadcasts, ["192.168.1.255"]);
        assert!(build_info(&[]).is_valid());
    }

    #[test]
    fn wake_cap_needs_a_valid_address() {
        let adapters = filter_adapters(vec![raw("en0", true, "a4:83:e7:00:00:01")], None);
        let with = WakeStatus { info: build_info(&adapters), adapters };
        assert!(wake_cap_allowed(&with));
        assert!(!wake_cap_allowed(&WakeStatus::default()));
        // Addresses that are not valid, or an empty list, never allow it.
        let bad = WakeStatus {
            info: PcWakeInfo { macs: vec!["nonsense".into()], broadcasts: vec![] },
            adapters: vec![],
        };
        assert!(!wake_cap_allowed(&bad));
    }

    #[test]
    fn pmset_cache_expires() {
        let start = Instant::now();
        let mut cache = PmsetCache::default();
        assert_eq!(cache.get(start), None);
        cache.set(start, Some(true));
        assert_eq!(cache.get(start + Duration::from_secs(30)), Some(Some(true)));
        assert_eq!(cache.get(start + PMSET_TTL), None);
        // A failed read is cached too.
        cache.set(start, None);
        assert_eq!(cache.get(start), Some(None));
    }
}

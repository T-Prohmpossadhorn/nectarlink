// SPDX-License-Identifier: GPL-3.0-or-later
//! What the Connection Doctor reads on a Mac: the application firewall, the
//! name of the network service, and whether a VPN is up. Nothing here changes
//! a setting. Tool output, paths and network names are never logged.
//!
//! The firewall comes from `/usr/libexec/ApplicationFirewall/socketfilterfw`.
//! That tool is not in the SDK and Apple does not document it, so every parse
//! is strict: output that does not match a known shape is an `Err`, and the
//! Doctor then shows no firewall row. The Application Firewall also lets
//! signed apps in without a list entry, so an app that is not listed is
//! unknown, not blocked. Only block-all or an explicit Block entry is a
//! failure. The macOS firewall is global, so there is no "for this network".

use std::{
    io::Read,
    net::{Ipv4Addr, Ipv6Addr},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

/// The firewall tool, by absolute path so nothing on `PATH` can stand in.
const FIREWALL_TOOL: &str = "/usr/libexec/ApplicationFirewall/socketfilterfw";
/// `open`, by absolute path.
const OPEN_TOOL: &str = "/usr/bin/open";
/// How long one firewall tool call may take.
const TOOL_TIMEOUT: Duration = Duration::from_secs(3);
/// The most output read from one call. More than this is an error.
const MAX_OUTPUT: u64 = 1 << 20;
/// System Settings pane for Network on macOS 13 and later. It is not proven
/// that this URL lands on the firewall, and a failed open falls back to System
/// Settings itself.
const FIREWALL_PANE: &str = "x-apple.systempreferences:com.apple.Network-Settings.extension";
/// Bundle id of System Settings.
const SETTINGS_BUNDLE: &str = "com.apple.systempreferences";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Firewall {
    pub on: bool,
    pub allowed: bool,
    pub blocked: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Network {
    pub name: String,
    pub trusted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Adapters {
    pub local_address: bool,
    pub vpn: bool,
}

/// One entry of `socketfilterfw --listapps`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ListedApp {
    path: String,
    allow: bool,
}

/// Runs `program` with `args` and returns its standard output. Standard
/// input and error are closed. Any failure, a non-zero exit, a timeout or
/// output over [`MAX_OUTPUT`] is an `Err` that says nothing about the output.
fn run_program(program: &str, args: &[&str], timeout: Duration) -> Result<String, String> {
    let mut child = Command::new(program)
        .args(args)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| "can't start the tool".to_owned())?;
    let deadline = Instant::now() + timeout;
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err("no output pipe".into());
    };
    // `std::process` has no timeout, so a thread reads and the caller waits
    // on a channel. The thread ends when the pipe closes, which a kill does.
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let read = stdout.take(MAX_OUTPUT + 1).read_to_end(&mut bytes);
        let _ = tx.send(read.ok().map(|_| bytes));
    });
    let bytes = match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(Some(bytes)) if bytes.len() as u64 <= MAX_OUTPUT => bytes,
        _ => {
            let _ = child.kill();
            let _ = child.wait();
            return Err("the tool timed out or gave bad output".into());
        }
    };
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(Some(_)) => return Err("the tool failed".into()),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("the tool did not finish".into());
            }
        }
    }
    String::from_utf8(bytes).map_err(|_| "the tool gave text that is not UTF-8".to_owned())
}

/// The global state from `--getglobalstate`, such as
/// `Firewall is enabled. (State = 1)`. 0 is off, 1 and 2 are on. Anything
/// else is an error. What 2 means is a hardware check, so it is not read as
/// block-all here; `--getblockall` says that.
fn parse_global_state(text: &str) -> Result<bool, String> {
    let rest = text.split_once("State = ").ok_or("no firewall state")?.1;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    match digits.parse::<u32>() {
        Ok(0) => Ok(false),
        Ok(1 | 2) => Ok(true),
        _ => Err("unknown firewall state".into()),
    }
}

/// The block-all setting from `--getblockall`. The tool says
/// `Block all DISABLED!` or `Firewall has block all state set to disabled.`
/// when it is off, and `Firewall is set to block all non-essential incoming
/// connections` (or an "enabled" form) when it is on. "Disabled" is checked
/// first because the off text also contains "block all".
fn parse_block_all(text: &str) -> Result<bool, String> {
    let lower = text.to_ascii_lowercase();
    if !lower.contains("block all") {
        return Err("unknown block all text".into());
    }
    if lower.contains("disabled") {
        Ok(false)
    } else if lower.contains("enabled") || lower.contains("block all non-essential") {
        Ok(true)
    } else {
        Err("unknown block all text".into())
    }
}

/// The entries of `--listapps`: a `Total number of apps = N` line, then for
/// each app a `N : <path>` line and a line with `(Allow incoming connections)`
/// or `(Block incoming connections)`. A path may hold spaces. The entry count
/// must match the total, or the text is not trusted.
fn parse_listapps(text: &str) -> Result<Vec<ListedApp>, String> {
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
    let total: usize = lines
        .next()
        .and_then(|l| l.strip_prefix("Total number of apps = "))
        .and_then(|n| n.trim().parse().ok())
        .ok_or("no app total")?;
    let mut apps = Vec::new();
    while let Some(line) = lines.next() {
        let (number, path) = line.split_once(" : ").ok_or("unknown app line")?;
        if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) || path.trim().is_empty() {
            return Err("unknown app line".into());
        }
        let verdict = lines.next().ok_or("an app has no verdict")?;
        let allow = match verdict.trim_matches(|c: char| c == '(' || c == ')' || c.is_whitespace()) {
            "Allow incoming connections" => true,
            "Block incoming connections" => false,
            _ => return Err("unknown app verdict".into()),
        };
        apps.push(ListedApp { path: path.trim().to_owned(), allow });
    }
    if apps.len() == total { Ok(apps) } else { Err("the app list is cut short".into()) }
}

/// The `.app` folder that holds `exe`, only for the exact shape
/// `<Name>.app/Contents/MacOS/<file>`. `None` for a bare binary, such as a
/// development build, so the Doctor shows nothing rather than guessing.
fn app_bundle_for(exe: &Path) -> Option<PathBuf> {
    let macos = exe.parent()?;
    let contents = macos.parent()?;
    let bundle = contents.parent()?;
    let shaped = macos.file_name()? == "MacOS"
        && contents.file_name()? == "Contents"
        && bundle.extension().is_some_and(|e| e.eq_ignore_ascii_case("app"));
    shaped.then(|| bundle.to_path_buf())
}

/// The firewall verdict for the app. `targets` are the exact paths that name
/// it: its `.app` folder and its own executable, because a listing may be
/// keyed by either. Off is `on: false`.
/// Block-all and an explicit Block entry are `blocked`. An explicit Allow
/// entry is `allowed`. An app that is not listed is `None` (unknown), because
/// signed apps are let in without an entry. This never returns `on` with
/// neither `allowed` nor `blocked`.
fn firewall_from(on: bool, block_all: bool, apps: &[ListedApp], targets: &[&Path]) -> Option<Firewall> {
    if !on {
        return Some(Firewall::default());
    }
    if block_all {
        return Some(Firewall { on: true, allowed: false, blocked: true });
    }
    let mut mine = apps.iter().filter(|a| targets.contains(&Path::new(&a.path)));
    let first = mine.next()?;
    if !first.allow || mine.any(|a| !a.allow) {
        Some(Firewall { on: true, allowed: false, blocked: true })
    } else {
        Some(Firewall { on: true, allowed: true, blocked: false })
    }
}

/// Reads the firewall with `run` standing in for the tool call.
fn firewall_with(exe: &Path, run: impl Fn(&[&str]) -> Result<String, String>) -> Result<Firewall, String> {
    if !parse_global_state(&run(&["--getglobalstate"])?)? {
        return Ok(Firewall::default());
    }
    let block_all = parse_block_all(&run(&["--getblockall"])?)?;
    let apps = if block_all { Vec::new() } else { parse_listapps(&run(&["--listapps"])?)? };
    let bundle = app_bundle_for(exe);
    let targets: Vec<&Path> = bundle.iter().map(PathBuf::as_path).chain([exe]).collect();
    firewall_from(true, block_all, &apps, &targets).ok_or_else(|| "NectarLink is not listed".into())
}

/// The application firewall's view of this app. `Err` means unknown.
pub fn firewall(exe: &Path) -> Result<Firewall, String> {
    let real = std::fs::canonicalize(exe).unwrap_or_else(|_| exe.to_path_buf());
    firewall_with(&real, |args| run_program(FIREWALL_TOOL, args, TOOL_TIMEOUT))
}

/// The network the Mac is on, by the service name in System Settings (for
/// example `Wi-Fi`). The Wi-Fi name is never read, because that needs the
/// Location permission. A Mac has no public or private profile, so the
/// network is always trusted and the Doctor's profile check stays quiet.
pub fn networks() -> Result<Vec<Network>, String> {
    Ok(networks_from(super::sc::primary_network()))
}

fn networks_from(primary: Option<super::sc::Primary>) -> Vec<Network> {
    match primary.and_then(|p| p.service_name) {
        Some(name) => vec![Network { name, trusted: true }],
        None => Vec::new(),
    }
}

/// Up state and addresses of one interface, as `getifaddrs` lists it.
#[derive(Debug, Clone, Default)]
struct IfAddrs {
    name: String,
    up: bool,
    ipv4: Vec<Ipv4Addr>,
    ipv6: Vec<Ipv6Addr>,
}

/// Every interface with its addresses, IPv4 and IPv6, by BSD name.
#[allow(unsafe_code)]
fn read_interfaces() -> Vec<IfAddrs> {
    let mut out: Vec<IfAddrs> = Vec::new();
    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: `head` is a valid out pointer.
    if unsafe { libc::getifaddrs(&raw mut head) } != 0 {
        return out;
    }
    let up_mask = (libc::IFF_UP | libc::IFF_RUNNING) as u32;
    let mut cur = head;
    // SAFETY: the list returned by `getifaddrs` is walked through `ifa_next`
    // while it is alive and freed once with `freeifaddrs` below. Names are
    // NUL-terminated. An address is read as `sockaddr_in` only for AF_INET and
    // as `sockaddr_in6` only for AF_INET6.
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
                    out.push(IfAddrs { name, ..IfAddrs::default() });
                    out.len() - 1
                }
            };
            if ifa.ifa_flags & up_mask == up_mask {
                out[idx].up = true;
            }
            if ifa.ifa_addr.is_null() {
                continue;
            }
            match i32::from((*ifa.ifa_addr).sa_family) {
                libc::AF_INET => {
                    let raw = (*ifa.ifa_addr.cast::<libc::sockaddr_in>()).sin_addr.s_addr;
                    out[idx].ipv4.push(Ipv4Addr::from(u32::from_be(raw)));
                }
                libc::AF_INET6 => {
                    let raw = (*ifa.ifa_addr.cast::<libc::sockaddr_in6>()).sin6_addr.s6_addr;
                    out[idx].ipv6.push(Ipv6Addr::from(raw));
                }
                _ => {}
            }
        }
        libc::freeifaddrs(head);
    }
    out
}

/// Interface names that can be a VPN tunnel. A Mac has several `utun`
/// interfaces with only a link-local address even with no VPN, so the name
/// alone proves nothing.
fn tunnel_name(name: &str) -> bool {
    ["utun", "ipsec", "ppp"].iter().any(|p| name.starts_with(p))
}

fn usable_ipv4(ip: &Ipv4Addr) -> bool {
    !ip.is_loopback() && !ip.is_link_local() && !ip.is_unspecified()
}

fn usable_ipv6(ip: &Ipv6Addr) -> bool {
    !ip.is_loopback() && !ip.is_unicast_link_local() && !ip.is_unspecified() && !ip.is_multicast()
}

/// Reads whether a network address exists and whether a VPN is up. A tunnel
/// interface counts as a VPN only when it is up and has an IPv4 address, an
/// IPv6 address that is not link-local, or carries the default route
/// (`primary`). Its addresses never count as a local address. This guess is
/// unproven on hardware.
fn classify_adapters(interfaces: &[IfAddrs], primary: Option<&str>) -> Adapters {
    let mut found = Adapters::default();
    for i in interfaces.iter().filter(|i| i.up && !i.name.starts_with("lo")) {
        let routable = i.ipv4.iter().any(usable_ipv4) || i.ipv6.iter().any(usable_ipv6);
        if tunnel_name(&i.name) {
            if routable || primary == Some(i.name.as_str()) {
                found.vpn = true;
            }
        } else if i.ipv4.iter().any(usable_ipv4) {
            found.local_address = true;
        }
    }
    found
}

/// Whether the Mac has a network address and whether a VPN is up.
pub fn adapters() -> Adapters {
    let primary = super::sc::primary_network().and_then(|p| p.interface);
    classify_adapters(&read_interfaces(), primary.as_deref())
}

/// The macOS firewall's Allow needs administrator rights, and the path that
/// grants it is not proven, so nothing is changed from here. The Doctor shows
/// only its Firewall settings button.
pub fn fix_firewall(_exe: &Path) -> Result<bool, String> {
    Ok(false)
}

/// Opens the firewall's System Settings pane with `open`. If that fails, it
/// opens System Settings itself. Returns whether one of them worked.
fn open_firewall_settings_with(mut open: impl FnMut(&[&str]) -> bool) -> bool {
    open(&[FIREWALL_PANE]) || open(&["-b", SETTINGS_BUNDLE])
}

pub fn open_firewall_settings() -> bool {
    open_firewall_settings_with(|args| {
        Command::new(OPEN_TOOL)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    })
}

/// Readiness checks for this platform: `(id, title, ok, detail, fix)`.
/// None yet on macOS.
pub fn platform_checks() -> Vec<(String, String, bool, String, Option<&'static str>)> {
    Vec::new()
}

#[allow(dead_code)]
pub fn diagnose() -> Vec<(String, bool, String)> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    const APP: &str = "/Applications/NectarLink.app";
    const EXE: &str = "/Applications/NectarLink.app/Contents/MacOS/nectarlink-desktop";

    fn app(path: &str, allow: bool) -> ListedApp {
        ListedApp { path: path.into(), allow }
    }

    fn iface(name: &str, ipv4: &[&str], ipv6: &[&str]) -> IfAddrs {
        IfAddrs {
            name: name.into(),
            up: true,
            ipv4: ipv4.iter().map(|a| a.parse().unwrap()).collect(),
            ipv6: ipv6.iter().map(|a| a.parse().unwrap()).collect(),
        }
    }

    #[test]
    fn global_state_reads_the_number() {
        assert_eq!(parse_global_state("Firewall is disabled. (State = 0)\n"), Ok(false));
        assert_eq!(parse_global_state("Firewall is enabled. (State = 1) \n"), Ok(true));
        assert_eq!(parse_global_state("Firewall is enabled. (State = 2)"), Ok(true));
        assert!(parse_global_state("Firewall is enabled. (State = 7)").is_err());
        assert!(parse_global_state("Firewall is enabled. (State = )").is_err());
        assert!(parse_global_state("").is_err());
        assert!(parse_global_state("usage: socketfilterfw").is_err());
    }

    #[test]
    fn block_all_reads_both_forms() {
        assert_eq!(parse_block_all("Block all DISABLED! \n"), Ok(false));
        assert_eq!(parse_block_all("Firewall has block all state set to disabled.\n"), Ok(false));
        assert_eq!(parse_block_all("Block all ENABLED!\n"), Ok(true));
        assert_eq!(parse_block_all("Firewall has block all state set to enabled.\n"), Ok(true));
        assert_eq!(
            parse_block_all("Firewall is set to block all non-essential incoming connections \n"),
            Ok(true)
        );
        assert!(parse_block_all("").is_err());
        assert!(parse_block_all("Firewall is enabled. (State = 1)").is_err());
        assert!(parse_block_all("Block all").is_err());
    }

    #[test]
    fn listapps_reads_paths_with_spaces_and_both_verdict_forms() {
        let text = "Total number of apps = 3 \n\n\
                    1 : /Applications/Visual Studio Code.app\n\
                    \x20            (Allow incoming connections)\n\
                    \n\
                    2 :  /Applications/NectarLink.app\n\
                    \x20            ( Block incoming connections )\n\
                    3 : /usr/libexec/some tool\n\
                    \x20            (Allow incoming connections)\n";
        assert_eq!(
            parse_listapps(text).unwrap(),
            [
                app("/Applications/Visual Studio Code.app", true),
                app("/Applications/NectarLink.app", false),
                app("/usr/libexec/some tool", true),
            ]
        );
        assert_eq!(parse_listapps("Total number of apps = 0 \n").unwrap(), []);
    }

    #[test]
    fn listapps_rejects_text_it_does_not_know() {
        for text in [
            "",
            "1 : /Applications/A.app\n (Allow incoming connections)\n",
            "Total number of apps = 2\n1 : /A.app\n (Allow incoming connections)\n",
            "Total number of apps = 1\n1 : /A.app\n",
            "Total number of apps = 1\n1 : /A.app\n (Maybe)\n",
            "Total number of apps = 1\nnot an entry\n (Allow incoming connections)\n",
            "Total number of apps = x\n",
        ] {
            assert!(parse_listapps(text).is_err(), "{text:?}");
        }
    }

    #[test]
    fn app_bundle_needs_the_exact_shape() {
        assert_eq!(app_bundle_for(Path::new(EXE)), Some(PathBuf::from(APP)));
        assert_eq!(
            app_bundle_for(Path::new("/Users/me/My Apps/Nectar Link.app/Contents/MacOS/x")),
            Some(PathBuf::from("/Users/me/My Apps/Nectar Link.app"))
        );
        for bad in [
            "/Users/me/nectarlink/target/debug/nectarlink-desktop",
            "/Applications/NectarLink.app/Contents/Resources/x",
            "/Applications/NectarLink.app/Contents/MacOS/sub/x",
            "/Applications/NectarLink/Contents/MacOS/x",
            "/x",
            "",
        ] {
            assert_eq!(app_bundle_for(Path::new(bad)), None, "{bad}");
        }
    }

    #[test]
    fn firewall_from_covers_every_case() {
        let bundle = Path::new(APP);
        let off = Some(Firewall { on: false, allowed: false, blocked: false });
        assert_eq!(firewall_from(false, true, &[app(APP, false)], &[bundle]), off);
        let blocked = Some(Firewall { on: true, allowed: false, blocked: true });
        let allowed = Some(Firewall { on: true, allowed: true, blocked: false });
        assert_eq!(firewall_from(true, false, &[app(APP, true)], &[bundle]), allowed);
        assert_eq!(firewall_from(true, false, &[app(APP, false)], &[bundle]), blocked);
        // Block-all wins over an Allow entry and over no entry.
        assert_eq!(firewall_from(true, true, &[app(APP, true)], &[bundle]), blocked);
        assert_eq!(firewall_from(true, true, &[], &[]), blocked);
        // A Block entry wins over an Allow entry for the same app.
        assert_eq!(firewall_from(true, false, &[app(APP, true), app(APP, false)], &[bundle]), blocked);
        // Not listed, listed only under another path, or no bundle: unknown.
        assert_eq!(firewall_from(true, false, &[], &[bundle]), None);
        assert_eq!(
            firewall_from(true, false, &[app("/Applications/NectarLink.app2", false)], &[bundle]),
            None
        );
        assert_eq!(firewall_from(true, false, &[app(APP, false)], &[]), None);
    }

    #[test]
    fn firewall_from_never_says_on_without_allow_or_block() {
        let apps = [app(APP, true), app(APP, false), app("/other.app", true)];
        for on in [false, true] {
            for block_all in [false, true] {
                for list in [&apps[..0], &apps[..1], &apps[1..2], &apps[..]] {
                    for targets in [&[][..], &[Path::new(APP)][..]] {
                        if let Some(f) = firewall_from(on, block_all, list, targets) {
                            assert!(!f.on || f.allowed || f.blocked, "{f:?}");
                        }
                    }
                }
            }
        }
    }

    /// A fake tool that answers each argument from a table.
    fn fake(state: &str, block_all: &str, list: &str) -> impl Fn(&[&str]) -> Result<String, String> {
        let (state, block_all, list) = (state.to_owned(), block_all.to_owned(), list.to_owned());
        move |args| match args {
            ["--getglobalstate"] => Ok(state.clone()),
            ["--getblockall"] => Ok(block_all.clone()),
            ["--listapps"] => Ok(list.clone()),
            _ => Err("unexpected".into()),
        }
    }

    const ALLOWED_LIST: &str =
        "Total number of apps = 1\n1 : /Applications/NectarLink.app\n (Allow incoming connections)\n";
    const BLOCKED_LIST: &str =
        "Total number of apps = 1\n1 : /Applications/NectarLink.app\n (Block incoming connections)\n";

    #[test]
    fn firewall_with_reads_the_three_calls() {
        let exe = Path::new(EXE);
        let off = fake("Firewall is disabled. (State = 0)", "garbage", "garbage");
        assert_eq!(firewall_with(exe, off), Ok(Firewall::default()));
        let ok = fake("Firewall is enabled. (State = 1)", "Block all DISABLED!", ALLOWED_LIST);
        assert_eq!(firewall_with(exe, ok), Ok(Firewall { on: true, allowed: true, blocked: false }));
        let blocked = fake("Firewall is enabled. (State = 1)", "Block all DISABLED!", BLOCKED_LIST);
        assert_eq!(firewall_with(exe, blocked), Ok(Firewall { on: true, allowed: false, blocked: true }));
        // Block-all needs no list, so a broken list does not matter.
        let all = fake("Firewall is enabled. (State = 1)", "Block all ENABLED!", "garbage");
        assert_eq!(firewall_with(exe, all), Ok(Firewall { on: true, allowed: false, blocked: true }));
    }

    #[test]
    fn firewall_with_stays_quiet_when_unsure() {
        let exe = Path::new(EXE);
        let on = "Firewall is enabled. (State = 1)";
        let empty_list = "Total number of apps = 0 \n";
        // On and not listed, a broken list, a failing tool and a development
        // build all give an error, which leaves out the firewall row.
        assert!(firewall_with(exe, fake(on, "Block all DISABLED!", empty_list)).is_err());
        assert!(firewall_with(exe, fake(on, "Block all DISABLED!", "garbage")).is_err());
        assert!(firewall_with(exe, fake("", "", "")).is_err());
        assert!(firewall_with(exe, |_| Err("failed".into())).is_err());
        let dev = Path::new("/Users/me/target/debug/nectarlink-desktop");
        assert!(firewall_with(dev, fake(on, "Block all DISABLED!", ALLOWED_LIST)).is_err());
        // A development build still reports block-all.
        assert_eq!(
            firewall_with(dev, fake(on, "Block all ENABLED!", "")),
            Ok(Firewall { on: true, allowed: false, blocked: true })
        );
    }

    #[test]
    fn firewall_with_matches_an_entry_keyed_by_the_executable() {
        // The tool on this build lists binaries by path, so the exact
        // executable path counts as well as the .app folder.
        let on = "Firewall is enabled. (State = 1)";
        let list = |verdict: &str| {
            format!("Total number of apps = 1\n1 : {EXE}\n ({verdict} incoming connections)\n")
        };
        let (allow, block) = (list("Allow"), list("Block"));
        let exe = Path::new(EXE);
        assert_eq!(
            firewall_with(exe, fake(on, "Block all DISABLED!", &allow)),
            Ok(Firewall { on: true, allowed: true, blocked: false })
        );
        assert_eq!(
            firewall_with(exe, fake(on, "Block all DISABLED!", &block)),
            Ok(Firewall { on: true, allowed: false, blocked: true })
        );
        // A bare development binary that is listed by its own path works too.
        let dev = "/Users/me/target/debug/nectarlink-desktop";
        let text = format!("Total number of apps = 1\n1 : {dev}\n (Block incoming connections)\n");
        assert_eq!(
            firewall_with(Path::new(dev), fake(on, "Block all DISABLED!", &text)),
            Ok(Firewall { on: true, allowed: false, blocked: true })
        );
    }

    #[test]
    fn firewall_has_a_default_that_is_all_off() {
        assert_eq!(Firewall::default(), Firewall { on: false, allowed: false, blocked: false });
        assert_eq!(Adapters::default(), Adapters { local_address: false, vpn: false });
    }

    #[test]
    fn a_quiet_mac_has_a_local_address_and_no_vpn() {
        let list = [
            iface("lo0", &["127.0.0.1"], &["::1", "fe80::1"]),
            iface("en0", &["192.168.1.20"], &["fe80::1c2"]),
            iface("awdl0", &[], &["fe80::2"]),
            iface("utun0", &[], &["fe80::3"]),
            iface("utun1", &[], &["fe80::4"]),
            iface("utun2", &[], &["fe80::5"]),
            iface("utun3", &[], &["fe80::6"]),
        ];
        assert_eq!(classify_adapters(&list, Some("en0")), Adapters { local_address: true, vpn: false });
    }

    #[test]
    fn a_tunnel_is_a_vpn_only_with_a_routable_address_or_the_default_route() {
        let ipv4 = [iface("en0", &["192.168.1.20"], &[]), iface("utun4", &["10.8.0.2"], &[])];
        assert!(classify_adapters(&ipv4, Some("en0")).vpn);
        let ipv6 = [iface("en0", &["192.168.1.20"], &[]), iface("utun4", &[], &["fd00::2"])];
        assert!(classify_adapters(&ipv6, None).vpn);
        let bare = [iface("en0", &["192.168.1.20"], &[]), iface("utun4", &[], &["fe80::9"])];
        assert!(!classify_adapters(&bare, Some("en0")).vpn);
        assert!(classify_adapters(&bare, Some("utun4")).vpn);
        let ipsec = [iface("ipsec0", &["10.1.1.1"], &[])];
        assert!(classify_adapters(&ipsec, None).vpn);
        let ppp = [iface("ppp0", &["10.2.2.2"], &[])];
        assert!(classify_adapters(&ppp, None).vpn);
        // IPv4 self-assigned addresses do not count as routable.
        let linklocal = [iface("utun4", &["169.254.3.3"], &[])];
        assert!(!classify_adapters(&linklocal, None).vpn);
    }

    #[test]
    fn tunnel_addresses_and_down_interfaces_are_not_a_local_address() {
        let only_vpn = [iface("utun4", &["10.8.0.2"], &[])];
        assert_eq!(classify_adapters(&only_vpn, Some("utun4")), Adapters { local_address: false, vpn: true });
        let mut down = iface("en0", &["192.168.1.20"], &[]);
        down.up = false;
        let mut down_tunnel = iface("utun5", &["10.9.0.2"], &[]);
        down_tunnel.up = false;
        assert_eq!(classify_adapters(&[down, down_tunnel], None), Adapters::default());
        let selfassigned = [iface("en0", &["169.254.9.9"], &[]), iface("lo0", &["127.0.0.1"], &[])];
        assert_eq!(classify_adapters(&selfassigned, None), Adapters::default());
        assert_eq!(classify_adapters(&[], None), Adapters::default());
    }

    #[test]
    fn the_network_is_the_service_name_and_always_trusted() {
        use crate::win::sc::Primary;
        let named = Primary { interface: Some("en0".into()), service_name: Some("Wi-Fi".into()) };
        assert_eq!(networks_from(Some(named)), [Network { name: "Wi-Fi".into(), trusted: true }]);
        let unnamed = Primary { interface: Some("en0".into()), service_name: None };
        assert!(networks_from(Some(unnamed)).is_empty());
        assert!(networks_from(None).is_empty());
    }

    #[test]
    fn open_settings_tries_the_pane_then_system_settings() {
        let mut calls: Vec<Vec<String>> = Vec::new();
        assert!(open_firewall_settings_with(|args| {
            calls.push(args.iter().map(|a| (*a).to_owned()).collect());
            true
        }));
        assert_eq!(calls, [[FIREWALL_PANE]]);

        let mut calls: Vec<Vec<String>> = Vec::new();
        assert!(open_firewall_settings_with(|args| {
            calls.push(args.iter().map(|a| (*a).to_owned()).collect());
            calls.len() == 2
        }));
        assert_eq!(
            calls,
            [vec![FIREWALL_PANE.to_owned()], vec!["-b".to_owned(), SETTINGS_BUNDLE.to_owned()]]
        );

        let mut n = 0;
        assert!(!open_firewall_settings_with(|_| {
            n += 1;
            false
        }));
        assert_eq!(n, 2);
    }

    #[test]
    fn allow_does_nothing_from_here() {
        assert_eq!(fix_firewall(Path::new(EXE)), Ok(false));
    }

    #[test]
    fn run_program_returns_output_and_stops_a_slow_tool() {
        let out = run_program("/bin/echo", &["hello"], Duration::from_secs(5)).unwrap();
        assert_eq!(out, "hello\n");
        assert!(run_program("/usr/bin/false", &[], Duration::from_secs(5)).is_err());
        assert!(run_program("/nonexistent/tool", &[], Duration::from_secs(5)).is_err());
        let start = Instant::now();
        assert!(run_program("/bin/sleep", &["30"], Duration::from_millis(200)).is_err());
        assert!(start.elapsed() < Duration::from_secs(5));
    }
}

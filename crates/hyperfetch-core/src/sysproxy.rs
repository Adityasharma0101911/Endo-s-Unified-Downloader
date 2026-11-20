//! The proxy this computer's own settings name: Windows' Internet settings, macOS's network
//! settings, the Linux desktop's (GNOME's, else KDE's). Only when asked for (the app's setting,
//! `--system-proxy`), downloads take it when neither the app's settings nor the environment
//! (`http_proxy` and the like) name one: [`start`] puts it in the environment, where every client
//! of the app and every helper tool it starts finds it, and where the checks of what may not go
//! through a proxy (`engine::proxy_in_env`) see it. A setup script (PAC) or automatic detection,
//! which the app does not run, means none is taken.

use std::sync::OnceLock;

/// What the system's settings say.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum System {
    /// No proxy: direct.
    #[default]
    None,
    /// The proxies for `http://` links, for `https://` links and for all (a SOCKS one), the
    /// hosts that go direct as `NO_PROXY` lists them, whether host names without a dot go direct
    /// too (Windows' `<local>`, macOS's "Exclude simple hostnames"; see [`local_direct`]), and
    /// whose settings they are.
    Proxy { http: Option<String>, https: Option<String>, all: Option<String>, bypass: String, local: bool, from: &'static str },
    /// A setup script (PAC) at `url`, or automatic detection (`url` is [`DETECTION`]), which the
    /// app does not run.
    Script { url: String, from: &'static str },
}

/// The `url` of a [`System::Script`] that is automatic detection (WPAD).
pub const DETECTION: &str = "automatic detection";

/// How the system's proxy is turned off, in the app and on the command line.
pub const TURN_OFF: &str = "turn off \"Use the system's proxy\" (Settings > Network), or leave out --system-proxy";

/// What [`start`] found and did: the system's proxy (None when it was not asked for, or the
/// environment named one already), whether the environment did, and whether downloads take it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Seen {
    pub system: System,
    pub in_env: bool,
    pub taken: bool,
}

static SEEN: OnceLock<Seen> = OnceLock::new();

/// Hosts never reached through a proxy.
const LOOPBACK: &str = "localhost,127.0.0.1,::1";

/// When `use_it` and the environment names no proxy, reads the system's and puts it there
/// (`HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`, `NO_PROXY`, loopback added) for this process and the
/// tools it starts; not asked for, nothing of the system's is read. Call it once, before any other
/// thread starts: setting the environment while another thread reads it is not safe. Later calls
/// return what the first found. [`log`] logs it once the logger is set up.
pub fn start(use_it: bool) -> &'static Seen {
    SEEN.get_or_init(|| {
        let in_env = crate::engine::proxy_in_env();
        let system = if in_env || !use_it { System::None } else { read() };
        let taken = adopt(&system);
        Seen { system, in_env, taken }
    })
}

/// Logs the proxy [`start`] took from the system's settings, or why it took none.
pub fn log() {
    match seen() {
        Some(seen) if seen.taken => tracing::info!("Using the system's proxy: {}", describe(&seen.system)),
        Some(Seen { system: system @ System::Script { from, .. }, .. }) => {
            tracing::info!("Not using the system's proxy: the {from} settings use {}, which the app does not run", describe(system));
        }
        _ => {}
    }
}

/// For the engine's clients, the system's proxy when it was taken and keeps host names without a
/// dot direct, which `NO_PROXY` has no form for: the proxies [`start`] took, with `NO_PROXY`, such
/// names going direct. None otherwise: the clients then take the environment's as it is.
pub fn local_direct() -> Option<reqwest::Proxy> {
    let seen = seen().filter(|seen| seen.taken)?;
    let System::Proxy { http, https, all, local: true, .. } = &seen.system else { return None };
    let parse = |proxy: &Option<String>| proxy.as_deref().and_then(|proxy| reqwest::Url::parse(proxy).ok());
    let proxies = [parse(http), parse(https), parse(all)];
    let proxy = reqwest::Proxy::custom(move |url| route(url, &proxies).cloned());
    let bypass = std::env::var("NO_PROXY").unwrap_or_default();
    Some(proxy.no_proxy(reqwest::NoProxy::from_string(&bypass)))
}

/// Which of the `[http, https, all]` proxies `url` goes through: none for a host name without a
/// dot.
fn route<'a>(url: &reqwest::Url, [http, https, all]: &'a [Option<reqwest::Url>; 3]) -> Option<&'a reqwest::Url> {
    if !url.host_str().unwrap_or_default().contains(['.', ':']) {
        return None;
    }
    match url.scheme() {
        "http" => http.as_ref().or(all.as_ref()),
        "https" => https.as_ref().or(all.as_ref()),
        _ => all.as_ref(),
    }
}

/// What [`start`] found; None before it ran.
pub fn seen() -> Option<&'static Seen> {
    SEEN.get()
}

/// Puts `system`'s proxy in the environment; whether there was one.
fn adopt(system: &System) -> bool {
    let System::Proxy { http, https, all, bypass, .. } = system else { return false };
    for (name, value) in [("HTTP_PROXY", http), ("HTTPS_PROXY", https), ("ALL_PROXY", all)] {
        if let Some(value) = value {
            std::env::set_var(name, value);
        }
    }
    // What the environment already keeps direct stays so.
    let own = std::env::var("NO_PROXY").or_else(|_| std::env::var("no_proxy")).unwrap_or_default();
    let bypass: Vec<&str> = [own.as_str(), bypass, LOOPBACK].into_iter().filter(|hosts| !hosts.trim().is_empty()).collect();
    std::env::set_var("NO_PROXY", bypass.join(","));
    true
}

/// `system` in a few words: its proxies, else its script, else that there is none.
pub fn describe(system: &System) -> String {
    match system {
        System::None => "none".to_string(),
        System::Proxy { http, https, all, .. } => {
            let mut proxies: Vec<&str> = [http, https, all].into_iter().flatten().map(String::as_str).collect();
            proxies.dedup();
            proxies.join(", ")
        }
        System::Script { url, .. } if url == DETECTION => DETECTION.to_string(),
        System::Script { url, .. } => format!("a setup script (PAC), {url}"),
    }
}

/// The system's proxy settings, as they are now.
#[allow(unreachable_code)]
pub fn read() -> System {
    #[cfg(windows)]
    return windows::read();
    #[cfg(target_os = "macos")]
    return output_of("scutil", &["--proxy"]).map_or(System::None, |text| from_scutil(&text));
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let kde = std::env::var("XDG_CURRENT_DESKTOP").is_ok_and(|desktop| desktop.to_ascii_uppercase().contains("KDE"));
        let config = std::env::var_os("XDG_CONFIG_HOME").map(std::path::PathBuf::from).or_else(|| std::env::var_os("HOME").map(|home| std::path::Path::new(&home).join(".config")));
        if kde {
            if let Some(text) = config.and_then(|dir| std::fs::read_to_string(dir.join("kioslaverc")).ok()) {
                return from_kioslaverc(&text);
            }
        }
        return output_of("gsettings", &["list-recursively", "org.gnome.system.proxy"]).map_or(System::None, |text| from_gsettings(&text));
    }
    System::None
}

/// What `program args` prints, if it runs and succeeds within 2 s (a stuck one is killed).
#[cfg(unix)]
fn output_of(program: &str, args: &[&str]) -> Option<String> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    let mut child = Command::new(program).args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() < deadline => std::thread::sleep(std::time::Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    let mut text = String::new();
    child.stdout.take()?.read_to_string(&mut text).ok()?;
    status.success().then_some(text)
}

/// `host:port`, or a URL already, as a proxy URL of `scheme`.
fn proxy_url(scheme: &str, address: &str) -> Option<String> {
    let address = address.trim();
    match address {
        "" => None,
        _ if address.contains("://") => Some(address.to_string()),
        _ => Some(format!("{scheme}://{address}")),
    }
}

/// Windows' settings: `ProxyEnable`, `ProxyServer` (`host:port` for all, or
/// `http=host:port;https=host:port;socks=host:port`), `ProxyOverride` (`*.example.com;<local>`),
/// `AutoConfigURL` and whether settings are detected automatically (`detect`). Windows asks a
/// setup script, or automatic detection, first and takes the proxy set by hand only when they give
/// none: the app runs neither, so with either it takes no proxy.
pub fn from_windows(enabled: bool, server: Option<&str>, bypass: Option<&str>, script: Option<&str>, detect: bool) -> System {
    let from = "Windows";
    if let Some(url) = script.map(str::trim).filter(|url| !url.is_empty()) {
        return System::Script { url: url.to_string(), from };
    }
    if detect {
        return System::Script { url: DETECTION.to_string(), from };
    }
    let Some(server) = server.map(str::trim).filter(|server| enabled && !server.is_empty()) else { return System::None };
    let (mut http, mut https, mut all) = (None, None, None);
    if server.contains('=') {
        for entry in server.split(';') {
            match entry.split_once('=').map(|(kind, address)| (kind.trim().to_ascii_lowercase(), address)) {
                Some((kind, address)) if kind == "http" => http = proxy_url("http", address),
                Some((kind, address)) if kind == "https" => https = proxy_url("http", address),
                // Windows' own SOCKS setting is SOCKS 4.
                Some((kind, address)) if kind == "socks" => all = proxy_url("socks4", address),
                _ => {}
            }
        }
    } else {
        (http, https) = (proxy_url("http", server), proxy_url("http", server));
    }
    let hosts: Vec<&str> = bypass.unwrap_or_default().split(';').map(str::trim).filter(|host| !host.is_empty()).collect();
    // `<local>`: names without a dot, which NO_PROXY has no form for (see `local_direct`).
    let local = hosts.contains(&"<local>");
    let bypass = hosts.into_iter().filter(|host| *host != "<local>").map(no_proxy_entry).collect::<Vec<_>>().join(",");
    match (&http, &https, &all) {
        (None, None, None) => System::None,
        _ => System::Proxy { http, https, all, bypass, local, from },
    }
}

/// A host pattern as NO_PROXY takes it: `*.example.com` is `.example.com`, `10.*` is
/// `10.0.0.0/8`.
fn no_proxy_entry(host: &str) -> String {
    let parts: Vec<&str> = host.split('.').collect();
    let stars = parts.iter().rev().take_while(|part| **part == "*").count();
    let numbers = &parts[..parts.len() - stars];
    if stars > 0 && !numbers.is_empty() && numbers.len() < 4 && numbers.iter().all(|part| part.parse::<u8>().is_ok()) {
        let mut octets = numbers.to_vec();
        octets.resize(4, "0");
        return format!("{}/{}", octets.join("."), 8 * numbers.len());
    }
    host.strip_prefix('*').unwrap_or(host).to_string()
}

/// macOS's settings, as `scutil --proxy` prints them.
pub fn from_scutil(text: &str) -> System {
    let from = "macOS";
    let value = |key: &str| text.lines().find_map(|line| line.trim().strip_prefix(key)?.trim_start().strip_prefix(':').map(str::trim));
    let enabled = |kind: &str| value(&format!("{kind}Enable")) == Some("1");
    let proxy = |kind: &str, scheme: &str| {
        let (host, port) = (value(&format!("{kind}Proxy"))?, value(&format!("{kind}Port")));
        enabled(kind).then(|| proxy_url(scheme, &port.map_or(host.to_string(), |port| format!("{host}:{port}"))))?
    };
    // A setup script or automatic detection comes before a proxy set by hand, as on Windows.
    if let Some(url) = value("ProxyAutoConfigURLString").filter(|_| enabled("ProxyAutoConfig")) {
        return System::Script { url: url.to_string(), from };
    }
    if enabled("ProxyAutoDiscovery") {
        return System::Script { url: DETECTION.to_string(), from };
    }
    let (http, https, all) = (proxy("HTTP", "http"), proxy("HTTPS", "http"), proxy("SOCKS", "socks5"));
    if http.is_none() && https.is_none() && all.is_none() {
        return System::None;
    }
    // The exceptions are an array, an entry a line: `0 : *.local`.
    let mut bypass = Vec::new();
    let mut lines = text.lines().skip_while(|line| !line.trim_start().starts_with("ExceptionsList"));
    lines.next();
    for line in lines.take_while(|line| line.trim() != "}") {
        if let Some((_, host)) = line.split_once(':') {
            bypass.push(no_proxy_entry(host.trim()));
        }
    }
    let local = value("ExcludeSimpleHostnames") == Some("1");
    System::Proxy { http, https, all, bypass: bypass.join(","), local, from }
}

/// GNOME's settings, as `gsettings list-recursively org.gnome.system.proxy` prints them.
pub fn from_gsettings(text: &str) -> System {
    let from = "GNOME";
    let value = |schema: &str, key: &str| {
        text.lines().find_map(|line| {
            let mut words = line.splitn(3, ' ');
            (words.next()? == schema && words.next()? == key).then(|| words.next().unwrap_or_default().trim().trim_matches('\'').to_string())
        })
    };
    let base = "org.gnome.system.proxy";
    match value(base, "mode").as_deref() {
        Some("manual") => {}
        Some("auto") => {
            let url = value(base, "autoconfig-url").filter(|url| !url.is_empty()).unwrap_or_else(|| DETECTION.to_string());
            return System::Script { url, from };
        }
        _ => return System::None,
    }
    let proxy = |kind: &str, scheme: &str| {
        let schema = format!("{base}.{kind}");
        let host = value(&schema, "host").filter(|host| !host.is_empty())?;
        let port = value(&schema, "port").filter(|port| port != "0");
        proxy_url(scheme, &port.map_or(host.clone(), |port| format!("{host}:{port}")))
    };
    let http = proxy("http", "http");
    let same = value(base, "use-same-proxy").as_deref() == Some("true");
    let https = proxy("https", "http").or_else(|| http.clone().filter(|_| same));
    let all = proxy("socks", "socks5");
    if http.is_none() && https.is_none() && all.is_none() {
        return System::None;
    }
    // `['localhost', '127.0.0.0/8', '::1']`
    let hosts = value(base, "ignore-hosts").unwrap_or_default();
    let bypass = hosts.trim_matches(['[', ']', '@', 'a', 's', ' ']).split(',').map(|host| host.trim().trim_matches('\'')).filter(|host| !host.is_empty()).map(no_proxy_entry).collect::<Vec<_>>().join(",");
    System::Proxy { http, https, all, bypass, local: false, from }
}

/// KDE's settings, the `[Proxy Settings]` of `kioslaverc`: `ProxyType` 1 is by hand, 2 a setup
/// script, 3 automatic detection; addresses are `http://host:port` or `http://host port`.
pub fn from_kioslaverc(text: &str) -> System {
    let from = "KDE";
    let mut section = false;
    let mut values = std::collections::HashMap::new();
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            section = line == "[Proxy Settings]";
        } else if let Some((key, value)) = line.split_once('=').filter(|_| section) {
            values.insert(key.trim(), value.trim());
        }
    }
    let get = |key: &str| values.get(key).copied().filter(|value| !value.is_empty());
    match get("ProxyType") {
        Some("1") => {}
        Some("2") => return get("Proxy Config Script").map_or(System::None, |url| System::Script { url: url.to_string(), from }),
        Some("3") => return System::Script { url: DETECTION.to_string(), from },
        _ => return System::None,
    }
    let proxy = |key: &str, scheme: &str| {
        let address = get(key)?.replacen(' ', ":", 1);
        let address = address.split_once("://").map_or(address.as_str(), |(_, rest)| rest).to_string();
        proxy_url(scheme, &address)
    };
    let (http, https, all) = (proxy("httpProxy", "http"), proxy("httpsProxy", "http"), proxy("socksProxy", "socks5"));
    if http.is_none() && https.is_none() && all.is_none() {
        return System::None;
    }
    let bypass = get("NoProxyFor").unwrap_or_default().split(',').map(str::trim).filter(|host| !host.is_empty()).map(no_proxy_entry).collect::<Vec<_>>().join(",");
    System::Proxy { http, https, all, bypass, local: false, from }
}

#[cfg(windows)]
mod windows {
    use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_BINARY, RRF_RT_REG_DWORD, RRF_RT_REG_SZ};

    const KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Internet Settings";
    /// Where `DefaultConnectionSettings` keeps the switches of the LAN settings.
    const CONNECTIONS: &str = r"Software\Microsoft\Windows\CurrentVersion\Internet Settings\Connections";
    /// "Automatically detect settings", among the flags at byte 8 of those settings.
    const AUTO_DETECT: u8 = 8;

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain([0]).collect()
    }

    fn dword(name: &str) -> Option<u32> {
        let (key, name) = (wide(KEY), wide(name));
        let (mut value, mut size) = (0u32, 4u32);
        // SAFETY: the names end in NUL; `value` holds the `size` bytes asked for.
        let status = unsafe {
            RegGetValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr(), RRF_RT_REG_DWORD, std::ptr::null_mut(), (&mut value as *mut u32).cast(), &mut size)
        };
        (status == 0).then_some(value)
    }

    fn string(name: &str) -> Option<String> {
        let (key, name) = (wide(KEY), wide(name));
        let mut size = 0u32;
        // SAFETY: as in `dword`; asked first with no buffer, for its size in bytes.
        let status = unsafe { RegGetValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr(), RRF_RT_REG_SZ, std::ptr::null_mut(), std::ptr::null_mut(), &mut size) };
        if status != 0 || size < 2 {
            return None;
        }
        let mut text = vec![0u16; size as usize / 2];
        // SAFETY: `text` holds the `size` bytes the first call asked for.
        let status = unsafe {
            RegGetValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr(), RRF_RT_REG_SZ, std::ptr::null_mut(), text.as_mut_ptr().cast(), &mut size)
        };
        let end = text.iter().position(|&c| c == 0).unwrap_or(text.len());
        (status == 0).then(|| String::from_utf16_lossy(&text[..end]))
    }

    /// The flags of `DefaultConnectionSettings`, after its version and counter.
    fn connection_flags() -> Option<u8> {
        let (key, name) = (wide(CONNECTIONS), wide("DefaultConnectionSettings"));
        let mut size = 0u32;
        // SAFETY: as in `string`: asked first with no buffer, for its size.
        let status = unsafe { RegGetValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr(), RRF_RT_REG_BINARY, std::ptr::null_mut(), std::ptr::null_mut(), &mut size) };
        if status != 0 || size < 12 {
            return None;
        }
        let mut value = vec![0u8; size as usize];
        // SAFETY: `value` holds the `size` bytes the first call asked for.
        let status = unsafe {
            RegGetValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr(), RRF_RT_REG_BINARY, std::ptr::null_mut(), value.as_mut_ptr().cast(), &mut size)
        };
        (status == 0 && size >= 12).then(|| value[8])
    }

    pub(super) fn read() -> super::System {
        let (server, bypass, script) = (string("ProxyServer"), string("ProxyOverride"), string("AutoConfigURL"));
        let detect = connection_flags().is_some_and(|flags| flags & AUTO_DETECT != 0);
        super::from_windows(dword("ProxyEnable") == Some(1), server.as_deref(), bypass.as_deref(), script.as_deref(), detect)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proxy(http: Option<&str>, https: Option<&str>, all: Option<&str>, bypass: &str, from: &'static str) -> System {
        let own = |value: Option<&str>| value.map(String::from);
        System::Proxy { http: own(http), https: own(https), all: own(all), bypass: bypass.to_string(), local: false, from }
    }

    fn local(system: System) -> System {
        match system {
            System::Proxy { http, https, all, bypass, from, .. } => System::Proxy { http, https, all, bypass, local: true, from },
            other => other,
        }
    }

    /// A proxy set by hand is read with its exceptions, `<local>` kept apart; a setup script or
    /// automatic detection, which Windows asks first, means none.
    #[test]
    fn windows_settings_are_read() {
        let one = Some("proxy.example:8080");
        assert_eq!(from_windows(false, one, None, None, false), System::None, "switched off");
        let both = proxy(Some("http://proxy.example:8080"), Some("http://proxy.example:8080"), None, ".corp.example,10.0.0.0/8", "Windows");
        assert_eq!(from_windows(true, one, Some("*.corp.example;10.*;<local>"), None, false), local(both.clone()));
        assert_eq!(from_windows(true, one, Some("*.corp.example;10.*"), None, false), both);
        let each = Some("http=h.example:80;https=s.example:443;ftp=f.example:21;socks=k.example:1080");
        assert_eq!(from_windows(true, each, None, None, false), proxy(Some("http://h.example:80"), Some("http://s.example:443"), Some("socks4://k.example:1080"), "", "Windows"));
        let script = Some("http://wpad.example/proxy.pac");
        assert_eq!(from_windows(false, one, None, script, false), System::Script { url: "http://wpad.example/proxy.pac".into(), from: "Windows" });
        assert_eq!(from_windows(true, one, None, script, false), System::Script { url: "http://wpad.example/proxy.pac".into(), from: "Windows" }, "the script first");
        assert_eq!(from_windows(true, one, None, None, true), System::Script { url: DETECTION.into(), from: "Windows" }, "detection first");
    }

    /// With `<local>`, a host name without a dot goes direct; any other takes the proxy of its
    /// scheme, else the one for all.
    #[test]
    fn a_name_without_a_dot_goes_direct() {
        let url = |text: &str| reqwest::Url::parse(text).unwrap();
        let proxies = [Some(url("http://h.example:80")), None, Some(url("socks4://k.example:1080"))];
        assert_eq!(route(&url("http://fileserver/x.iso"), &proxies), None);
        assert_eq!(route(&url("http://files.corp.example/x.iso"), &proxies), proxies[0].as_ref());
        assert_eq!(route(&url("https://10.1.2.3/x.iso"), &proxies), proxies[2].as_ref());
        assert_eq!(route(&url("https://[2001:db8::1]/x.iso"), &proxies), proxies[2].as_ref());
    }

    #[test]
    fn macos_settings_are_read() {
        let text = "<dictionary> {\n  ExceptionsList : <array> {\n    0 : *.local\n    1 : 169.254/16\n  }\n  ExcludeSimpleHostnames : 1\n  FTPPassive : 1\n  HTTPEnable : 1\n  HTTPPort : 3128\n  HTTPProxy : p.example\n  HTTPSEnable : 0\n  HTTPSPort : 3128\n  HTTPSProxy : p.example\n  SOCKSEnable : 1\n  SOCKSPort : 1080\n  SOCKSProxy : s.example\n}\n";
        assert_eq!(from_scutil(text), local(proxy(Some("http://p.example:3128"), None, Some("socks5://s.example:1080"), ".local,169.254/16", "macOS")));
        let script = "<dictionary> {\n  HTTPEnable : 1\n  HTTPPort : 3128\n  HTTPProxy : p.example\n  ProxyAutoConfigEnable : 1\n  ProxyAutoConfigURLString : http://w.example/p.pac\n}\n";
        assert_eq!(from_scutil(script), System::Script { url: "http://w.example/p.pac".into(), from: "macOS" }, "the script first");
        assert_eq!(from_scutil("<dictionary> {\n  HTTPEnable : 0\n}\n"), System::None);
    }

    /// Not asked for, nothing of the system's is read or taken.
    #[test]
    fn nothing_is_taken_unless_asked_for() {
        let seen = start(false);
        assert_eq!((&seen.system, seen.taken), (&System::None, false));
        assert!(local_direct().is_none());
    }

    #[test]
    fn linux_desktop_settings_are_read() {
        let gnome = "org.gnome.system.proxy autoconfig-url ''\norg.gnome.system.proxy ignore-hosts ['localhost', '127.0.0.0/8', '*.corp.example']\norg.gnome.system.proxy mode 'manual'\norg.gnome.system.proxy use-same-proxy true\norg.gnome.system.proxy.http host 'p.example'\norg.gnome.system.proxy.http port 8080\norg.gnome.system.proxy.https host ''\norg.gnome.system.proxy.https port 0\norg.gnome.system.proxy.socks host ''\norg.gnome.system.proxy.socks port 0\n";
        assert_eq!(from_gsettings(gnome), proxy(Some("http://p.example:8080"), Some("http://p.example:8080"), None, "localhost,127.0.0.0/8,.corp.example", "GNOME"));
        assert_eq!(from_gsettings(&gnome.replace("'manual'", "'none'")), System::None);
        let auto = gnome.replace("'manual'", "'auto'").replace("autoconfig-url ''", "autoconfig-url 'http://w.example/p.pac'");
        assert_eq!(from_gsettings(&auto), System::Script { url: "http://w.example/p.pac".into(), from: "GNOME" });

        let kde = "[General]\nx=1\n[Proxy Settings]\nProxyType=1\nhttpProxy=http://p.example 8080\nhttpsProxy=http://p.example:8443\nsocksProxy=\nNoProxyFor=localhost,*.corp.example\n";
        assert_eq!(from_kioslaverc(kde), proxy(Some("http://p.example:8080"), Some("http://p.example:8443"), None, "localhost,.corp.example", "KDE"));
        assert_eq!(from_kioslaverc(&kde.replace("ProxyType=1", "ProxyType=0")), System::None);
    }

    #[test]
    fn a_proxy_is_described_in_a_few_words() {
        assert_eq!(describe(&from_windows(true, Some("p.example:8080"), None, None, false)), "http://p.example:8080");
        assert_eq!(describe(&System::None), "none");
        assert_eq!(describe(&from_windows(true, None, None, None, true)), DETECTION);
    }
}

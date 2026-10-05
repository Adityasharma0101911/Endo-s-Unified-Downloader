//! Credentials inside a paste: a link with `user:pass@`, a link with "Password: x", "Key: x",
//! "Bearer x" or a known API token near it, or a whole copied request (DevTools' "Copy as cURL",
//! "Copy as fetch" and "Copy as PowerShell", or a wget command). [`parse`] cuts each secret out
//! and pairs it with its link; [`apply`] gives a link's secrets to its download, which sends them
//! only to the link's own hosts (see `engine::DownloadOptions::secret_headers`) and never saves
//! them.
//!
//! In plain text a secret on a link's line, or on the lines after it until the next link, is that
//! link's. Secrets before the first link are every link's, and so are those after the last link
//! when no link has any of its own (the parts of an archive, then its password). A known API token
//! only ever goes to a link of its service, and a key only to a MEGA link that lacks one. A
//! copied command is one link with what the command sends.

use std::fmt;
use std::iter::Peekable;
use std::str::Chars;

use base64::Engine as _;
use percent_encoding::percent_decode_str;
use serde_json::Value;
use url::Url;

use crate::engine::DownloadOptions;
use crate::ingest::http_url;

/// What a copied request that sends data is refused with.
pub const SENDS_DATA: &str = "This copied request sends data (POST); only downloads (GET) can be added";

/// What a copied fetch call that is not one DevTools writes is refused with.
const UNREADABLE_FETCH: &str = "The copied fetch request could not be read";

/// Request headers a copied request's download does not send: the engine sets them itself
/// (Range, Accept-Encoding, …), they belong to the copied connection, or are HTTP/2's pseudo
/// headers as PowerShell copies them. `If-*` and `Sec-Fetch-*` go too.
const DROPPED: &[&str] = &[
    "host",
    "content-length",
    "content-type",
    "connection",
    "keep-alive",
    "proxy-connection",
    "proxy-authorization",
    "transfer-encoding",
    "te",
    "trailer",
    "upgrade",
    "expect",
    "accept-encoding",
    "range",
    "priority",
    "authority",
    "method",
    "path",
    "scheme",
];

/// Labels a secret goes by, case aside, a longer one before one it starts with.
const LABELS: &[(&str, Label)] = &[
    ("mot de passe", Label::Password),
    ("contraseña", Label::Password),
    ("password", Label::Password),
    ("passwd", Label::Password),
    ("parola", Label::Password),
    ("pass", Label::Password),
    ("pwd", Label::Password),
    ("pw", Label::Password),
    ("decryption key", Label::Key),
    ("mega key", Label::Key),
    ("key", Label::Key),
    ("access token", Label::Token),
    ("api key", Label::Token),
    ("apikey", Label::Token),
    ("token", Label::Token),
    ("bearer", Label::Token),
    ("authorization", Label::Authorization),
    ("cookie", Label::Cookie),
];

/// What "Password: none" and the like say: there is no password.
const NO_PASSWORD: &[&str] = &["none", "no", "n/a", "na", "-", "nil", "null", "empty", "nothing"];

/// API tokens known by their prefix: the characters allowed after it besides letters and digits,
/// how many at least, and the service that takes them.
const TOKENS: &[(&str, &str, usize, Service)] = &[
    ("ghp_", "", 36, Service::GitHub),
    ("gho_", "", 36, Service::GitHub),
    ("github_pat_", "_", 50, Service::GitHub),
    ("hf_", "", 30, Service::HuggingFace),
    ("glpat-", "_-.", 20, Service::GitLab),
];

/// What a paste said about one link besides the link itself. Its `Debug` names the fields set,
/// never their values.
#[derive(Clone, Default, PartialEq)]
pub struct Secrets {
    /// An `Authorization` header value: `Basic …` from `user:pass@`, `Bearer …` from a token.
    pub auth_header: Option<String>,
    /// The password of a share, a video or an archive.
    pub password: Option<String>,
    /// A decryption key the link lacked (MEGA's), joined to the link.
    pub key: Option<String>,
    /// A `Cookie` header value.
    pub cookies: Option<String>,
    /// Other request headers (User-Agent, X-Api-Key, GitLab's PRIVATE-TOKEN, …).
    pub headers: Vec<(String, String)>,
    pub referer: Option<String>,
}

impl fmt::Debug for Secrets {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let set = [
            ("auth_header", self.auth_header.is_some()),
            ("password", self.password.is_some()),
            ("key", self.key.is_some()),
            ("cookies", self.cookies.is_some()),
            ("headers", !self.headers.is_empty()),
            ("referer", self.referer.is_some()),
        ];
        f.write_str("Secrets")?;
        f.debug_list().entries(set.iter().filter(|(_, on)| *on).map(|(name, _)| name)).finish()
    }
}

impl Secrets {
    /// What a download using these tells the user, naming what it uses, never a value: "Using
    /// the password and sign-in from your paste for h.example". None when it uses nothing.
    pub fn note(&self, url: &Url) -> Option<String> {
        let sign_in = self.auth_header.is_some() || self.cookies.is_some() || !self.headers.is_empty();
        let used: Vec<&str> = [("password", self.password.is_some()), ("key", self.key.is_some()), ("sign-in", sign_in)]
            .into_iter()
            .filter_map(|(name, on)| on.then_some(name))
            .collect();
        let host = url.host_str().unwrap_or("this link");
        (!used.is_empty()).then(|| format!("Using the {} from your paste for {}", used.join(" and "), host))
    }

    /// Takes from `other` what this does not say already.
    fn fill(&mut self, other: &Secrets) {
        for (mine, theirs) in [
            (&mut self.auth_header, &other.auth_header),
            (&mut self.password, &other.password),
            (&mut self.key, &other.key),
            (&mut self.cookies, &other.cookies),
            (&mut self.referer, &other.referer),
        ] {
            if mine.is_none() {
                mine.clone_from(theirs);
            }
        }
        for (name, value) in &other.headers {
            if !self.headers.iter().any(|(n, _)| n.eq_ignore_ascii_case(name)) {
                self.headers.push((name.clone(), value.clone()));
            }
        }
    }
}

/// One link of a paste, with what the paste said about it.
#[derive(Clone, Debug, PartialEq)]
pub struct PastedLink {
    /// The link without its `user:pass@` (in `secrets.auth_header` now), with the MEGA key the
    /// paste gave joined to it.
    pub url: Url,
    pub secrets: Secrets,
}

/// The links of `text` with the secrets the paste gives each, in paste order, a link given twice
/// once with the secrets of both. None when `text` is no copied request and holds no secret for
/// any link: it is taken as any other input. `Err` (with [`SENDS_DATA`]) for a copied request that
/// sends data, unless other requests copied with it are downloads.
pub fn parse(text: &str) -> Option<Result<Vec<PastedLink>, String>> {
    if let Some(requests) = requests(text) {
        let (mut links, mut refused) = (Vec::new(), None);
        for result in requests.iter().filter_map(|(tool, command)| request(*tool, command).transpose()) {
            match result {
                Ok(link) => links.push(link),
                Err(e) => refused = refused.or(Some(e)),
            }
        }
        return match refused {
            Some(e) if links.is_empty() => Some(Err(e)),
            _ => (!links.is_empty()).then(|| Ok(merge(links))),
        };
    }
    let links = merge(plain(text));
    links.iter().any(|l| l.secrets != Secrets::default()).then_some(Ok(links))
}

/// Gives `link`'s secrets to its download's `options`: the Authorization header unless the user
/// entered one (theirs wins), as do their password and referer; the headers, the cookie and the
/// referer (a copied request's may hold a session) replace any of the same name in
/// `secret_headers`, which the engine sends only to the link's own hosts and never saves. The
/// key is in the link already.
pub fn apply(link: &PastedLink, options: &mut DownloadOptions) {
    let s = &link.secrets;
    let typed = |value: &Option<String>| value.as_deref().is_some_and(|v| !v.trim().is_empty());
    for (mine, pasted) in [(&mut options.auth_header, &s.auth_header), (&mut options.password, &s.password)] {
        if !typed(mine) && pasted.is_some() {
            mine.clone_from(pasted);
        }
    }
    let cookie = s.cookies.as_ref().map(|c| ("Cookie".to_string(), c.clone()));
    let referer = s.referer.as_ref().filter(|_| !typed(&options.referer)).map(|r| ("Referer".to_string(), r.clone()));
    for (name, value) in s.headers.iter().cloned().chain(cookie).chain(referer) {
        options.secret_headers.retain(|(n, _)| !n.eq_ignore_ascii_case(&name));
        options.secret_headers.push((name, value));
    }
}

/// The lines of a paste that [`parse`] leaves out but are inputs of their own, with their numbers
/// from 1: a magnet link, a local .torrent or .metalink (see `ingest::needs_reading`).
pub fn other_inputs(text: &str) -> Vec<(usize, &str)> {
    let input = |line: &str| read_line(line).0.is_empty() && crate::ingest::needs_reading(line);
    text.lines().map(str::trim).enumerate().filter(|(_, line)| input(line)).map(|(n, line)| (n + 1, line)).collect()
}

/// What the user entered as an Authorization header, `user:pass` or a link with its `user:pass@`
/// as the Basic credentials they stand for.
pub fn authorization(entered: &str) -> String {
    let entered = entered.trim();
    match http_url(entered) {
        Some(url) if !url.username().is_empty() || url.password().is_some() => {
            link(url, Secrets::default()).secrets.auth_header.unwrap_or_default()
        }
        None if entered.contains(':') && !entered.contains(char::is_whitespace) => basic(entered),
        _ => entered.to_string(),
    }
}

/// `Basic` credentials for `user:password`.
fn basic(pair: &str) -> String {
    format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(pair))
}

/// `url` with `secrets` as a pasted link: its `user:pass@` taken out as Basic credentials (unless
/// the paste gave others), a "leaving this site" link replaced by its target (as
/// `ingest::link_task` takes it), the key joined to a MEGA link that lacks one.
fn link(mut url: Url, mut secrets: Secrets) -> PastedLink {
    if !url.username().is_empty() || url.password().is_some() {
        let decode = |s: &str| percent_decode_str(s).decode_utf8_lossy().into_owned();
        let pair = format!("{}:{}", decode(url.username()), decode(url.password().unwrap_or_default()));
        secrets.auth_header.get_or_insert_with(|| basic(&pair));
        let _ = url.set_username("");
        let _ = url.set_password(None);
    }
    if let Some(target) = crate::resolver::unwrap_redirect(&url) {
        // What was sent to the wrapper's host (a copied request's cookies, its sign-in) is not
        // for another host; the password and key are the target's.
        if target.host_str() != url.host_str() {
            secrets = Secrets { password: secrets.password, key: secrets.key, ..Default::default() };
        }
        url = target;
    }
    if let Some(joined) = secrets.key.as_deref().and_then(|key| with_key(&url, key)) {
        url = joined;
    }
    PastedLink { url, secrets }
}

/// `links` with a link given twice once, where it first was, with the secrets of both.
fn merge(links: Vec<PastedLink>) -> Vec<PastedLink> {
    let mut merged: Vec<PastedLink> = Vec::new();
    for link in links {
        match merged.iter_mut().find(|m| m.url == link.url) {
            Some(m) => m.secrets.fill(&link.secrets),
            None => merged.push(link),
        }
    }
    merged
}

/// `url`, a MEGA file or folder link without its key, with `key` joined to it; None for any other
/// link, a MEGA link with its key included.
fn with_key(url: &Url, key: &str) -> Option<Url> {
    let host = url.host_str()?;
    if !matches!(host.strip_prefix("www.").unwrap_or(host), "mega.nz" | "mega.co.nz") {
        return None;
    }
    let fragment = url.fragment().unwrap_or_default();
    let path: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    let joined = match path.as_slice() {
        ["file" | "folder" | "embed", _] if fragment.is_empty() => key.to_string(),
        // The older `#!<id>` and `#F!<id>`, the key after one more `!`.
        [] if (fragment.starts_with('!') || fragment.starts_with("F!")) && fragment.matches('!').count() == 1 => {
            format!("{fragment}!{key}")
        }
        _ => return None,
    };
    let mut url = url.clone();
    url.set_fragment(Some(&joined));
    Some(url)
}

/// A MEGA key as links spell it: a file's (43 characters) or a folder's (22) in URL-safe base64.
fn is_mega_key(key: &str) -> bool {
    matches!(key.len(), 22 | 43) && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Where a secret found in plain text may go, besides any link.
#[derive(Clone, Copy, PartialEq)]
enum Service {
    GitHub,
    HuggingFace,
    GitLab,
    /// A MEGA link without its key.
    Mega,
}

impl Service {
    fn serves(self, url: &Url) -> bool {
        let host = url.host_str().unwrap_or_default().trim_end_matches('.').to_ascii_lowercase();
        let under = |domain: &str| host == domain || host.strip_suffix(domain).is_some_and(|sub| sub.ends_with('.'));
        match self {
            Self::GitHub => under("github.com") || under("githubusercontent.com"),
            Self::HuggingFace => under("huggingface.co") || under("hf.co"),
            Self::GitLab => under("gitlab.com") || host.starts_with("gitlab."),
            Self::Mega => with_key(url, "").is_some(),
        }
    }
}

/// A secret found in plain text, with the service whose links alone it goes to (None: any link).
type Item = (Option<Service>, Secrets);

/// The secrets the known API token `word` stands for, with the service that takes it: a bearer
/// token, but GitLab's, which goes in a `PRIVATE-TOKEN` header.
fn known_token(word: &str) -> Option<Item> {
    let (prefix, extra, least, service) = TOKENS.iter().find(|(prefix, ..)| word.starts_with(prefix))?;
    let rest = &word[prefix.len()..];
    if rest.len() < *least || !rest.chars().all(|c| c.is_ascii_alphanumeric() || extra.contains(c)) {
        return None;
    }
    let secrets = match service {
        Service::GitLab => Secrets { headers: vec![("PRIVATE-TOKEN".into(), word.into())], ..Default::default() },
        _ => Secrets { auth_header: Some(format!("Bearer {word}")), ..Default::default() },
    };
    Some((Some(*service), secrets))
}

/// The links of a paste of plain text, each with the secrets meant for it (see the module's
/// documentation).
fn plain(text: &str) -> Vec<PastedLink> {
    // Each line with links, with the secrets on it and those on the lines after it.
    let mut groups: Vec<(Vec<Url>, Vec<Item>, Vec<Item>)> = Vec::new();
    let mut shared = Vec::new();
    for line in text.lines() {
        let (links, items) = read_line(line);
        if !links.is_empty() {
            groups.push((links, items, Vec::new()));
        } else if let Some((_, _, after)) = groups.last_mut() {
            after.extend(items);
        } else {
            shared.extend(items);
        }
    }
    if let Some(((_, own, after), earlier)) = groups.split_last_mut() {
        if own.is_empty() && earlier.iter().all(|(_, own, after)| own.is_empty() && after.is_empty()) {
            shared.append(after);
        } else if own.is_empty() && after.is_empty() && !shared.is_empty() && earlier.iter().any(|(_, _, after)| !after.is_empty()) {
            // Each secret before its link ("Pass: 1\n<link>\nPass: 2\n<link>"): the lines after a
            // link are the next link's, those before the first link the first's.
            let mut before = std::mem::take(&mut shared);
            for (_, _, after) in &mut groups {
                std::mem::swap(after, &mut before);
            }
        }
    }
    let mut links = Vec::new();
    for (urls, own, after) in groups {
        for url in urls {
            let mut secrets = Secrets::default();
            for (service, found) in own.iter().chain(&after).chain(&shared) {
                if service.is_none_or(|s| s.serves(&url)) {
                    secrets.fill(found);
                }
            }
            links.push(link(url, secrets));
        }
    }
    links
}

/// The links in `line` (http(s) URLs, see [`link_end`]) and the secrets the rest of it gives: a
/// label after its last link (or on a line without one), and known API tokens and MEGA key
/// fragments (`#key`, `!key`) anywhere.
fn read_line(line: &str) -> (Vec<Url>, Vec<Item>) {
    let lower = line.to_ascii_lowercase();
    let (mut links, mut rest, mut at) = (Vec::new(), String::new(), 0);
    while let Some(start) = ["http://", "https://"].iter().filter_map(|s| lower[at..].find(s)).min().map(|i| at + i) {
        let link = link_end(&line[start..]);
        links.extend(http_url(link));
        rest.push_str(&line[at..start]);
        rest.push(' ');
        at = start + link.len();
    }
    rest.push_str(&line[at..]);
    let mut items: Vec<Item> = labelled(if links.is_empty() { line } else { &line[at..] }).into_iter().collect();
    for word in rest.split_whitespace().map(|w| w.trim_matches(|c: char| "\"'`()[]<>,;.".contains(c))) {
        if let Some(item) = known_token(word) {
            items.push(item);
        } else if let Some(key) = word.strip_prefix(['#', '!']).filter(|key| is_mega_key(key)) {
            items.push((Some(Service::Mega), Secrets { key: Some(key.into()), ..Default::default() }));
        }
    }
    (links, items)
}

/// The link `text` starts with: up to a space, `"`, `<`, `>` or `` ` ``, without the `.,:;` of the
/// sentence after it nor a `)`, `]`, `}` or `'` it does not open (`(see <link>)`, `'<link>'`).
/// Browsers copy `( ) [ ] '` in links as they are: "Foo%20(1999).zip", "[Group]%20Show.mkv".
fn link_end(text: &str) -> &str {
    let mut link = &text[..text.find(|c: char| c.is_whitespace() || "\"<>`".contains(c)).unwrap_or(text.len())];
    loop {
        link = link.trim_end_matches(['.', ',', ':', ';']);
        let unopened = |open: char, close: char| link.ends_with(close) && link.matches(close).count() > link.matches(open).count();
        let quoted = link.ends_with('\'') && link.matches('\'').count() % 2 == 1;
        if !(unopened('(', ')') || unopened('[', ']') || unopened('{', '}') || quoted) {
            return link;
        }
        link = &link[..link.len() - 1];
    }
}

#[derive(Clone, Copy)]
enum Label {
    Password,
    Key,
    Token,
    Authorization,
    Cookie,
}

/// The secret a labelled piece of text gives ("Password: x", "key=x", "Bearer x", "- **pw:** x"),
/// with where it may go. A known API token is left to [`known_token`].
fn labelled(text: &str) -> Option<Item> {
    let text = text.replace("**", "");
    let text = text.trim();
    // "(password: x)" after a link.
    let text = match text.chars().next() {
        Some('(') => text.strip_suffix(')').unwrap_or(text),
        Some('[') => text.strip_suffix(']').unwrap_or(text),
        _ => text,
    };
    let text = text.trim_start_matches(|c: char| c.is_whitespace() || "-*•>|([,;–—".contains(c));
    // Without `:` or `=` only a MEGA key (checked below) or "Bearer <token>" is one: "Password
    // protected" and "Authorization required" are prose.
    let (value, label, spaced) = LABELS.iter().find_map(|(name, kind)| {
        let (value, spaced) = after_separator(strip_label(text, name)?)?;
        (!spaced || matches!(kind, Label::Key) || *name == "bearer").then_some((value, *kind, spaced))
    })?;
    let quoted = || {
        let open = value.chars().next().filter(|c| "\"'`“‘".contains(*c))?;
        let close = match open {
            '“' => '”',
            '‘' => '’',
            c => c,
        };
        let inner = &value[open.len_utf8()..];
        inner.find(close).map(|end| inner[..end].to_string())
    };
    let whole = quoted().unwrap_or_else(|| value.to_string());
    let mut words = value.split_whitespace();
    let word = quoted().or_else(|| words.next().map(str::to_string)).unwrap_or_default();
    if known_token(&word).is_some() || word.is_empty() || whole.contains(char::is_control) {
        return None;
    }
    let secrets = match label {
        Label::Password if NO_PASSWORD.contains(&word.to_lowercase().as_str()) => return None,
        Label::Password => Secrets { password: Some(word), ..Default::default() },
        Label::Key => {
            let key = word.trim_start_matches(['#', '!']);
            return is_mega_key(key).then(|| (Some(Service::Mega), Secrets { key: Some(key.into()), ..Default::default() }));
        }
        Label::Token => {
            // "token: Bearer x" keeps its scheme; a bare token is a bearer token.
            let schemed = ["bearer", "basic", "token", "digest"].contains(&word.to_lowercase().as_str());
            let (scheme, token) = match words.next() {
                Some(token) if schemed => (word.as_str(), token),
                _ => ("Bearer", word.as_str()),
            };
            let token_chars = |c: char| c.is_ascii_alphanumeric() || "-._~+/=".contains(c);
            // "Bearer x" without a separator: only what looks like a token, not a word.
            let least = if spaced { 16 } else { 8 };
            let word_like = spaced && !token.contains(|c: char| c.is_ascii_digit());
            if token.len() < least || word_like || !token.chars().all(token_chars) || known_token(token).is_some() {
                return None;
            }
            Secrets { auth_header: Some(format!("{scheme} {token}")), ..Default::default() }
        }
        Label::Authorization => Secrets { auth_header: Some(whole), ..Default::default() },
        Label::Cookie if whole.contains('=') => Secrets { cookies: Some(whole), ..Default::default() },
        Label::Cookie => return None,
    };
    Some((None, secrets))
}

/// `text` after `label`, which it starts with, case aside.
fn strip_label<'a>(text: &'a str, label: &str) -> Option<&'a str> {
    let end = text.char_indices().nth(label.chars().count()).map_or(text.len(), |(i, _)| i);
    (text[..end].to_lowercase() == label).then(|| &text[end..])
}

/// What follows a label's separator: `:` or `=`, or whitespace before a lone word ("Bearer x"),
/// and whether it was that whitespace.
fn after_separator(rest: &str) -> Option<(&str, bool)> {
    let trimmed = rest.trim_start();
    if let Some(value) = trimmed.strip_prefix([':', '=', '：']) {
        return Some((value.trim(), false));
    }
    (trimmed.len() < rest.len() && trimmed.split_whitespace().count() == 1).then(|| (trimmed.trim(), true))
}

/// A tool whose copied requests a paste may hold.
#[derive(Clone, Copy, PartialEq)]
enum Tool {
    Curl,
    Wget,
    Fetch,
    PowerShell,
}

/// The tool whose command `line` starts, if it starts one.
fn tool_of(line: &str) -> Option<Tool> {
    let line = line.trim_start().to_ascii_lowercase();
    let command = |name: &str| line.strip_prefix(name).is_some_and(|rest| rest.starts_with(char::is_whitespace));
    let session = line.strip_prefix("$session").is_some_and(|rest| rest.trim_start().starts_with('='));
    if command("curl") || command("curl.exe") {
        Some(Tool::Curl)
    } else if command("wget") || command("wget.exe") {
        Some(Tool::Wget)
    } else if line.starts_with("fetch(") || line.starts_with("await fetch(") {
        Some(Tool::Fetch)
    } else if session || ["invoke-webrequest", "iwr", "invoke-restmethod", "irm"].into_iter().any(command) {
        Some(Tool::PowerShell)
    } else {
        None
    }
}

/// The copied requests in `text`, when it starts with one: a line that starts a command starts
/// the next request, but for PowerShell's `Invoke-WebRequest` after the lines of its `$session`.
fn requests(text: &str) -> Option<Vec<(Tool, String)>> {
    tool_of(text.lines().find(|l| !l.trim().is_empty())?)?;
    // Each request, and whether a PowerShell one has its `Invoke-WebRequest` yet.
    let mut requests: Vec<(Tool, String, bool)> = Vec::new();
    for line in text.lines() {
        let invokes = !line.trim_start().starts_with('$');
        match (tool_of(line), requests.last_mut()) {
            (Some(Tool::PowerShell), Some((Tool::PowerShell, command, invoked))) if invokes && !*invoked => {
                *invoked = true;
                command.push_str(line);
                command.push('\n');
            }
            (Some(tool), _) => requests.push((tool, format!("{line}\n"), invokes)),
            (None, Some((_, command, _))) => {
                command.push_str(line);
                command.push('\n');
            }
            (None, None) => {}
        }
    }
    Some(requests.into_iter().map(|(tool, command, _)| (tool, command)).collect())
}

/// The link a copied `command` of `tool` downloads, with what it sends; None when it names no
/// http(s) link.
fn request(tool: Tool, command: &str) -> Result<Option<PastedLink>, String> {
    let mut request = Request::default();
    match tool {
        Tool::Curl | Tool::Wget => request.command(tool == Tool::Curl, &words(command))?,
        Tool::Fetch => request.fetch(command)?,
        Tool::PowerShell => request.powershell(command)?,
    }
    if request.user.is_some() || request.pass.is_some() {
        let pair = format!("{}:{}", request.user.unwrap_or_default(), request.pass.unwrap_or_default());
        request.secrets.auth_header.get_or_insert_with(|| basic(&pair));
    }
    Ok(request.url.map(|url| link(url, request.secrets)))
}

/// What a copied request says.
#[derive(Default)]
struct Request {
    url: Option<Url>,
    secrets: Secrets,
    /// The user name and password of curl's `-u` or wget's `--user` and `--password`.
    user: Option<String>,
    pass: Option<String>,
}

impl Request {
    /// The request's link, the first http(s) one.
    fn target(&mut self, word: &str) {
        if self.url.is_none() {
            self.url = http_url(word);
        }
    }

    /// A header the request sends, kept unless the download sets it itself or it belongs to the
    /// copied connection (see [`DROPPED`]), or it is none a request can carry (a control
    /// character in it, say).
    fn header(&mut self, name: &str, value: &str) {
        let (name, value) = (name.trim(), value.trim());
        let lower = name.to_ascii_lowercase();
        let dropped = DROPPED.contains(&lower.as_str()) || lower.starts_with("if-") || lower.starts_with("sec-fetch-");
        let unusable = value.is_empty()
            || format!("{name}{value}").contains(char::is_control)
            || crate::engine::request_header(name, value).is_none();
        if dropped || unusable {
            return;
        }
        let s = &mut self.secrets;
        match lower.as_str() {
            "authorization" => s.auth_header = Some(value.to_string()),
            "referer" => s.referer = Some(value.to_string()),
            "cookie" => s.cookies = Some(s.cookies.take().map_or_else(|| value.to_string(), |c| format!("{c}; {value}"))),
            _ => {
                s.headers.retain(|(n, _)| !n.eq_ignore_ascii_case(name));
                s.headers.push((name.to_string(), value.to_string()));
            }
        }
    }

    /// The options of a curl (`curl`) or wget command line, `words` with its name first, that say
    /// where the request goes and what it sends. Options it does not know are skipped.
    fn command(&mut self, curl: bool, words: &[String]) -> Result<(), String> {
        let mut words = words.iter().skip(1).map(String::as_str);
        while let Some(word) = words.next() {
            // `--name=value` (wget's way) and curl's `-Xvalue` as `--name value` and `-X value`.
            let short = curl && word.len() > 2 && !word.starts_with("--") && word.starts_with('-');
            let (flag, inline) = match word.split_once('=') {
                Some((flag, value)) if word.starts_with("--") => (flag, Some(value)),
                _ if short && b"HbuAeXdFTx".contains(&word.as_bytes()[1]) => (&word[..2], Some(&word[2..])),
                _ => (word, None),
            };
            let mut value = || inline.or_else(|| words.next()).unwrap_or_default().to_string();
            match (curl, flag) {
                (_, "--header") | (true, "-H") => {
                    if let Some((name, v)) = value().split_once(':') {
                        self.header(name, v);
                    }
                }
                (true, "-b" | "--cookie") => {
                    // A value without `=` is a cookies file, which stays where it is.
                    let cookies = value();
                    if cookies.contains('=') {
                        self.header("Cookie", &cookies);
                    }
                }
                (true, "-u" | "--user") => {
                    let pair = value();
                    let (user, pass) = pair.split_once(':').unwrap_or((pair.as_str(), ""));
                    (self.user, self.pass) = (Some(user.to_string()), Some(pass.to_string()));
                }
                (false, "--user" | "--http-user") => self.user = Some(value()),
                (false, "--password" | "--http-password") => self.pass = Some(value()),
                (true, "-A") | (false, "-U") | (_, "--user-agent") => self.header("User-Agent", &value()),
                (true, "-e") | (_, "--referer") => {
                    let referer = value();
                    self.header("Referer", referer.strip_suffix(";auto").unwrap_or(&referer));
                }
                (true, "--oauth2-bearer") => self.header("Authorization", &format!("Bearer {}", value())),
                (true, "--url") => self.target(&value()),
                (true, "-X" | "--request") | (false, "--method") => {
                    if !matches!(value().to_ascii_uppercase().as_str(), "GET" | "HEAD") {
                        return Err(SENDS_DATA.to_string());
                    }
                }
                (true, "-d" | "-F" | "-T" | "--json" | "--form" | "--form-string" | "--upload-file")
                | (false, "--post-data" | "--post-file" | "--body-data" | "--body-file") => return Err(SENDS_DATA.to_string()),
                (true, flag) if flag.starts_with("--data") => return Err(SENDS_DATA.to_string()),
                // Options whose values could pass for the link.
                (true, "-x" | "--proxy" | "--preproxy" | "--doh-url") => {
                    value();
                }
                (_, flag) if flag.starts_with('-') => {}
                (_, word) => self.target(word),
            }
        }
        Ok(())
    }

    /// DevTools' "Copy as fetch": `fetch("<url>", {"headers": {…}, "referrer": "…", "body": null,
    /// "method": "GET", …});`, the object as JSON.
    fn fetch(&mut self, command: &str) -> Result<(), String> {
        let unreadable = || UNREADABLE_FETCH.to_string();
        let args = &command[command.find("fetch(").ok_or_else(unreadable)? + "fetch(".len()..];
        let mut values = serde_json::Deserializer::from_str(args).into_iter::<Value>();
        let Some(Ok(Value::String(url))) = values.next() else { return Err(unreadable()) };
        self.target(&url);
        let Some(rest) = args[values.byte_offset()..].trim_start().strip_prefix(',') else { return Ok(()) };
        let Some(Ok(Value::Object(init))) = serde_json::Deserializer::from_str(rest).into_iter::<Value>().next() else {
            return Err(unreadable());
        };
        let method = init.get("method").and_then(Value::as_str).unwrap_or("GET");
        let body = init.get("body").is_some_and(|body| !body.is_null());
        if body || !(method.eq_ignore_ascii_case("GET") || method.eq_ignore_ascii_case("HEAD")) {
            return Err(SENDS_DATA.to_string());
        }
        for (name, value) in init.get("headers").and_then(Value::as_object).into_iter().flatten() {
            self.header(name, value.as_str().unwrap_or_default());
        }
        if let Some(referrer) = init.get("referrer").and_then(Value::as_str).filter(|r| http_url(r).is_some()) {
            self.header("Referer", referrer);
        }
        Ok(())
    }

    /// DevTools' "Copy as PowerShell": a `$session` with its user agent and cookies
    /// (`$session.Cookies.Add((New-Object System.Net.Cookie("name", "value", …)))`), then
    /// `Invoke-WebRequest -Uri "…" -WebSession $session -Headers @{"name"="value" …}`.
    fn powershell(&mut self, command: &str) -> Result<(), String> {
        let tokens = ps_tokens(command);
        let text = |i: usize| match tokens.get(i) {
            Some(Token::Text(t)) => Some(t.as_str()),
            _ => None,
        };
        let sym = |i: usize, c: char| matches!(tokens.get(i), Some(Token::Sym(s)) if *s == c);
        let mut i = 0;
        while i < tokens.len() {
            let word = text(i).unwrap_or_default().to_ascii_lowercase();
            i += 1;
            match word.as_str() {
                "-uri" => self.url = text(i).and_then(http_url).or(self.url.take()),
                "-method" if !matches!(text(i).map(str::to_ascii_uppercase).as_deref(), Some("GET" | "HEAD")) => {
                    return Err(SENDS_DATA.to_string());
                }
                "-body" | "-infile" => return Err(SENDS_DATA.to_string()),
                "-useragent" => self.header("User-Agent", text(i).unwrap_or_default()),
                "$session.useragent" if sym(i, '=') => self.header("User-Agent", text(i + 1).unwrap_or_default()),
                "-headers" if sym(i, '{') => {
                    i += 1;
                    while let (Some(name), true, Some(value)) = (text(i), sym(i + 1, '='), text(i + 2)) {
                        self.header(name, value);
                        i += if sym(i + 3, ';') { 4 } else { 3 };
                    }
                }
                "system.net.cookie" if sym(i, '(') && sym(i + 2, ',') => {
                    if let (Some(name), Some(value)) = (text(i + 1), text(i + 3)) {
                        self.header("Cookie", &format!("{name}={value}"));
                    }
                }
                "-proxy" | "-outfile" => i += 1,
                word => self.target(word),
            }
        }
        Ok(())
    }
}

/// The words a shell hands a program for `command`: cmd's quoting when it has cmd's `^` escapes
/// (DevTools' "Copy as cURL (cmd)"), else POSIX shell quoting.
fn words(command: &str) -> Vec<String> {
    if command.contains("^\"") || command.contains("^\n") {
        cmd_words(command)
    } else {
        posix_words(command)
    }
}

/// POSIX shell words: `'…'`, `"…"` with its `\` escapes, `$'…'`, `\` escapes and line
/// continuations outside quotes.
fn posix_words(command: &str) -> Vec<String> {
    let (mut words, mut word) = (Vec::new(), None::<String>);
    let mut chars = command.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => words.extend(word.take()),
            '\\' => match chars.next() {
                Some('\n') | None => {}
                Some(c) => word.get_or_insert_default().push(c),
            },
            '\'' => {
                let w = word.get_or_insert_default();
                w.extend(chars.by_ref().take_while(|&c| c != '\''));
            }
            '"' => {
                let w = word.get_or_insert_default();
                while let Some(c) = chars.next() {
                    match c {
                        '"' => break,
                        '\\' => match chars.next() {
                            Some('\n') => {}
                            Some(c @ ('"' | '\\' | '$' | '`')) => w.push(c),
                            Some(c) => w.extend(['\\', c]),
                            None => w.push('\\'),
                        },
                        c => w.push(c),
                    }
                }
            }
            '$' if chars.peek() == Some(&'\'') => {
                chars.next();
                ansi_c(&mut chars, word.get_or_insert_default());
            }
            c => word.get_or_insert_default().push(c),
        }
    }
    words.extend(word);
    words
}

/// The rest of a `$'…'` string into `out`: `\n`, `\r`, `\t`, `\xHH` and `\uHHHH` as what they
/// stand for, any other escaped character as itself.
fn ansi_c(chars: &mut Peekable<Chars<'_>>, out: &mut String) {
    while let Some(c) = chars.next() {
        match c {
            '\'' => return,
            '\\' => match chars.next() {
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some('t') => out.push('\t'),
                Some(x @ ('x' | 'u')) => {
                    let digits: String = chars.clone().take(if x == 'x' { 2 } else { 4 }).take_while(char::is_ascii_hexdigit).collect();
                    chars.by_ref().take(digits.len()).for_each(drop);
                    out.extend(u32::from_str_radix(&digits, 16).ok().and_then(char::from_u32));
                }
                Some(c) => out.push(c),
                None => return,
            },
            c => out.push(c),
        }
    }
}

/// cmd's words: `^` escapes the character after it (a line break: the line goes on), then the C
/// runtime splits the line: `"` quotes, `\"` is a quote, backslashes before a quote halve.
fn cmd_words(command: &str) -> Vec<String> {
    let mut line = String::new();
    let mut chars = command.chars();
    while let Some(c) = chars.next() {
        match c {
            '^' => line.extend(chars.next().filter(|&c| c != '\n')),
            c => line.push(c),
        }
    }
    let (mut words, mut word, mut quoted) = (Vec::new(), None::<String>, false);
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                let mut n = 1;
                while chars.next_if_eq(&'\\').is_some() {
                    n += 1;
                }
                let w = word.get_or_insert_default();
                let quote = chars.peek() == Some(&'"');
                w.extend(std::iter::repeat_n('\\', if quote { n / 2 } else { n }));
                if quote && n % 2 == 1 {
                    chars.next();
                    w.push('"');
                }
            }
            '"' => {
                quoted = !quoted;
                word.get_or_insert_default();
            }
            c if c.is_whitespace() && !quoted => words.extend(word.take()),
            c => word.get_or_insert_default().push(c),
        }
    }
    words.extend(word);
    words
}

/// A piece of a PowerShell command: a word or string, or one of `{ } ( ) = ; ,`.
enum Token {
    Text(String),
    Sym(char),
}

/// The pieces of a PowerShell command: strings unquoted (`` ` `` escapes and `""` in double
/// quotes, `''` in single ones), `` ` `` line continuations dropped, `@{` as `{`.
fn ps_tokens(command: &str) -> Vec<Token> {
    const SYMS: &str = "{}()=;,";
    let mut tokens = Vec::new();
    let mut chars = command.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '`' => {
                chars.next();
            }
            '"' | '\'' => {
                let mut s = String::new();
                while let Some(ch) = chars.next() {
                    match ch {
                        '`' if c == '"' => match chars.next() {
                            Some('n') => s.push('\n'),
                            Some('r') => s.push('\r'),
                            Some('t') => s.push('\t'),
                            Some('0') => s.push('\0'),
                            Some(e) => s.push(e),
                            None => {}
                        },
                        ch if ch == c && chars.next_if_eq(&c).is_some() => s.push(c),
                        ch if ch == c => break,
                        ch => s.push(ch),
                    }
                }
                tokens.push(Token::Text(s));
            }
            '@' if chars.peek() == Some(&'{') => {}
            c if SYMS.contains(c) => tokens.push(Token::Sym(c)),
            c if c.is_whitespace() => {}
            c => {
                let mut word = c.to_string();
                while let Some(ch) = chars.next_if(|ch| !ch.is_whitespace() && !SYMS.contains(*ch) && !"\"'`".contains(*ch)) {
                    word.push(ch);
                }
                tokens.push(Token::Text(word));
            }
        }
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINK: &str = "https://h.example/f.rar";
    const FILE_KEY: &str = "0123456789abcdefghijklmnopqrstuvwxyzABCDE-_";
    const FOLDER_KEY: &str = "AbCdEfGhIjKlMnOpQrStU_";

    fn links(text: &str) -> Vec<PastedLink> {
        parse(text).unwrap_or_else(|| panic!("no secrets found in {text:?}")).unwrap_or_else(|e| panic!("{e}: {text:?}"))
    }

    fn one(text: &str) -> PastedLink {
        let mut links = links(text);
        assert_eq!(links.len(), 1, "{text:?}: {links:?}");
        links.remove(0)
    }

    fn password(p: &str) -> Secrets {
        Secrets { password: Some(p.into()), ..Default::default() }
    }

    fn auth(a: &str) -> Secrets {
        Secrets { auth_header: Some(a.into()), ..Default::default() }
    }

    fn headers(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(n, v)| (n.to_string(), v.to_string())).collect()
    }

    #[test]
    fn userinfo_becomes_basic_credentials_and_leaves_the_link() {
        for (text, url, pair) in [
            ("https://alice:s3cret@h.example/f.zip", "https://h.example/f.zip", "alice:s3cret"),
            ("Get it: https://al%20ice:p%40ss%3Aw@h.example/a?b=1.", "https://h.example/a?b=1", "al ice:p@ss:w"),
            ("<https://user:p@ss@h.example/f>", "https://h.example/f", "user:p@ss"),
            ("https://ghtoken@github.com/o/r/archive/main.zip", "https://github.com/o/r/archive/main.zip", "ghtoken:"),
            ("http://u:@h.example:8080/f", "http://h.example:8080/f", "u:"),
        ] {
            let link = one(text);
            assert_eq!(link.url.as_str(), url, "{text}");
            assert_eq!(link.secrets, auth(&basic(pair)), "{text}");
        }
    }

    #[test]
    fn labels_give_their_secrets_in_every_language() {
        for (line, secret) in [
            ("Password: hunter2", "hunter2"),
            ("password=hunter2", "hunter2"),
            ("PASSWORD : hunter2", "hunter2"),
            ("Pass: hunter2", "hunter2"),
            ("pw: x1", "x1"),
            ("PWD = 'with space'", "with space"),
            ("passwd: \"q\"", "q"),
            ("Parola: ciao123", "ciao123"),
            ("Contraseña: hola", "hola"),
            ("CONTRASEÑA = hola", "hola"),
            ("Mot de passe : bonjour", "bonjour"),
            ("- **Password:** hunter2", "hunter2"),
            ("Password: `hunter2` (case sensitive)", "hunter2"),
            ("Password: www.site.example", "www.site.example"),
        ] {
            assert_eq!(one(&format!("{LINK}\n{line}")).secrets, password(secret), "{line}");
        }
        // After the link on its line.
        for text in [format!("{LINK} password: hunter2"), format!("{LINK} | pw=hunter2"), format!("{LINK} (password: hunter2)")] {
            assert_eq!(one(&text).secrets, password("hunter2"), "{text}");
        }
        for (line, header) in [
            ("Token: abcdef123456", "Bearer abcdef123456"),
            ("API key = abcdef123456", "Bearer abcdef123456"),
            ("apikey: abcdef123456", "Bearer abcdef123456"),
            ("Access token: abcdef123456", "Bearer abcdef123456"),
            ("Bearer abcdef1234567890", "Bearer abcdef1234567890"),
            ("bearer: abcdef123456", "Bearer abcdef123456"),
            ("token: Bearer abcdef123456", "Bearer abcdef123456"),
            ("token: token abcdef123456", "token abcdef123456"),
            ("Authorization: Basic dXNlcjpwYXNz", "Basic dXNlcjpwYXNz"),
        ] {
            assert_eq!(one(&format!("{LINK}\n{line}")).secrets, auth(header), "{line}");
        }
        let cookies = Secrets { cookies: Some("session=abc; theme=dark".into()), ..Default::default() };
        assert_eq!(one(&format!("{LINK}\nCookie: session=abc; theme=dark")).secrets, cookies);
        for line in [format!("Key: {FILE_KEY}"), format!("Decryption key: {FILE_KEY}"), format!("MEGA key = #{FILE_KEY}"), format!("key {FILE_KEY}")] {
            let link = one(&format!("https://mega.nz/file/AbCdEfGh\n{line}"));
            assert_eq!(link.url.as_str(), format!("https://mega.nz/file/AbCdEfGh#{FILE_KEY}"), "{line}");
            assert_eq!(link.secrets, Secrets { key: Some(FILE_KEY.into()), ..Default::default() }, "{line}");
        }
    }

    #[test]
    fn known_tokens_go_only_to_their_service() {
        let ghp = format!("ghp_{}", "A1b2".repeat(9));
        let pat = format!("github_pat_{}", "A1_b".repeat(20));
        let hf = format!("hf_{}xy", "aBcD".repeat(8));
        let glpat = format!("glpat-{}", "x1Y2-z3_".repeat(3));
        let bearer = |t: &str| auth(&format!("Bearer {t}"));
        let gitlab = Secrets { headers: headers(&[("PRIVATE-TOKEN", &glpat)]), ..Default::default() };
        for (link, token, secrets) in [
            ("https://github.com/o/r/releases/download/v1/a.zip", &ghp, bearer(&ghp)),
            ("https://api.github.com/repos/o/r/zipball", &format!("gho_{}", "Z9y8".repeat(9)), bearer(&format!("gho_{}", "Z9y8".repeat(9)))),
            ("https://raw.githubusercontent.com/o/r/main/f.txt", &pat, bearer(&pat)),
            ("https://objects.githubusercontent.com/x/y", &ghp, bearer(&ghp)),
            ("https://codeload.github.com/o/r/zip/main", &ghp, bearer(&ghp)),
            ("https://huggingface.co/o/m/resolve/main/model.bin", &hf, bearer(&hf)),
            ("https://cdn-lfs.hf.co/x", &hf, bearer(&hf)),
            ("https://gitlab.com/api/v4/projects/1/repository/archive.zip", &glpat, gitlab.clone()),
            ("https://gitlab.example.org/g/p/-/raw/main/f", &glpat, gitlab.clone()),
        ] {
            for text in [format!("{link}\n{token}"), format!("{link} {token}"), format!("token: {token}\n{link}")] {
                assert_eq!(one(&text).secrets, secrets, "{text}");
            }
        }
        for text in [
            format!("https://huggingface.co/o/m {ghp}"),
            format!("https://github.com/o/r\n{hf}"),
            format!("{LINK}\n{glpat}"),
            format!("https://github.com.evil.example/f {ghp}"),
            format!("https://notgithub.com/f {ghp}"),
            format!("Token: {ghp}\n{LINK}"),
            format!("{LINK}\nghp_short"),
        ] {
            assert_eq!(parse(&text), None, "{text}");
        }
    }

    #[test]
    fn mega_keys_join_links_that_lack_them() {
        for (text, url) in [
            (format!("https://mega.nz/file/AbCdEfGh\n#{FILE_KEY}"), format!("https://mega.nz/file/AbCdEfGh#{FILE_KEY}")),
            (format!("https://mega.nz/folder/AbCdEfGh #{FOLDER_KEY}"), format!("https://mega.nz/folder/AbCdEfGh#{FOLDER_KEY}")),
            (format!("https://mega.nz/#!AbCdEfGh\n!{FILE_KEY}"), format!("https://mega.nz/#!AbCdEfGh!{FILE_KEY}")),
            (format!("https://mega.co.nz/#F!AbCdEfGh\nKey: {FOLDER_KEY}"), format!("https://mega.co.nz/#F!AbCdEfGh!{FOLDER_KEY}")),
            (format!("Decryption key: {FILE_KEY}\nhttps://mega.nz/file/AbCdEfGh"), format!("https://mega.nz/file/AbCdEfGh#{FILE_KEY}")),
        ] {
            let link = one(&text);
            assert_eq!(link.url.as_str(), url, "{text}");
            assert!(crate::mega::handles(&link.url), "{url}");
        }
        // A link with its key keeps it, and a key goes to no other link.
        let keyed = format!("https://mega.nz/file/AbCdEfGh#{FILE_KEY}");
        for text in [format!("{keyed}\nKey: {}", FILE_KEY.to_lowercase()), format!("{LINK}\n#{FILE_KEY}"), format!("{LINK}\nKey: {FILE_KEY}")] {
            assert_eq!(parse(&text), None, "{text}");
        }
    }

    #[test]
    fn copied_curl_commands_give_their_link_and_headers() {
        let bash = r#"curl 'https://files.example.com/dl/report.pdf?id=7' \
  -H 'accept: text/html,application/xhtml+xml' \
  -H 'accept-language: en-US,en;q=0.9' \
  -b 'session=abc123; theme=dark' \
  -H 'priority: u=0, i' \
  -H 'referer: https://files.example.com/list' \
  -H $'x-note: it\'s !fine' \
  -H 'sec-ch-ua: "Chromium";v="128", "Not;A=Brand";v="24"' \
  -H 'sec-fetch-mode: navigate' \
  -H 'user-agent: Mozilla/5.0 Test' \
  -H 'x-api-key: k-1'\''2' \
  --compressed"#;
        let link = one(bash);
        assert_eq!(link.url.as_str(), "https://files.example.com/dl/report.pdf?id=7");
        let expected = Secrets {
            cookies: Some("session=abc123; theme=dark".into()),
            referer: Some("https://files.example.com/list".into()),
            headers: headers(&[
                ("accept", "text/html,application/xhtml+xml"),
                ("accept-language", "en-US,en;q=0.9"),
                ("x-note", "it's !fine"),
                ("sec-ch-ua", r#""Chromium";v="128", "Not;A=Brand";v="24""#),
                ("user-agent", "Mozilla/5.0 Test"),
                ("x-api-key", "k-1'2"),
            ]),
            ..Default::default()
        };
        assert_eq!(link.secrets, expected);

        let cmd = "curl ^\"https://files.example.com/dl/my^%^20report.pdf?id=7^&x=1^\" ^\n  -H ^\"accept: text/html^\" ^\n  -b ^\"session=abc123; theme=dark^\" ^\n  -H ^\"sec-ch-ua: ^\\^\"Chromium^\\^\";v=^\\^\"128^\\^\"^\" ^\n  -H ^\"authorization: Bearer tok123456^\" ^\n  --compressed";
        let link = one(cmd);
        assert_eq!(link.url.as_str(), "https://files.example.com/dl/my%20report.pdf?id=7&x=1");
        let expected = Secrets {
            auth_header: Some("Bearer tok123456".into()),
            cookies: Some("session=abc123; theme=dark".into()),
            headers: headers(&[("accept", "text/html"), ("sec-ch-ua", r#""Chromium";v="128""#)]),
            ..Default::default()
        };
        assert_eq!(link.secrets, expected);

        let typed = "curl -L -o out.zip -sS -x http://proxy.example:8080 -u 'bob:pa ss' -A 'Agent/1' -e https://ref.example/ --url https://h.example/a.zip";
        let link = one(typed);
        assert_eq!(link.url.as_str(), "https://h.example/a.zip");
        let expected = Secrets {
            auth_header: Some(basic("bob:pa ss")),
            referer: Some("https://ref.example/".into()),
            headers: headers(&[("User-Agent", "Agent/1")]),
            ..Default::default()
        };
        assert_eq!(link.secrets, expected);
        assert_eq!(one("curl -H 'Authorization: Bearer tok123456' -u bob:x https://h.example/a").secrets, auth("Bearer tok123456"));
        assert_eq!(one("curl --oauth2-bearer tok123456 https://h.example/a").secrets, auth("Bearer tok123456"));
        // A command is the link even with nothing secret in it, and a file of cookies stays put.
        assert_eq!(one("curl -L -b cookies.txt -X GET https://h.example/a.zip").secrets, Secrets::default());
        // "Copy all as cURL": one link per command.
        let all = "curl 'https://h.example/a' -H 'x-a: 1' ;\ncurl 'https://h.example/b' -H 'x-b: 2' ;";
        let urls: Vec<String> = links(all).iter().map(|l| l.url.to_string()).collect();
        assert_eq!(urls, ["https://h.example/a", "https://h.example/b"]);
    }

    #[test]
    fn copied_wget_fetch_and_powershell_requests_give_their_link_and_headers() {
        let wget = "wget --header='Authorization: Bearer tok123456' --header \"X-Api-Key: k1\" --referer=https://ref.example/ -U 'Agent/2' -O out.bin https://h.example/a.bin";
        let link = one(wget);
        assert_eq!(link.url.as_str(), "https://h.example/a.bin");
        let expected = Secrets {
            auth_header: Some("Bearer tok123456".into()),
            referer: Some("https://ref.example/".into()),
            headers: headers(&[("X-Api-Key", "k1"), ("User-Agent", "Agent/2")]),
            ..Default::default()
        };
        assert_eq!(link.secrets, expected);
        assert_eq!(one("wget --http-user=bob --http-password='p w' https://h.example/a").secrets, auth(&basic("bob:p w")));
        assert_eq!(one("wget --user bob --password pw https://h.example/a").secrets, auth(&basic("bob:pw")));

        let chrome = r#"fetch("https://files.example.com/dl/a.zip", {
  "headers": {
    "accept": "*/*",
    "authorization": "Bearer tok123456",
    "cookie": "sid=1",
    "sec-fetch-dest": "empty",
    "x-csrf-token": "c\"q"
  },
  "referrer": "https://files.example.com/",
  "referrerPolicy": "strict-origin-when-cross-origin",
  "body": null,
  "method": "GET",
  "mode": "cors",
  "credentials": "include"
});"#;
        let link = one(chrome);
        assert_eq!(link.url.as_str(), "https://files.example.com/dl/a.zip");
        let expected = Secrets {
            auth_header: Some("Bearer tok123456".into()),
            cookies: Some("sid=1".into()),
            referer: Some("https://files.example.com/".into()),
            headers: headers(&[("accept", "*/*"), ("x-csrf-token", "c\"q")]),
            ..Default::default()
        };
        assert_eq!(link.secrets, expected);
        let firefox = r#"await fetch("https://files.example.com/dl/a.zip", {
    "credentials": "include",
    "headers": {
        "User-Agent": "Mozilla/5.0 Test",
        "Accept": "*/*"
    },
    "referrer": "https://files.example.com/",
    "method": "GET",
    "mode": "cors"
});"#;
        let expected = Secrets {
            referer: Some("https://files.example.com/".into()),
            // In the order of their names, as JSON objects are read.
            headers: headers(&[("Accept", "*/*"), ("User-Agent", "Mozilla/5.0 Test")]),
            ..Default::default()
        };
        assert_eq!(one(firefox).secrets, expected);
        assert_eq!(one("fetch(\"https://h.example/a\");").url.as_str(), "https://h.example/a");
        assert_eq!(parse("fetch(\"https://h.example/a\", {headers: {}});"), Some(Err(UNREADABLE_FETCH.to_string())));

        let powershell = "$session = New-Object Microsoft.PowerShell.Commands.WebRequestSession
$session.UserAgent = \"Mozilla/5.0 Test\"
$session.Cookies.Add((New-Object System.Net.Cookie(\"sid\", \"abc\", \"/\", \"files.example.com\")))
$session.Cookies.Add((New-Object System.Net.Cookie(\"theme\", \"dark\", \"/\", \".example.com\")))
Invoke-WebRequest -UseBasicParsing -Uri \"https://files.example.com/dl/a.zip\" `
-WebSession $session `
-Headers @{
\"authority\"=\"files.example.com\"
  \"method\"=\"GET\"
  \"path\"=\"/dl/a.zip\"
  \"scheme\"=\"https\"
  \"accept\"=\"*/*\"
  \"authorization\"=\"Bearer tok123456\"
  \"referer\"=\"https://files.example.com/\"
  \"sec-ch-ua\"=\"`\"Chromium`\";v=`\"128`\"\"
}";
        let link = one(powershell);
        assert_eq!(link.url.as_str(), "https://files.example.com/dl/a.zip");
        let expected = Secrets {
            auth_header: Some("Bearer tok123456".into()),
            cookies: Some("sid=abc; theme=dark".into()),
            referer: Some("https://files.example.com/".into()),
            headers: headers(&[("User-Agent", "Mozilla/5.0 Test"), ("accept", "*/*"), ("sec-ch-ua", r#""Chromium";v="128""#)]),
            ..Default::default()
        };
        assert_eq!(link.secrets, expected);
        let firefox = "Invoke-WebRequest -Uri 'https://h.example/a' -Headers @{ 'X-Api-Key' = 'it''s'; \"Accept\" = \"*/*\" } -UserAgent \"Agent/3\"";
        let expected = Secrets { headers: headers(&[("X-Api-Key", "it's"), ("Accept", "*/*"), ("User-Agent", "Agent/3")]), ..Default::default() };
        assert_eq!(one(firefox).secrets, expected);
    }

    #[test]
    fn requests_that_send_data_are_refused() {
        for text in [
            "curl 'https://h.example/api' --data-raw '{\"a\":1}'",
            "curl https://h.example/api -d a=1",
            "curl -X POST https://h.example/api",
            "curl -XPOST https://h.example/api",
            "curl --request put https://h.example/api",
            "curl https://h.example/up -F 'f=@x.bin'",
            "curl -T x.bin https://h.example/up",
            "curl --json '{}' https://h.example/api",
            "wget --post-data='a=1' https://h.example/api",
            "wget --method=PUT https://h.example/api",
            "fetch(\"https://h.example/api\", {\"body\": \"a=1\", \"method\": \"POST\"});",
            "fetch(\"https://h.example/api\", {\"body\": \"a=1\"});",
            "fetch(\"https://h.example/api\", {\"method\": \"DELETE\"});",
            "Invoke-WebRequest -Uri \"https://h.example/api\" -Method \"POST\" -Body \"a=1\"",
            "Invoke-WebRequest -Uri \"https://h.example/api\" -Method Post",
            "iwr -Uri https://h.example/api -InFile x.bin",
        ] {
            assert_eq!(parse(text), Some(Err(SENDS_DATA.to_string())), "{text}");
        }
        for text in ["curl -X GET https://h.example/f", "curl -I https://h.example/f", "wget --method=get https://h.example/f"] {
            assert_eq!(one(text).url.as_str(), "https://h.example/f", "{text}");
        }
        // Downloads copied together with requests that send data are kept.
        let urls: Vec<String> = links("curl https://h.example/a\ncurl https://h.example/api -d x=1").iter().map(|l| l.url.to_string()).collect();
        assert_eq!(urls, ["https://h.example/a"]);
    }

    #[test]
    fn headers_the_download_sets_itself_or_cannot_send_are_dropped() {
        for header in [
            "Host: h.example",
            "Content-Length: 3",
            "Connection: keep-alive",
            "Accept-Encoding: br",
            "Range: bytes=0-",
            "If-None-Match: \"e\"",
            "If-Modified-Since: Sun, 06 Nov 1994 08:49:37 GMT",
            "Sec-Fetch-Site: same-origin",
            "Priority: u=0, i",
            ":authority: h.example",
            "Bad Name: 1",
            "X-Empty:",
        ] {
            let text = format!("curl https://h.example/f -H '{header}' -H 'X-Kept: 1'");
            assert_eq!(one(&text).secrets.headers, headers(&[("X-Kept", "1")]), "{header}");
        }
        // Control characters: refused wherever they are.
        for text in [
            r"curl https://h.example/f -H $'X-Bad: a\nb' -H 'X-Kept: 1'",
            r"curl https://h.example/f -H $'X-Bad: a\u0007b' -H 'X-Kept: 1'",
            r"curl https://h.example/f -H $'X-B\tad: 1' -H 'X-Kept: 1'",
            "curl ^\"https://h.example/f^\" -H ^\"X-Bad: a^\n\nb^\" -H ^\"X-Kept: 1^\"",
        ] {
            assert_eq!(one(text).secrets, Secrets { headers: headers(&[("X-Kept", "1")]), ..Default::default() }, "{text}");
        }
        assert_eq!(one(r"curl https://h.example/f -b $'a=1\r\nX-Evil: y'").secrets, Secrets::default());
        assert_eq!(one(r"curl https://h.example/f -H $'Authorization: Bearer a\nb'").secrets, Secrets::default());
        assert_eq!(parse(&format!("{LINK}\nPassword: a\u{7}b")), None);
        assert_eq!(parse(&format!("{LINK}\nCookie: a=1\tb")), None);
    }

    #[test]
    fn secrets_pair_with_their_links() {
        let (a, b, c) = ("https://h.example/a.zip", "https://h.example/b.zip", "https://h.example/c.zip");
        let passwords = |text: &str| -> Vec<Option<String>> { links(text).into_iter().map(|l| l.secrets.password).collect() };
        let p = |s: &str| Some(s.to_string());
        // Each link its own, on its line or after it.
        assert_eq!(passwords(&format!("{a}\nPassword: one\n{b}\n\nPassword: two\n{c} pw: three")), [p("one"), p("two"), p("three")]);
        // Before the first link: every link's.
        assert_eq!(passwords(&format!("Password: all\n{a}\n{b}")), [p("all"), p("all")]);
        // After the last link with no link having its own: every link's (an archive's parts).
        assert_eq!(passwords(&format!("{a}\n{b}\n{c}\nPassword: parts")), [p("parts"), p("parts"), p("parts")]);
        // ... but only the last link's when another has its own.
        assert_eq!(passwords(&format!("{a}\nPassword: one\n{b}\n{c}\nPassword: three")), [p("one"), None, p("three")]);
        // A label on the last link's line is that link's.
        assert_eq!(passwords(&format!("{a}\n{b} password: mine")), [None, p("mine")]);
        // Each before its link.
        assert_eq!(passwords(&format!("Pass: 1111\n{a}\nPass: 2222\n{b}")), [p("1111"), p("2222")]);
        assert_eq!(passwords(&format!("Pass: 1111\n{a}\nPass: 2222\n{b}\n{c}")), [p("1111"), p("2222"), None]);
        // Mirrors on one line share theirs.
        assert_eq!(passwords(&format!("{a} {b}\nPassword: x\n{c}\nPassword: y")), [p("x"), p("x"), p("y")]);
        // A link given twice is one, with what was said of it both times.
        let merged = links(&format!("{a}\nPassword: x\n{b}\nPassword: y\n{a} Cookie: s=1"));
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].secrets, Secrets { password: p("x"), cookies: p("s=1"), ..Default::default() });
        assert_eq!(merged[1].secrets, password("y"));
        // A token goes to the links of its service among the others.
        let ghp = format!("ghp_{}", "A1b2".repeat(9));
        let paired = links(&format!("{ghp}\nhttps://github.com/o/r/archive/main.zip\n{a}"));
        assert_eq!(paired.iter().map(|l| l.secrets.clone()).collect::<Vec<_>>(), [auth(&format!("Bearer {ghp}")), Secrets::default()]);
    }

    #[test]
    fn text_without_secrets_stays_plain() {
        for text in [
            "",
            "https://h.example/f.zip",
            "https://h.example/a.zip https://h.example/b.zip\nhttps://h.example/c.zip",
            "https://h.example/keys/monkey-key.zip",
            "https://h.example/f.zip\nkey.txt and my_key_file.bin",
            "https://h.example/f.zip keyboard: 1",
            "https://h.example/account/password-reset.html",
            "https://h.example/f.zip\npassword-reset.html",
            "https://h.example/f.zip\nPassport: 123",
            "Note: get it from https://h.example/f.zip before Friday",
            "https://h.example/f.zip\nUpdate: mirrors below",
            "https://h.example/f.zip\nx7Gk2pQ9vL4mN8rTqW3e",
            "https://h.example/f.zip\nPassword: none",
            "https://h.example/f.zip\nPassword: N/A",
            "https://h.example/f.zip\nThe password is in the readme",
            "https://h.example/f.zip\nPassword reset instructions",
            "https://h.example/f.zip\nKey features: fast",
            "https://h.example/f.zip\nToken: see below",
            "https://h.example/f.zip\nCookie: none",
            "https://h.example/f.zip #photography #throwbackthursday",
            "https://h.example/f.zip\nhf_hub_download_example_file",
            "https://h.example/f.zip\nPassword protected",
            "https://h.example/f.zip\nAuthorization required",
            "https://h.example/f.zip\nToken required",
            "https://h.example/f.zip\nBearer required",
            "https://h.example/f.zip\nBearer authenticationrequired",
            "Password: hunter2",
            "curl is a command line tool",
        ] {
            assert_eq!(parse(text), None, "{text}");
        }
    }

    #[test]
    fn secrets_are_never_shown() {
        let secrets = Secrets {
            auth_header: Some("Bearer zzz-auth".into()),
            password: Some("hunter2".into()),
            cookies: Some("sid=ccc".into()),
            headers: headers(&[("X-Api-Key", "kkk")]),
            ..Default::default()
        };
        let link = PastedLink { url: Url::parse(LINK).unwrap(), secrets };
        let shown = format!("{:?} {:?} {}", link.secrets, link, link.secrets.note(&link.url).unwrap());
        for secret in ["zzz-auth", "hunter2", "ccc", "kkk"] {
            assert!(!shown.contains(secret), "{shown}");
        }
        assert!(shown.contains(r#"Secrets["auth_header", "password", "cookies", "headers"]"#), "{shown}");
        assert_eq!(link.secrets.note(&link.url).unwrap(), "Using the password and sign-in from your paste for h.example");
        assert_eq!(Secrets::default().note(&link.url), None);
    }

    #[test]
    fn apply_hands_the_secrets_to_the_download() {
        let pasted = Secrets {
            auth_header: Some("Bearer pasted".into()),
            password: Some("hunter2".into()),
            cookies: Some("sid=1".into()),
            headers: headers(&[("X-Api-Key", "k")]),
            referer: Some("https://ref.example/".into()),
            ..Default::default()
        };
        let link = PastedLink { url: Url::parse(LINK).unwrap(), secrets: pasted };
        let mut options = DownloadOptions { secret_headers: headers(&[("x-api-key", "old"), ("X-Other", "o")]), ..Default::default() };
        apply(&link, &mut options);
        assert_eq!(options.auth_header.as_deref(), Some("Bearer pasted"));
        // The referer as the headers are: a copied one may hold a session.
        assert_eq!((options.password.as_deref(), options.referer.as_deref()), (Some("hunter2"), None));
        let expected = headers(&[("X-Other", "o"), ("X-Api-Key", "k"), ("Cookie", "sid=1"), ("Referer", "https://ref.example/")]);
        assert_eq!(options.secret_headers, expected);
        let saved = serde_json::to_string(&options).unwrap();
        for secret in ["pasted", "hunter2", "sid=1", "\"k\"", "ref.example"] {
            assert!(!saved.contains(secret), "{saved}");
        }

        // What the user entered wins.
        let mut options = DownloadOptions {
            auth_header: Some("Bearer typed".into()),
            password: Some("typed".into()),
            referer: Some("https://typed.example/".into()),
            ..Default::default()
        };
        apply(&link, &mut options);
        assert_eq!(options.auth_header.as_deref(), Some("Bearer typed"));
        assert_eq!((options.password.as_deref(), options.referer.as_deref()), (Some("typed"), Some("https://typed.example/")));
        assert!(!options.secret_headers.iter().any(|(name, _)| name == "Referer"), "{:?}", options.secret_headers);
    }

    /// Browsers copy `( ) [ ] '` in links as they are: a link ends at a space or a quote, and a
    /// bracket or quote of the sentence around it is left out.
    #[test]
    fn links_keep_their_brackets() {
        for (text, url) in [
            ("https://bob:pw@h.example/[Group]%20Show%2001.mkv", "https://h.example/[Group]%20Show%2001.mkv"),
            ("https://archive.org/download/x/Foo%20(1999).zip\nPassword: s3cret", "https://archive.org/download/x/Foo%20(1999).zip"),
            ("(see https://h.example/f.zip)\nPassword: s3cret", "https://h.example/f.zip"),
            ("[mirror](https://h.example/f_(v2).zip).\nPassword: s3cret", "https://h.example/f_(v2).zip"),
            ("'https://h.example/f.zip',\nPassword: s3cret", "https://h.example/f.zip"),
            ("\"https://h.example/f.zip\"\nPassword: s3cret", "https://h.example/f.zip"),
        ] {
            assert_eq!(one(text).url, Url::parse(url).unwrap(), "{text}");
        }
    }

    /// A "leaving this site" link is its target, which gets the password and key of the paste
    /// but nothing it sent to the wrapper's host.
    #[test]
    fn wrapped_links_keep_only_what_is_for_their_target() {
        let wrapped = "https://www.youtube.com/redirect?q=https%3A%2F%2Ffiles.example%2Ftool.zip";
        let link = one(&format!("curl '{wrapped}' -b 'SID=yt-session' -H 'Authorization: Bearer yt-token'"));
        assert_eq!((link.url.as_str(), link.secrets), ("https://files.example/tool.zip", Secrets::default()));
        let link = one("https://bob:pw@www.google.com/url?q=https://files.example/tool.zip\nPassword: s3cret");
        assert_eq!((link.url.as_str(), link.secrets), ("https://files.example/tool.zip", password("s3cret")));
    }

    /// Lines a paste leaves out that are inputs of their own are kept for the caller to read.
    #[test]
    fn other_inputs_are_what_the_paste_leaves() {
        let magnet = "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567&dn=ubuntu.iso";
        let text = format!("{magnet}\n\n  C:\\lists\\set.meta4  \nhttps://bob:pw@nas.example/backup.tar\nPassword: x\nThe files:");
        assert_eq!(other_inputs(&text), [(1, magnet), (3, "C:\\lists\\set.meta4")]);
        assert_eq!(links(&text).len(), 1);
    }

    #[test]
    fn typed_sign_ins_become_basic_credentials() {
        assert_eq!(authorization(" bob:pw "), basic("bob:pw"));
        assert_eq!(authorization("https://bob:p%40w@nas.example/big.iso"), basic("bob:p@w"));
        for typed in ["Bearer abc", "Basic Ym9iOnB3", "Digest username=\"a\", uri=\"/x:y\""] {
            assert_eq!(authorization(typed), typed);
        }
    }
}

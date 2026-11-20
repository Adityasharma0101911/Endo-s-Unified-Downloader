//! Desktop notifications when downloads finish or fail, while the window is not in front: one for
//! a download, one with a count for downloads that end together (see [`Batch`]). A Windows toast,
//! the macOS Notification Center (through `osascript`), `org.freedesktop.Notifications` on Linux.

use std::sync::mpsc;

use eframe::egui;

/// Downloads that end within this many seconds of each other share one notification.
const QUIET: f64 = 3.0;
/// Longest a download that ended waits for others to end with it.
const HOLD: f64 = 60.0;
/// Fewest seconds between two notifications: a queue of small files is no storm of them.
const GAP: f64 = 20.0;
/// Names a notification of several downloads lists before "and N more".
const NAMED: usize = 2;

/// What one notification says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub title: String,
    pub body: String,
}

/// Shows a notice on the desktop: [`sender`]'s in the app, a stand-in in tests.
pub type Sender = Box<dyn Fn(Notice)>;

/// A download that ended: its name, and the folder it went to or why it failed (in the words its
/// card uses).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ended {
    Done { name: String, folder: String },
    Failed { name: String, why: String },
}

/// The downloads that ended and are not told yet, gathered into one notification (see
/// [`Batch::due_at`]); times are the window's clock, in seconds.
#[derive(Debug, Default)]
pub struct Batch {
    ended: Vec<Ended>,
    /// When the first and the last of them ended.
    first: f64,
    last: f64,
    /// When the last notification was shown.
    shown: Option<f64>,
}

impl Batch {
    pub fn push(&mut self, ended: Ended, now: f64) {
        if self.ended.is_empty() {
            self.first = now;
        }
        self.ended.push(ended);
        self.last = now;
    }

    /// Forgets the downloads gathered: the user saw them end.
    pub fn clear(&mut self) {
        self.ended.clear();
    }

    /// When the downloads gathered are told: once none more ended for [`QUIET`] seconds (or the
    /// first waited [`HOLD`]), and [`GAP`] after the last notification. None while there are none.
    pub fn due_at(&self) -> Option<f64> {
        let ready = (self.last + QUIET).min(self.first + HOLD);
        (!self.ended.is_empty()).then(|| self.shown.map_or(ready, |shown| ready.max(shown + GAP)))
    }

    /// The notification of the downloads gathered, when it is due at `now`; they are then told.
    pub fn take(&mut self, now: f64) -> Option<Notice> {
        if self.due_at()? > now {
            return None;
        }
        self.shown = Some(now);
        Some(notice(&std::mem::take(&mut self.ended)))
    }
}

/// What a notification of `ended` (not empty) says: one download's name and where it went or why
/// it failed; for several, how many finished and failed, and the first names.
pub fn notice(ended: &[Ended]) -> Notice {
    let failed: Vec<&Ended> = ended.iter().filter(|e| matches!(e, Ended::Failed { .. })).collect();
    match ended {
        [Ended::Done { name, folder }] => Notice { title: name.clone(), body: format!("Downloaded to {}", folder) },
        [Ended::Failed { name, why }] => Notice { title: name.clone(), body: format!("Failed: {}", why) },
        _ => {
            let done = ended.len() - failed.len();
            let downloads = |n: usize| if n == 1 { "1 download".to_string() } else { format!("{} downloads", n) };
            let title = match (done, failed.len()) {
                (done, 0) => format!("{} finished", downloads(done)),
                (0, failed) => format!("{} failed", downloads(failed)),
                (done, failed) => format!("{} finished, {} failed", downloads(done), failed),
            };
            // The failed ones first: they need the user.
            let names: Vec<&str> = failed.iter().copied().chain(ended.iter().filter(|e| matches!(e, Ended::Done { .. }))).map(|e| match e {
                Ended::Done { name, .. } | Ended::Failed { name, .. } => name.as_str(),
            }).collect();
            let body = match names.len().saturating_sub(NAMED) {
                0 => names.join(", "),
                more => format!("{} and {} more", names[..NAMED].join(", "), more),
            };
            Notice { title, body }
        }
    }
}

/// Shows notices on this desktop, off the UI thread; a click on a Windows toast brings the window
/// forward (as the tray's Show does, through `tx`).
pub fn sender(tx: mpsc::Sender<crate::AppEvent>, ctx: egui::Context, rt: tokio::runtime::Handle) -> Sender {
    Box::new(move |notice| {
        #[cfg(windows)]
        {
            let (tx, ctx) = (tx.clone(), ctx.clone());
            let _ = rt.spawn_blocking(move || {
                if let Err(e) = windows_toast(&notice, move || {
                    if tx.send(crate::AppEvent::Tray(crate::tray::TrayAction::Show)).is_ok() {
                        ctx.request_repaint();
                    }
                }) {
                    tracing::warn!("Could not show a notification: {}", e);
                }
            });
        }
        #[cfg(target_os = "macos")]
        {
            let _ = (&tx, &ctx);
            let _ = rt.spawn_blocking(move || {
                // Its click opens Script Editor, not the app: there is no cheaper way in.
                let script = format!("display notification {} with title {}", apple_string(&notice.body), apple_string(&notice.title));
                if let Err(e) = std::process::Command::new("/usr/bin/osascript").args(["-e", &script]).output() {
                    tracing::warn!("Could not show a notification: {}", e);
                }
            });
        }
        #[cfg(target_os = "linux")]
        {
            let _ = (&tx, &ctx);
            rt.spawn(async move {
                match tokio::time::timeout(std::time::Duration::from_secs(5), freedesktop(&notice)).await {
                    Ok(Err(e)) => tracing::warn!("Could not show a notification: {}", e),
                    Err(_) => tracing::warn!("Could not show a notification: D-Bus did not answer"),
                    Ok(Ok(())) => {}
                }
            });
        }
        #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
        let _ = (notice, &tx, &ctx, &rt);
    })
}

/// The name Windows shows notifications under (see [`register`]).
#[cfg(windows)]
const APP_ID: &str = "EndosUnifiedDownloader";

/// Shows `notice` as a toast of the app; `clicked` runs when the user clicks it while the app runs.
#[cfg(windows)]
fn windows_toast(notice: &Notice, clicked: impl Fn() + Send + 'static) -> windows::core::Result<()> {
    use std::sync::Mutex;
    use windows::core::HSTRING;
    use windows::Data::Xml::Dom::XmlDocument;
    use windows::Foundation::TypedEventHandler;
    use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};

    /// The toasts shown lately, kept so that a click on one still reaches the app.
    static SHOWN: Mutex<Vec<ToastNotification>> = Mutex::new(Vec::new());
    static REGISTERED: std::sync::Once = std::sync::Once::new();
    REGISTERED.call_once(register);
    let xml = format!(
        "<toast><visual><binding template=\"ToastGeneric\"><text>{}</text><text>{}</text></binding></visual></toast>",
        xml_text(&notice.title),
        xml_text(&notice.body)
    );
    let document = XmlDocument::new()?;
    document.LoadXml(&HSTRING::from(xml))?;
    let toast = ToastNotification::CreateToastNotification(&document)?;
    toast.Activated(&TypedEventHandler::new(move |_, _| {
        clicked();
        Ok(())
    }))?;
    ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(APP_ID))?.Show(&toast)?;
    let mut shown = crate::util::lock(&SHOWN);
    if shown.len() >= 8 {
        shown.remove(0);
    }
    shown.push(toast);
    Ok(())
}

/// Registers [`APP_ID`] for the user with the app's name and logo, which Windows shows its toasts
/// under (an app without an installed package or shortcut of that id has no toasts otherwise).
#[cfg(windows)]
fn register() {
    use crate::desktop::Place;
    let key = format!(r"Software\Classes\AppUserModelId\{}", APP_ID);
    let at = |name: &str| Place::Registry { key: key.clone(), name: Some(name.to_string()) };
    let icon = crate::settings::data_dir().join("notification-icon.png");
    let mut values = vec![(at("DisplayName"), crate::TITLE.to_string())];
    if icon.is_file() || crate::settings::write_atomic(&icon, crate::WINDOW_ICON).is_ok() {
        values.push((at("IconUri"), icon.display().to_string()));
    }
    for (place, value) in values {
        if let Err(e) = crate::desktop::write(&place, Some(&value)) {
            tracing::warn!("Could not register the app's notifications: {}", e);
        }
    }
}

/// `text` for the content of an XML element.
#[cfg(any(windows, target_os = "linux", test))]
fn xml_text(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// `text` as an AppleScript string literal.
#[cfg(any(target_os = "macos", test))]
fn apple_string(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Shows `notice` through the desktop's notification server; its body may be read as markup.
#[cfg(target_os = "linux")]
async fn freedesktop(notice: &Notice) -> zbus::Result<()> {
    let connection = zbus::Connection::session().await?;
    let hints: std::collections::HashMap<&str, zbus::zvariant::Value> = std::collections::HashMap::new();
    let body = (crate::TITLE, 0u32, "", notice.title.as_str(), xml_text(&notice.body), Vec::<&str>::new(), hints, -1i32);
    connection
        .call_method(Some("org.freedesktop.Notifications"), "/org/freedesktop/Notifications", Some("org.freedesktop.Notifications"), "Notify", &body)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn done(name: &str) -> Ended {
        Ended::Done { name: name.to_string(), folder: "D:/dl".to_string() }
    }

    fn failed(name: &str) -> Ended {
        Ended::Failed { name: name.to_string(), why: "Server error".to_string() }
    }

    /// One download is told by its name and where it went, or why it failed; several by a count,
    /// failures first, a few names and how many more.
    #[test]
    fn notices_say_what_ended() {
        assert_eq!(notice(&[done("a.iso")]), Notice { title: "a.iso".into(), body: "Downloaded to D:/dl".into() });
        assert_eq!(notice(&[failed("a.iso")]), Notice { title: "a.iso".into(), body: "Failed: Server error".into() });
        assert_eq!(notice(&[done("a"), done("b")]), Notice { title: "2 downloads finished".into(), body: "a, b".into() });
        let mixed = notice(&[done("a"), done("b"), failed("c"), done("d")]);
        assert_eq!(mixed, Notice { title: "3 downloads finished, 1 failed".into(), body: "c, a and 2 more".into() });
        assert_eq!(notice(&[done("a"), failed("b")]).title, "1 download finished, 1 failed");
        assert_eq!(notice(&[failed("x"), failed("y"), failed("z")]).title, "3 downloads failed");
    }

    /// Downloads ending close together wait for each other and share one notification; one ending
    /// soon after a notification waits out the gap; a stream of them is told at least every minute.
    #[test]
    fn a_batch_is_one_notification() {
        let mut batch = Batch::default();
        assert_eq!((batch.due_at(), batch.take(100.0)), (None, None));
        batch.push(done("a"), 10.0);
        batch.push(done("b"), 11.0);
        assert_eq!(batch.due_at(), Some(14.0));
        assert_eq!(batch.take(13.9), None);
        assert_eq!(batch.take(14.0).map(|n| n.title), Some("2 downloads finished".to_string()));
        assert_eq!(batch.due_at(), None);
        batch.push(failed("c"), 15.0);
        assert_eq!(batch.due_at(), Some(34.0), "20 s after the last one");
        assert_eq!(batch.take(34.0).map(|n| n.title), Some("c".to_string()));
        for n in 0..40 {
            batch.push(done(&n.to_string()), 100.0 + 2.0 * n as f64);
        }
        assert_eq!(batch.due_at(), Some(160.0), "held a minute at most");
        batch.clear();
        assert_eq!(batch.due_at(), None);
    }

    #[test]
    fn text_is_escaped() {
        assert_eq!(xml_text("a <b> & c"), "a &lt;b&gt; &amp; c");
        assert_eq!(apple_string(r#"say "hi" \ bye"#), r#""say \"hi\" \\ bye""#);
    }
}

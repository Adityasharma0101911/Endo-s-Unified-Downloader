//! The icon in the notification area (the menu bar on macOS) with Show, Pause all, Resume all,
//! Start the queue, Stop the queue and Quit; on Windows a click on it shows the window. Each
//! choice reaches the UI thread as `AppEvent::Tray`, which wakes it. `tray-icon` on Windows and
//! macOS, a StatusNotifierItem (`ksni`) on Linux; none on a desktop without a tray.

use std::sync::Arc;

/// What the tray's menu asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayAction {
    Show,
    PauseAll,
    ResumeAll,
    StartQueue,
    StopQueue,
    Quit,
}

/// The menu, in order.
const MENU: [(TrayAction, &str); 6] = [
    (TrayAction::Show, "Show"),
    (TrayAction::PauseAll, "Pause all"),
    (TrayAction::ResumeAll, "Resume all"),
    (TrayAction::StartQueue, "Start the queue"),
    (TrayAction::StopQueue, "Stop the queue"),
    (TrayAction::Quit, "Quit"),
];

/// The app's logo, 32×32 RGBA (`extension/icons/icon-32.png`).
const ICON: &[u8] = include_bytes!("../assets/tray-32.rgba");

/// Hands a choice to the UI thread.
type Handler = Arc<dyn Fn(TrayAction) + Send + Sync>;

/// The tray icon, there while this is kept.
#[cfg(any(windows, target_os = "macos"))]
pub struct Tray(#[allow(dead_code)] tray_icon::TrayIcon);

/// Shows the tray icon, on the UI thread once its event loop runs; None where the desktop has
/// no tray.
#[cfg(any(windows, target_os = "macos"))]
pub fn start(handler: impl Fn(TrayAction) + Send + Sync + 'static, _rt: &tokio::runtime::Handle) -> Option<Tray> {
    use tray_icon::menu::{Menu, MenuEvent, MenuItem};
    use tray_icon::{TrayIconBuilder, TrayIconEvent};

    let handler: Handler = Arc::new(handler);
    let menu = Menu::new();
    for (_, label) in MENU {
        // An item's id is its label, which its event gives back.
        menu.append(&MenuItem::with_id(label, label, true, None)).ok()?;
    }
    let on_menu = Arc::clone(&handler);
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        if let Some(&(action, _)) = MENU.iter().find(|(_, label)| event.id == *label) {
            on_menu(action);
        }
    }));
    TrayIconEvent::set_event_handler(Some(move |event| {
        use tray_icon::{MouseButton, MouseButtonState};
        if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
            // On macOS a click opens the menu.
            if cfg!(windows) {
                handler(TrayAction::Show);
            }
        }
    }));
    let built = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_icon(tray_icon::Icon::from_rgba(ICON.to_vec(), 32, 32).ok()?)
        .with_tooltip(crate::TITLE)
        .with_menu_on_left_click(cfg!(target_os = "macos"))
        .build();
    built.map(Tray).inspect_err(|e| tracing::warn!("No tray icon: {}", e)).ok()
}

/// The tray icon, there while this is kept.
#[cfg(target_os = "linux")]
pub struct Tray(#[allow(dead_code)] ksni::Handle<Item>);

/// The StatusNotifierItem the desktop's tray shows.
#[cfg(target_os = "linux")]
pub struct Item(Handler);

#[cfg(target_os = "linux")]
impl ksni::Tray for Item {
    fn id(&self) -> String {
        "endos-unified-downloader".to_string()
    }

    fn title(&self) -> String {
        crate::TITLE.to_string()
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        // ARGB in network byte order, as the specification has it.
        let data = ICON.chunks_exact(4).flat_map(|p| [p[3], p[0], p[1], p[2]]).collect();
        vec![ksni::Icon { width: 32, height: 32, data }]
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        (self.0)(TrayAction::Show);
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        let item = |(action, label): (TrayAction, &str)| ksni::menu::StandardItem {
            label: label.to_string(),
            activate: Box::new(move |item: &mut Self| (item.0)(action)),
            ..Default::default()
        };
        MENU.into_iter().map(|entry| item(entry).into()).collect()
    }
}

/// Shows the tray icon; None where the desktop has no tray (no StatusNotifierWatcher), or its
/// D-Bus does not answer within 3 s.
#[cfg(target_os = "linux")]
pub fn start(handler: impl Fn(TrayAction) + Send + Sync + 'static, rt: &tokio::runtime::Handle) -> Option<Tray> {
    use ksni::TrayMethods;
    let item = Item(Arc::new(handler));
    match rt.block_on(async { tokio::time::timeout(std::time::Duration::from_secs(3), item.spawn()).await }) {
        Ok(Ok(handle)) => Some(Tray(handle)),
        Ok(Err(e)) => {
            tracing::info!("No tray icon: {}", e);
            None
        }
        Err(_) => {
            tracing::info!("No tray icon: D-Bus did not answer");
            None
        }
    }
}

/// No tray here.
#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
pub struct Tray;

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
pub fn start(_handler: impl Fn(TrayAction) + Send + Sync + 'static, _rt: &tokio::runtime::Handle) -> Option<Tray> {
    None
}

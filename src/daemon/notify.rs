//! Desktop notifications on track change.
//!
//! Linux uses the freedesktop.org D-Bus interface, which every Linux
//! notification daemon implements. macOS has no such interface, so it shells
//! out to `osascript`'s `display notification`. Other platforms get a no-op.

use crate::subsonic::models::Child;

/// Notification body: artist on the first line, album on the second.
#[must_use]
pub fn track_body(song: &Child) -> String {
    let artist = song.artist.as_deref().unwrap_or("Unknown Artist");
    match song.album.as_deref() {
        Some(album) if !album.is_empty() => format!("{artist}\n{album}"),
        _ => artist.to_string(),
    }
}

/// Test/headless guard: never touch the notification backend when set. The
/// test harness sets this (tests/common) so the suite cannot spam the desktop;
/// it must apply to every backend, not just the Linux D-Bus one.
#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn desktop_notify_suppressed() -> bool {
    std::env::var_os("FERROSONIC_NO_DESKTOP_NOTIFY").is_some()
}

/// Write `bytes` into the notifier's reusable cover tempfile, returning its
/// path. The slot lock intentionally spans the `spawn_blocking` write so
/// concurrent cover writes to the shared path serialize; do not tighten.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[allow(clippy::significant_drop_tightening)]
async fn write_cover_tempfile(
    slot: &tokio::sync::Mutex<Option<tempfile::NamedTempFile>>,
    bytes: &[u8],
) -> Option<std::path::PathBuf> {
    let mut guard = slot.lock().await;
    if guard.is_none() {
        *guard = tempfile::Builder::new()
            .prefix("ferrosonic-notify-")
            .suffix(".img")
            .tempfile()
            .ok();
    }
    let path = guard.as_ref()?.path().to_path_buf();
    // Atomic write off the async worker (atomic_write_bytes fsyncs).
    let dest = path.clone();
    let owned = bytes.to_vec();
    tokio::task::spawn_blocking(move || crate::io_util::atomic_write_bytes(&dest, &owned))
        .await
        .ok()?
        .ok()?;
    Some(path)
}

#[cfg(target_os = "linux")]
pub use linux::Notifier;
#[cfg(target_os = "macos")]
pub use macos::Notifier;
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub use stub::Notifier;

#[cfg(target_os = "linux")]
mod linux {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Mutex as StdMutex;

    use tempfile::NamedTempFile;
    use tokio::sync::{Mutex, OnceCell};
    use tracing::debug;
    use zbus::zvariant::Value;
    use zbus::{proxy, Connection};

    #[proxy(
        interface = "org.freedesktop.Notifications",
        default_service = "org.freedesktop.Notifications",
        default_path = "/org/freedesktop/Notifications"
    )]
    trait Notifications {
        // org.freedesktop.Notifications.Notify signature is fixed by the D-Bus spec.
        #[allow(clippy::too_many_arguments)]
        fn notify(
            &self,
            app_name: &str,
            replaces_id: u32,
            app_icon: &str,
            summary: &str,
            body: &str,
            actions: &[&str],
            hints: HashMap<&str, &Value<'_>>,
            expire_timeout: i32,
        ) -> zbus::Result<u32>;
    }

    /// Sends track-change notifications over the session bus. The connection is
    /// established lazily and cached; a missing session bus (headless / TTY)
    /// disables notifications instead of erroring.
    pub struct Notifier {
        conn: OnceCell<Option<Connection>>,
        last_notif_id: AtomicU32,
        last_song: StdMutex<Option<String>>,
        cover_file: Mutex<Option<NamedTempFile>>,
    }

    impl Default for Notifier {
        fn default() -> Self {
            Self::new()
        }
    }

    impl Notifier {
        /// Construct an idle notifier; no D-Bus connection is made until the
        /// first notification is shown.
        #[must_use]
        pub fn new() -> Self {
            Self {
                conn: OnceCell::new(),
                last_notif_id: AtomicU32::new(0),
                last_song: StdMutex::new(None),
                cover_file: Mutex::new(None),
            }
        }

        /// True when `song_id` differs from the last notified track, recording
        /// it so the 500ms tick fires a notification once per track change.
        pub fn mark_if_changed(&self, song_id: &str) -> bool {
            let mut last = self
                .last_song
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if last.as_deref() == Some(song_id) {
                false
            } else {
                *last = Some(song_id.to_string());
                true
            }
        }

        async fn proxy(&self) -> Option<NotificationsProxy<'_>> {
            if super::desktop_notify_suppressed() {
                return None;
            }
            let conn = self
                .conn
                .get_or_init(|| async { Connection::session().await.ok() })
                .await
                .as_ref()?;
            NotificationsProxy::new(conn).await.ok()
        }

        async fn cover_uri(&self, bytes: &[u8]) -> Option<String> {
            let path = super::write_cover_tempfile(&self.cover_file, bytes).await?;
            Some(format!("file://{}", path.display()))
        }

        /// Show or replace the track-change notification. A failed `Notify`
        /// (no daemon listening) is logged and ignored.
        pub async fn show(&self, title: &str, body: &str, cover: Option<&[u8]>) {
            let Some(proxy) = self.proxy().await else {
                return;
            };
            let uri = match cover {
                Some(bytes) => self.cover_uri(bytes).await,
                None => None,
            };
            let uri_val = uri.as_deref().map(Value::from);
            let mut hints: HashMap<&str, &Value<'_>> = HashMap::new();
            if let Some(v) = &uri_val {
                hints.insert("image-path", v);
            }
            let replaces = self.last_notif_id.load(Ordering::Relaxed);
            match proxy
                .notify("Ferrosonic", replaces, "", title, body, &[], hints, 5000)
                .await
            {
                Ok(id) => self.last_notif_id.store(id, Ordering::Relaxed),
                Err(e) => debug!("desktop notify failed: {e}"),
            }
        }
    }
}

/// Build the `osascript` invocation that displays `body` under `title`.
///
/// The text is passed as `argv` and read back inside `on run argv`, so no
/// `AppleScript` string escaping is needed and a title/body containing quotes (or
/// anything else) cannot alter the script.
///
/// Compiled under `test` as well so the argv construction is covered by a unit
/// test on any host; only macOS actually runs it.
#[cfg(any(target_os = "macos", test))]
fn osascript_notification_command(title: &str, body: &str) -> tokio::process::Command {
    const SCRIPT: &str = "on run argv\n\
                          \tdisplay notification (item 2 of argv) with title (item 1 of argv)\n\
                          \tend run";
    let mut cmd = tokio::process::Command::new("osascript");
    cmd.arg("-e").arg(SCRIPT).arg("--").arg(title).arg(body);
    cmd
}

/// Build the `terminal-notifier` invocation for `title`/`body`, optionally
/// attaching `cover_path` as the notification image.
///
/// Compiled under `test` as well so the argv construction is covered on any
/// host; only macOS actually runs it.
#[cfg(any(target_os = "macos", test))]
fn terminal_notifier_command(
    binary: &std::path::Path,
    title: &str,
    body: &str,
    cover_path: Option<&std::path::Path>,
) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(binary);
    cmd.arg("-title")
        .arg(title)
        .arg("-message")
        .arg(body)
        // One group id so each track replaces the previous banner instead of
        // stacking a new one.
        .arg("-group")
        .arg("ferrosonic");
    if let Some(path) = cover_path {
        cmd.arg("-contentImage").arg(path);
    }
    cmd
}

#[cfg(target_os = "macos")]
mod macos {
    use std::path::PathBuf;
    use std::sync::Mutex as StdMutex;

    use tempfile::NamedTempFile;
    use tokio::sync::Mutex;
    use tracing::warn;

    /// Track-change notifications on macOS.
    ///
    /// macOS ships no freedesktop notification daemon, so the Linux D-Bus path
    /// does not apply. Homebrew's `terminal-notifier` is preferred when
    /// installed: it is a bundled app posting through `UserNotifications`, so
    /// it works where `osascript` is unreliable, replaces the previous banner
    /// (`-group`), and can attach cover art (`-contentImage`). Without it, the
    /// built-in `AppleScript` `display notification` fallback is used, which is
    /// text-only.
    pub struct Notifier {
        last_song: StdMutex<Option<String>>,
        /// Absolute path of `terminal-notifier` when found on PATH.
        terminal_notifier: Option<PathBuf>,
        cover_file: Mutex<Option<NamedTempFile>>,
    }

    impl Default for Notifier {
        fn default() -> Self {
            Self::new()
        }
    }

    impl Notifier {
        /// Construct an idle notifier, probing PATH once for
        /// `terminal-notifier`; nothing else runs until the first change.
        #[must_use]
        pub fn new() -> Self {
            Self {
                last_song: StdMutex::new(None),
                terminal_notifier: find_terminal_notifier(),
                cover_file: Mutex::new(None),
            }
        }

        /// True when `song_id` differs from the last notified track, recording
        /// it so the 500ms tick fires a notification once per track change.
        pub fn mark_if_changed(&self, song_id: &str) -> bool {
            let mut last = self
                .last_song
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if last.as_deref() == Some(song_id) {
                false
            } else {
                *last = Some(song_id.to_string());
                true
            }
        }

        async fn cover_path(&self, bytes: &[u8]) -> Option<PathBuf> {
            super::write_cover_tempfile(&self.cover_file, bytes).await
        }

        /// Show a track-change notification, preferring `terminal-notifier`
        /// when installed and falling back to `osascript`. Text is passed as
        /// separate argv entries in both paths, so no escaping is needed and an
        /// arbitrary title/body cannot alter a script. Failures are logged at
        /// `warn` (not `debug`) because a missing banner is otherwise invisible.
        pub async fn show(&self, title: &str, body: &str, cover: Option<&[u8]>) {
            if super::desktop_notify_suppressed() {
                return;
            }
            if let Some(binary) = self.terminal_notifier.as_deref() {
                let cover_path = match cover {
                    Some(bytes) => self.cover_path(bytes).await,
                    None => None,
                };
                let mut cmd =
                    super::terminal_notifier_command(binary, title, body, cover_path.as_deref());
                match cmd.output().await {
                    Ok(out) if out.status.success() => return,
                    Ok(out) => warn!(
                        "desktop notify failed (terminal-notifier {}): {}; falling back to osascript",
                        out.status,
                        String::from_utf8_lossy(&out.stderr).trim()
                    ),
                    Err(e) => warn!(
                        "desktop notify failed (terminal-notifier spawn): {e}; \
                         falling back to osascript"
                    ),
                }
            }
            match super::osascript_notification_command(title, body)
                .output()
                .await
            {
                Ok(out) if out.status.success() => {}
                Ok(out) => warn!(
                    "desktop notify failed (osascript {}): {}",
                    out.status,
                    String::from_utf8_lossy(&out.stderr).trim()
                ),
                Err(e) => warn!("desktop notify failed (osascript spawn): {e}"),
            }
        }
    }

    /// Absolute path of `terminal-notifier` on PATH, if installed.
    fn find_terminal_notifier() -> Option<PathBuf> {
        let out = std::process::Command::new("which")
            .arg("terminal-notifier")
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if path.is_empty() {
            None
        } else {
            Some(PathBuf::from(path))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        desktop_notify_suppressed, osascript_notification_command, terminal_notifier_command,
        track_body,
    };
    use crate::subsonic::models::Child;

    fn argv(cmd: &tokio::process::Command) -> Vec<String> {
        cmd.as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    fn song(artist: Option<&str>, album: Option<&str>) -> Child {
        Child {
            id: "x".into(),
            title: "Title".into(),
            artist: artist.map(str::to_string),
            album: album.map(str::to_string),
            ..Child::default()
        }
    }

    #[test]
    fn body_is_artist_then_album_on_two_lines() {
        assert_eq!(
            track_body(&song(Some("Boards"), Some("Geogaddi"))),
            "Boards\nGeogaddi"
        );
    }

    #[test]
    fn body_drops_the_album_line_when_absent_or_empty() {
        assert_eq!(track_body(&song(Some("Boards"), None)), "Boards");
        assert_eq!(track_body(&song(Some("Boards"), Some(""))), "Boards");
    }

    #[test]
    fn body_falls_back_when_artist_missing() {
        assert_eq!(
            track_body(&song(None, Some("Geogaddi"))),
            "Unknown Artist\nGeogaddi"
        );
    }

    #[test]
    #[serial_test::serial]
    fn suppression_guard_reads_env_var() {
        std::env::set_var("FERROSONIC_NO_DESKTOP_NOTIFY", "1");
        assert!(desktop_notify_suppressed());
        std::env::remove_var("FERROSONIC_NO_DESKTOP_NOTIFY");
        assert!(!desktop_notify_suppressed());
    }

    #[test]
    fn terminal_notifier_command_passes_text_and_cover_as_args() {
        let cmd = terminal_notifier_command(
            std::path::Path::new("/opt/homebrew/bin/terminal-notifier"),
            "Title \"quoted\"",
            "Artist\nAlbum",
            Some(std::path::Path::new("/tmp/ferrosonic-cover.img")),
        );
        assert_eq!(
            argv(&cmd),
            vec![
                "-title",
                "Title \"quoted\"",
                "-message",
                "Artist\nAlbum",
                "-group",
                "ferrosonic",
                "-contentImage",
                "/tmp/ferrosonic-cover.img",
            ]
        );
        assert_eq!(
            cmd.as_std().get_program().to_string_lossy(),
            "/opt/homebrew/bin/terminal-notifier"
        );
    }

    #[test]
    fn terminal_notifier_command_omits_cover_when_absent() {
        let cmd =
            terminal_notifier_command(std::path::Path::new("terminal-notifier"), "T", "B", None);
        let args = argv(&cmd);
        assert!(!args.iter().any(|a| a == "-contentImage"));
        assert_eq!(
            args,
            vec!["-title", "T", "-message", "B", "-group", "ferrosonic"]
        );
    }

    #[test]
    fn osascript_command_passes_text_as_argv_not_in_script() {
        let cmd = osascript_notification_command("Title \"quoted\"", "Artist\nAlbum");
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        // Layout: osascript -e <script> -- <title> <body>. Every fixed slot is
        // checked so a reordering cannot silently break the argv passing.
        assert_eq!(args.len(), 5);
        assert_eq!(args[0], "-e");
        assert_eq!(args[2], "--");
        assert_eq!(args[3], "Title \"quoted\"");
        assert_eq!(args[4], "Artist\nAlbum");
        // Untrusted text is never interpolated into the AppleScript source.
        assert!(args[1].contains("on run argv"));
        assert!(
            !args[1].contains("quoted"),
            "title must not be embedded in the script"
        );
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod stub {
    /// No-op notifier on platforms with no notification backend wired up.
    pub struct Notifier;
    impl Notifier {
        pub fn new() -> Self {
            Self
        }
        pub fn mark_if_changed(&self, _song_id: &str) -> bool {
            false
        }
        pub async fn show(&self, _title: &str, _body: &str, _cover: Option<&[u8]>) {}
    }
}

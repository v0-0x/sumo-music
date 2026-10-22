//! A desktop notification when a song starts, through the desktop's notification service
//! (Linux). Each one replaces the last, so they don't pile up. Windows and macOS show the
//! playing song in their own media overlays instead.

#[cfg(not(any(windows, target_os = "macos")))]
pub fn song(title: &str, artist: &str, album: &str) {
    use std::collections::HashMap;
    use zbus::zvariant::Value;

    /// The connection and the id of the notification shown last.
    static LAST: tokio::sync::Mutex<Option<(zbus::Connection, u32)>> = tokio::sync::Mutex::const_new(None);

    let summary = title.to_string();
    let body = escape(
        &[artist, album]
            .iter()
            .filter(|s| !s.is_empty())
            .copied()
            .collect::<Vec<_>>()
            .join(" — "),
    );
    tokio::spawn(async move {
        let mut last = LAST.lock().await;
        let conn = match last.as_ref() {
            Some((conn, _)) => conn.clone(),
            None => match zbus::Connection::session().await {
                Ok(conn) => conn,
                Err(e) => {
                    tracing::debug!("no session bus for notifications: {e}");
                    return;
                }
            },
        };
        let replaces = last.as_ref().map_or(0, |l| l.1);
        let hints: HashMap<&str, Value> = HashMap::from([("desktop-entry", Value::from("multimusic"))]);
        let sent = conn
            .call_method(
                Some("org.freedesktop.Notifications"),
                "/org/freedesktop/Notifications",
                Some("org.freedesktop.Notifications"),
                "Notify",
                &(
                    "Sumo",
                    replaces,
                    "sumo",
                    summary.as_str(),
                    body.as_str(),
                    Vec::<&str>::new(),
                    hints,
                    5000i32,
                ),
            )
            .await
            .and_then(|reply| reply.body().deserialize::<u32>());
        match sent {
            Ok(id) => *last = Some((conn, id)),
            Err(e) => tracing::debug!("notification not shown: {e}"),
        }
    });
}

#[cfg(any(windows, target_os = "macos"))]
pub fn song(_title: &str, _artist: &str, _album: &str) {}

/// Notification bodies may hold a little markup: song names are shown as written.
#[cfg_attr(any(windows, target_os = "macos"), allow(dead_code))]
fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    #[test]
    fn markup_is_escaped() {
        assert_eq!(super::escape("Tom & Jerry <3"), "Tom &amp; Jerry &lt;3");
    }
}

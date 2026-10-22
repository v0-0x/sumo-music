//! The system's file and folder pickers. They run on their own thread, so the window keeps
//! drawing; the choice is picked up with [`take`] a moment later.

use std::path::PathBuf;

use egui::Id;

/// What to pick.
#[derive(Clone, Copy)]
pub enum Pick {
    Folder,
    /// A file: what kind (named in the picker's filter) and its extensions.
    File(&'static str, &'static [&'static str]),
}

/// Opens a picker titled `title`; its answer comes back under `id`.
pub fn open(ctx: &egui::Context, id: &'static str, title: &'static str, pick: Pick) {
    let ctx = ctx.clone();
    let busy = Id::new(("picker-open", id));
    if ctx.data(|d| d.get_temp::<bool>(busy)).unwrap_or(false) {
        return;
    }
    ctx.data_mut(|d| d.insert_temp(busy, true));
    std::thread::spawn(move || {
        let picked = match system::pick(title, pick) {
            Ok(picked) => picked.map(Ok),
            Err(e) => {
                tracing::warn!("file picker: {e:#}");
                Some(Err(e.to_string()))
            }
        };
        ctx.data_mut(|d| {
            d.insert_temp(busy, false);
            if let Some(picked) = picked {
                d.insert_temp(Id::new(("picked", id)), picked);
            }
        });
        ctx.request_repaint();
    });
}

/// The answer to the picker opened under `id`, once: a path, or why there's no picker.
pub fn take(ctx: &egui::Context, id: &'static str) -> Option<Result<PathBuf, String>> {
    let key = Id::new(("picked", id));
    ctx.data_mut(|d| {
        let picked = d.get_temp(key);
        d.remove::<Result<PathBuf, String>>(key);
        picked
    })
}

#[cfg(any(windows, target_os = "macos"))]
mod system {
    use super::Pick;
    use std::path::PathBuf;

    pub fn pick(title: &str, pick: Pick) -> anyhow::Result<Option<PathBuf>> {
        let dialog = rfd::AsyncFileDialog::new().set_title(title);
        let picked = match pick {
            Pick::Folder => pollster::block_on(dialog.pick_folder()),
            Pick::File(what, extensions) => pollster::block_on(dialog.add_filter(what, extensions).pick_file()),
        };
        Ok(picked.map(|f| f.path().to_path_buf()))
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod system {
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::process::Command;

    use anyhow::{anyhow, Context as _};
    use zbus::zvariant::{OwnedValue, Value};

    use super::Pick;

    /// The desktop's picker (through the XDG portal), else zenity's or kdialog's.
    pub fn pick(title: &str, pick: Pick) -> anyhow::Result<Option<PathBuf>> {
        match portal(title, pick) {
            Ok(picked) => Ok(picked),
            Err(e) => {
                tracing::info!("no desktop portal for picking files ({e:#}), trying zenity and kdialog");
                helper(title, pick)
            }
        }
    }

    fn portal(title: &str, pick: Pick) -> anyhow::Result<Option<PathBuf>> {
        let conn = zbus::blocking::Connection::session()?;
        // The portal answers on a request object whose path we know beforehand, so the
        // answer can't come before we listen for it.
        let token = format!("sumo{}", rand::random::<u32>());
        let sender = conn
            .unique_name()
            .ok_or_else(|| anyhow!("no D-Bus name"))?
            .trim_start_matches(':')
            .replace('.', "_");
        let path = format!("/org/freedesktop/portal/desktop/request/{sender}/{token}");
        let request = zbus::blocking::Proxy::new(
            &conn,
            "org.freedesktop.portal.Desktop",
            path.as_str(),
            "org.freedesktop.portal.Request",
        )?;
        let mut answers = request.receive_signal("Response")?;

        let mut options: HashMap<&str, Value> = HashMap::new();
        options.insert("handle_token", Value::from(token.as_str()));
        options.insert("modal", Value::from(true));
        match pick {
            Pick::Folder => {
                options.insert("directory", Value::from(true));
            }
            Pick::File(what, extensions) => {
                let patterns: Vec<(u32, String)> = extensions.iter().map(|e| (0, format!("*.{e}"))).collect();
                options.insert("filters", Value::from(vec![(what.to_string(), patterns)]));
            }
        }
        let chooser = zbus::blocking::Proxy::new(
            &conn,
            "org.freedesktop.portal.Desktop",
            "/org/freedesktop/portal/desktop",
            "org.freedesktop.portal.FileChooser",
        )?;
        let _: zbus::zvariant::OwnedObjectPath = chooser.call("OpenFile", &("", title, options))?;

        let answer = answers.next().ok_or_else(|| anyhow!("the portal didn't answer"))?;
        let (code, results): (u32, HashMap<String, OwnedValue>) = answer.body()?;
        // 1 is cancelled, 2 is closed some other way.
        if code != 0 {
            return Ok(None);
        }
        let uris: Vec<String> = results
            .get("uris")
            .cloned()
            .ok_or_else(|| anyhow!("the portal sent no file"))?
            .try_into()
            .context("reading the portal's answer")?;
        Ok(uris.first().and_then(|uri| file_path(uri)))
    }

    /// The path of a `file://` link.
    fn file_path(uri: &str) -> Option<PathBuf> {
        let path = uri.strip_prefix("file://")?;
        Some(PathBuf::from(urlencoding::decode(path).ok()?.into_owned()))
    }

    fn helper(title: &str, pick: Pick) -> anyhow::Result<Option<PathBuf>> {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
        let zenity = || {
            let mut c = Command::new("zenity");
            c.args(["--file-selection", "--title", title]);
            match pick {
                Pick::Folder => {
                    c.arg("--directory");
                }
                Pick::File(what, extensions) => {
                    let patterns: Vec<String> = extensions.iter().map(|e| format!("*.{e}")).collect();
                    c.arg(format!("--file-filter={what} | {}", patterns.join(" ")));
                }
            }
            c
        };
        let kdialog = || {
            let mut c = Command::new("kdialog");
            c.args(["--title", title]);
            match pick {
                Pick::Folder => {
                    c.args(["--getexistingdirectory", &home]);
                }
                Pick::File(what, extensions) => {
                    let patterns: Vec<String> = extensions.iter().map(|e| format!("*.{e}")).collect();
                    c.args(["--getopenfilename", &home, &format!("{} ({what})", patterns.join(" "))]);
                }
            }
            c
        };
        for mut c in [zenity(), kdialog()] {
            let Ok(out) = c.output() else { continue };
            // Both exit with 1 when cancelled.
            if !out.status.success() {
                return Ok(None);
            }
            let path = String::from_utf8_lossy(&out.stdout).trim_end_matches('\n').to_string();
            return Ok((!path.is_empty()).then(|| PathBuf::from(path)));
        }
        Err(anyhow!(
            "No file picker found (install xdg-desktop-portal, zenity or kdialog); type the path instead"
        ))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn portal_links_become_paths() {
            assert_eq!(
                file_path("file:///home/me/My%20Music"),
                Some(PathBuf::from("/home/me/My Music"))
            );
            assert_eq!(file_path("https://example.com/a"), None);
        }
    }
}

//! New versions of Sumo, and settings files (export and import).

use super::*;

impl Service {
    // ---------------------------------------------------------------- updates

    pub(super) fn check_updates(&mut self, manual: bool) {
        if !manual && !self.cfg.updates.check {
            return;
        }
        {
            let mut feed = self.shared.feed.write().unwrap();
            if matches!(
                feed.update.status,
                UpdateStatus::Downloading(_) | UpdateStatus::Installing
            ) {
                return;
            }
            if manual {
                feed.update.status = UpdateStatus::Checking;
            }
        }
        self.shared.repaint();
        let repo = self.cfg.updates.repo.clone();
        let tx = self.internal_tx.clone();
        tokio::spawn(async move {
            let http = crate::updater::client();
            // Tests point this at a stand-in for GitHub.
            let api = std::env::var("MULTIMUSIC_UPDATE_API").unwrap_or_else(|_| GITHUB_API.to_string());
            let result = crate::updater::latest(&http, &api, &repo)
                .await
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(Internal::UpdateChecked { manual, result });
        });
    }

    pub(super) fn install_update(&mut self) {
        let release = self.shared.feed.read().unwrap().update.available.clone();
        let Some(release) = release else { return };
        let Some(asset) = release.asset.clone().filter(|_| crate::updater::can_install()) else {
            let why = if crate::updater::can_install() {
                "This release has no download for your system yet"
            } else {
                "Update with: cd ~/localmusicplayer && git pull && makepkg -sif"
            };
            self.shared.feed.write().unwrap().update.status = UpdateStatus::Failed(why.into());
            return;
        };
        self.shared.feed.write().unwrap().update.status = UpdateStatus::Downloading(0.0);
        self.shared.repaint();
        let shared = self.shared.clone();
        let tx = self.internal_tx.clone();
        let cache = self.paths.cache_dir.clone();
        tokio::spawn(async move {
            let result = async {
                let http = crate::updater::client();
                let dir = tokio::task::spawn_blocking(move || crate::updater::fresh_download_dir(&cache)).await??;
                let file = dir.join(&asset.name);
                let last = std::sync::atomic::AtomicU32::new(0);
                crate::updater::download(&http, &asset, &file, |done| {
                    let percent = (done * 100.0) as u32;
                    if last.swap(percent, std::sync::atomic::Ordering::Relaxed) != percent {
                        shared.feed.write().unwrap().update.status = UpdateStatus::Downloading(done);
                        shared.repaint();
                    }
                })
                .await?;
                let sums = release
                    .sums
                    .as_ref()
                    .ok_or_else(|| anyhow!("the release has no checksums, so it can't be checked"))?;
                let sums_file = dir.join(crate::updater::SUMS_FILE);
                crate::updater::download(&http, sums, &sums_file, |_| {}).await?;
                let sums_text = tokio::fs::read_to_string(&sums_file).await?;
                let name = asset.name.clone();
                let installed = tokio::task::spawn_blocking(move || {
                    crate::updater::verify(&file, &name, &sums_text)?;
                    if crate::updater::NEEDS_PASSWORD {
                        // Installed once the password is in (`authorize_update`).
                        return Ok(Some(file));
                    }
                    crate::updater::install(&file).map(|()| None)
                })
                .await?;
                installed
            }
            .await;
            match result {
                Ok(Some(file)) => {
                    let _ = tx.send(Internal::UpdateDownloaded(file));
                }
                Ok(None) => {
                    let mut feed = shared.feed.write().unwrap();
                    feed.update.status = UpdateStatus::Installing;
                    // Quit so the new version can take over (it starts by itself).
                    feed.quit = true;
                    drop(feed);
                    shared.repaint();
                }
                Err(e) => {
                    let _ = tx.send(Internal::UpdateFailed(format!("{e:#}")));
                }
            }
        });
    }

    /// Asks the UI for the password to install the downloaded update.
    pub(super) fn ask_update_password(&mut self, wrong: bool) {
        self.update_attempt += 1;
        self.shared.feed.write().unwrap().update.status = UpdateStatus::NeedsPassword {
            attempt: self.update_attempt,
            wrong,
        };
        self.shared.repaint();
    }

    /// Installs the downloaded update with the password (Linux: sudo pacman -U).
    pub(super) fn authorize_update(&mut self, password: crate::updater::Secret) {
        let Some(file) = self.update_package.clone() else {
            return;
        };
        self.shared.feed.write().unwrap().update.status = UpdateStatus::Installing;
        self.shared.repaint();
        let tx = self.internal_tx.clone();
        tokio::task::spawn_blocking(move || {
            let result = crate::updater::install_package(&file, &password).map_err(|e| match e {
                crate::updater::InstallError::WrongPassword => None,
                crate::updater::InstallError::Failed(why) => Some(why),
            });
            drop(password);
            let _ = tx.send(Internal::UpdateInstalled(result));
        });
    }

    pub(super) fn update_installed(&mut self, result: std::result::Result<(), Option<String>>) {
        match result {
            Ok(()) => {
                if let Some(file) = self.update_package.take() {
                    let _ = std::fs::remove_file(file);
                }
                #[cfg(unix)]
                let restarted = crate::updater::relaunch();
                #[cfg(not(unix))]
                let restarted: Result<()> = Err(anyhow!("can't restart here"));
                let mut feed = self.shared.feed.write().unwrap();
                match restarted {
                    Ok(()) => {
                        tracing::info!("update installed; starting again");
                        feed.update.status = UpdateStatus::Installing;
                        feed.quit = true;
                    }
                    Err(e) => {
                        tracing::warn!("update installed, but couldn't start again: {e:#}");
                        feed.update.status = UpdateStatus::Installed;
                    }
                }
                drop(feed);
                self.shared.repaint();
            }
            Err(None) => self.ask_update_password(true),
            Err(Some(why)) => {
                self.shared.feed.write().unwrap().update.status = UpdateStatus::Failed(why);
                self.shared.repaint();
            }
        }
    }

    // ---------------------------------------------------------------- settings files

    pub(super) fn export_settings(&mut self, path: &Path, include: crate::backup::Include) {
        let file = {
            let lib = self.shared.library.read().unwrap();
            crate::backup::build(&self.cfg, &self.paths.spotify_dir(), &lib, include)
        };
        match crate::backup::write(path, &file) {
            Ok(()) => {
                self.shared.feed.write().unwrap().exported_settings = Some(path.to_path_buf());
                let keys = if include.keys {
                    ". It holds your keys and logins: keep it private"
                } else {
                    ""
                };
                self.shared
                    .info(format!("Saved your settings to {}{keys}", path.display()));
            }
            Err(e) => self.shared.error(format!("Couldn't save your settings: {e:#}")),
        }
    }

    pub(super) fn import_settings(&mut self, path: &Path) {
        let file = match crate::backup::read(path) {
            Ok(file) => file,
            Err(e) => {
                self.shared.error(format!("Couldn't import settings: {e:#}"));
                return;
            }
        };
        let cfg = crate::backup::merge_config(&self.cfg, &file);
        if file.has_keys {
            let dir = self.paths.spotify_dir();
            let _ = std::fs::create_dir_all(&dir);
            for (name, text) in &file.logins {
                if !crate::backup::LOGIN_FILES.contains(&name.as_str()) {
                    continue;
                }
                let target = dir.join(name);
                if let Err(e) = crate::config::write_private(&target, text) {
                    self.shared.error(format!("Couldn't restore the Spotify login: {e}"));
                }
            }
        }
        if !file.playlists.is_empty() {
            let (playlists, tracks) = {
                let lib = self.shared.library.read().unwrap();
                crate::backup::merge_playlists(&lib, &file)
            };
            self.store_tracks(&tracks);
            self.save_playlists(playlists, &[]);
        }
        self.cfg = cfg.clone();
        if let Err(e) = self.cfg.save(&self.paths) {
            self.shared.error(format!("Couldn't save the imported settings: {e:#}"));
            return;
        }
        // Logins and connections are set up from scratch on the next start.
        self.shared.feed.write().unwrap().imported_settings = Some(Box::new(cfg));
        self.shared.repaint();
    }
}

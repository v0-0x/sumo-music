//! Cover art: loaded off-thread, downscaled to the size actually drawn, and kept in a
//! small LRU of GPU textures so memory stays flat no matter how big the library is.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::Arc;

use egui::{Color32, ColorImage, TextureHandle, TextureOptions};
use md5::{Digest, Md5};

/// Thumbnail used in lists.
pub const THUMB: u32 = 96;
/// Grid tiles and headers.
pub const MEDIUM: u32 = 320;
/// Now playing view.
pub const LARGE: u32 = 640;

type Key = (String, u32);

struct Decoded {
    key: Key,
    image: Option<ColorImage>,
    accent: Option<Color32>,
}

pub struct ArtCache {
    textures: HashMap<Key, TextureHandle>,
    accents: HashMap<String, Color32>,
    failed: HashSet<Key>,
    pending: HashSet<Key>,
    lru: VecDeque<Key>,
    capacity: usize,
    tx: mpsc::Sender<Decoded>,
    rx: mpsc::Receiver<Decoded>,
    rt: tokio::runtime::Handle,
    http: reqwest::Client,
    disk: Arc<PathBuf>,
    /// Limits concurrent decodes so scrolling a huge list doesn't spike memory.
    permits: Arc<tokio::sync::Semaphore>,
}

impl ArtCache {
    pub fn new(rt: tokio::runtime::Handle, disk: PathBuf, capacity: usize) -> ArtCache {
        let _ = std::fs::create_dir_all(&disk);
        let (tx, rx) = mpsc::channel();
        ArtCache {
            textures: HashMap::new(),
            accents: HashMap::new(),
            failed: HashSet::new(),
            pending: HashSet::new(),
            lru: VecDeque::new(),
            capacity: capacity.max(32),
            tx,
            rx,
            rt,
            http: crate::http::client(),
            disk: Arc::new(disk),
            permits: Arc::new(tokio::sync::Semaphore::new(3)),
        }
    }

    /// Uploads finished decodes. Call once per frame.
    pub fn poll(&mut self, ctx: &egui::Context) {
        while let Ok(d) = self.rx.try_recv() {
            self.pending.remove(&d.key);
            if let Some(c) = d.accent {
                self.accents.insert(d.key.0.clone(), c);
            }
            match d.image {
                Some(img) => {
                    let name = format!("art:{}:{}", d.key.1, d.key.0);
                    let tex = ctx.load_texture(name, img, TextureOptions::LINEAR);
                    self.textures.insert(d.key.clone(), tex);
                    self.lru.push_back(d.key);
                    self.evict();
                }
                None => {
                    self.failed.insert(d.key);
                }
            }
            ctx.request_repaint();
        }
    }

    fn evict(&mut self) {
        while self.textures.len() > self.capacity {
            let Some(old) = self.lru.pop_front() else { break };
            self.textures.remove(&old);
        }
    }

    fn touch(&mut self, key: &Key) {
        // Cheap LRU: move to back if it's not already among the newest entries.
        if self.lru.iter().rev().take(16).any(|k| k == key) {
            return;
        }
        if let Some(i) = self.lru.iter().position(|k| k == key) {
            self.lru.remove(i);
        }
        self.lru.push_back(key.clone());
    }

    /// Returns the texture if loaded, otherwise starts loading it.
    pub fn get(&mut self, src: Option<&str>, size: u32) -> Option<TextureHandle> {
        let src = src?;
        if src.is_empty() {
            return None;
        }
        let key = (src.to_string(), size);
        if let Some(t) = self.textures.get(&key).cloned() {
            self.touch(&key);
            return Some(t);
        }
        if self.failed.contains(&key) || self.pending.contains(&key) {
            return None;
        }
        self.pending.insert(key.clone());
        let tx = self.tx.clone();
        let http = self.http.clone();
        let disk = self.disk.clone();
        let permits = self.permits.clone();
        self.rt.spawn(async move {
            let _permit = permits.acquire().await;
            let bytes = load_bytes(&http, &disk, &key.0).await;
            let decoded = match bytes {
                Some(b) => tokio::task::spawn_blocking(move || decode(key, &b)).await.ok(),
                None => Some(Decoded {
                    key,
                    image: None,
                    accent: None,
                }),
            };
            if let Some(d) = decoded {
                let _ = tx.send(d);
            }
        });
        None
    }

    /// Drops an image (all sizes), so it is loaded again next time (it changed).
    pub fn forget(&mut self, src: &str) {
        let gone = |key: &Key| key.0 == src;
        self.textures.retain(|k, _| !gone(k));
        self.failed.retain(|k| !gone(k));
        self.lru.retain(|k| !gone(k));
        self.accents.remove(src);
    }

    pub fn len(&self) -> usize {
        self.textures.len()
    }

    /// Dominant colour of an image that was loaded at any size.
    pub fn accent(&self, src: &str) -> Option<Color32> {
        self.accents.get(src).copied()
    }
}

async fn load_bytes(http: &reqwest::Client, disk: &Path, src: &str) -> Option<Vec<u8>> {
    if src.starts_with("http://") || src.starts_with("https://") {
        let name: String = Md5::digest(src.as_bytes()).iter().map(|b| format!("{b:02x}")).collect();
        let file = disk.join(name);
        if let Ok(b) = tokio::fs::read(&file).await {
            return Some(b);
        }
        let bytes = crate::http::get_image(http, src).await.ok()?;
        let _ = tokio::fs::write(&file, &bytes).await;
        return Some(bytes);
    }
    let path = PathBuf::from(src.strip_prefix("file://").unwrap_or(src));
    let lower = src.to_ascii_lowercase();
    if [".jpg", ".jpeg", ".png", ".webp"].iter().any(|e| lower.ends_with(e)) {
        return tokio::fs::read(&path).await.ok();
    }
    // An audio file: read its embedded front cover.
    tokio::task::spawn_blocking(move || embedded_picture(&path))
        .await
        .ok()
        .flatten()
}

fn embedded_picture(path: &Path) -> Option<Vec<u8>> {
    use lofty::picture::PictureType;
    use lofty::prelude::*;
    let file = lofty::read_from_path(path).ok()?;
    let mut best: Option<Vec<u8>> = None;
    for tag in file.tags() {
        for pic in tag.pictures() {
            if pic.pic_type() == PictureType::CoverFront {
                return Some(pic.data().to_vec());
            }
            if best.is_none() {
                best = Some(pic.data().to_vec());
            }
        }
    }
    best
}

fn decode(key: Key, bytes: &[u8]) -> Decoded {
    let size = key.1;
    let Ok(img) = image::load_from_memory(bytes) else {
        return Decoded {
            key,
            image: None,
            accent: None,
        };
    };
    // Square-crop like every music app, then shrink to the requested size. The crop is a view
    // and the shrink reads it directly: a 3000 px cover is never copied at full size.
    let (w, h) = (img.width(), img.height());
    let side = w.min(h);
    let (x, y) = ((w - side) / 2, (h - side) / 2);
    let target = size.min(side);
    let rgba = match &img {
        image::DynamicImage::ImageRgb8(rgb) => {
            let small = image::imageops::thumbnail(&*image::imageops::crop_imm(rgb, x, y, side, side), target, target);
            image::DynamicImage::ImageRgb8(small).to_rgba8()
        }
        image::DynamicImage::ImageRgba8(rgba) => {
            image::imageops::thumbnail(&*image::imageops::crop_imm(rgba, x, y, side, side), target, target)
        }
        other => image::imageops::thumbnail(&*image::imageops::crop_imm(other, x, y, side, side), target, target),
    };
    drop(img);
    let accent = dominant_color(&rgba);
    let image = ColorImage::from_rgba_unmultiplied([rgba.width() as usize, rgba.height() as usize], rgba.as_raw());
    Decoded {
        key,
        image: Some(image),
        accent,
    }
}

/// Saturation-weighted average colour, nudged to a usable UI accent.
pub fn dominant_color(img: &image::RgbaImage) -> Option<Color32> {
    let (mut r, mut g, mut b, mut wsum) = (0f64, 0f64, 0f64, 0f64);
    let step = ((img.width() * img.height()) / 2048).max(1) as usize;
    for p in img.pixels().step_by(step) {
        let [pr, pg, pb, _] = p.0;
        let (fr, fg, fb) = (pr as f64 / 255.0, pg as f64 / 255.0, pb as f64 / 255.0);
        let max = fr.max(fg).max(fb);
        let min = fr.min(fg).min(fb);
        let sat = if max > 0.0 { (max - min) / max } else { 0.0 };
        // Ignore near-black/near-white pixels, prefer colourful ones.
        if !(0.12..=0.97).contains(&max) {
            continue;
        }
        let w = 0.05 + sat * sat;
        r += fr * w;
        g += fg * w;
        b += fb * w;
        wsum += w;
    }
    if wsum <= 0.0 {
        return None;
    }
    let (r, g, b) = (r / wsum, g / wsum, b / wsum);
    let (h, s, l) = rgb_to_hsl(r, g, b);
    // Keep it vivid enough and readable on a dark background.
    let (s, l) = (s.clamp(0.35, 0.85), l.clamp(0.55, 0.68));
    let (r, g, b) = hsl_to_rgb(h, s, l);
    Some(Color32::from_rgb(
        (r * 255.0) as u8,
        (g * 255.0) as u8,
        (b * 255.0) as u8,
    ))
}

fn rgb_to_hsl(r: f64, g: f64, b: f64) -> (f64, f64, f64) {
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    if (max - min).abs() < 1e-9 {
        return (0.0, 0.0, l);
    }
    let d = max - min;
    let s = if l > 0.5 {
        d / (2.0 - max - min)
    } else {
        d / (max + min)
    };
    let h = if max == r {
        (g - b) / d + if g < b { 6.0 } else { 0.0 }
    } else if max == g {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    };
    (h / 6.0, s, l)
}

fn hsl_to_rgb(h: f64, s: f64, l: f64) -> (f64, f64, f64) {
    if s == 0.0 {
        return (l, l, l);
    }
    let q = if l < 0.5 { l * (1.0 + s) } else { l + s - l * s };
    let p = 2.0 * l - q;
    let f = |mut t: f64| {
        if t < 0.0 {
            t += 1.0;
        }
        if t > 1.0 {
            t -= 1.0;
        }
        if t < 1.0 / 6.0 {
            p + (q - p) * 6.0 * t
        } else if t < 0.5 {
            q
        } else if t < 2.0 / 3.0 {
            p + (q - p) * (2.0 / 3.0 - t) * 6.0
        } else {
            p
        }
    };
    (f(h + 1.0 / 3.0), f(h), f(h - 1.0 / 3.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dominant_color_prefers_saturated_pixels() {
        let mut img = image::RgbaImage::new(10, 10);
        for (i, p) in img.pixels_mut().enumerate() {
            *p = if i % 4 == 0 {
                image::Rgba([220, 30, 30, 255])
            } else {
                image::Rgba([90, 90, 90, 255])
            };
        }
        let c = dominant_color(&img).unwrap();
        assert!(c.r() > c.g() && c.r() > c.b(), "{c:?}");
    }

    #[test]
    fn hsl_roundtrip() {
        let (h, s, l) = rgb_to_hsl(0.2, 0.4, 0.8);
        let (r, g, b) = hsl_to_rgb(h, s, l);
        assert!((r - 0.2).abs() < 1e-6 && (g - 0.4).abs() < 1e-6 && (b - 0.8).abs() < 1e-6);
    }

    #[test]
    fn decode_crops_and_scales() {
        let mut img = image::RgbaImage::new(200, 100);
        for p in img.pixels_mut() {
            *p = image::Rgba([10, 200, 10, 255]);
        }
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png)
            .unwrap();
        let d = decode(("x".into(), 64), &bytes);
        let image = d.image.unwrap();
        assert_eq!(image.size, [64, 64]);
        assert!(d.accent.is_some());
    }

    /// The crop-and-shrink without full-size copies gives exactly what copying did.
    #[test]
    fn decode_matches_the_copying_version() {
        let pattern = |x: u32, y: u32| [(x * 7 % 256) as u8, (y * 13 % 256) as u8, ((x + y) * 3 % 256) as u8];
        let rgb = image::RgbImage::from_fn(301, 173, |x, y| image::Rgb(pattern(x, y)));
        let rgba = image::RgbaImage::from_fn(120, 200, |x, y| {
            let [r, g, b] = pattern(x, y);
            image::Rgba([r, g, b, (x % 256) as u8])
        });
        let gray = image::GrayImage::from_fn(90, 90, |x, y| image::Luma([((x * y) % 256) as u8]));
        let images = [
            image::DynamicImage::ImageRgb8(rgb),
            image::DynamicImage::ImageRgba8(rgba),
            image::DynamicImage::ImageLuma8(gray),
        ];
        for img in images {
            let mut bytes = Vec::new();
            img.write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png)
                .unwrap();
            for size in [32, 96, 640] {
                let got = decode(("x".into(), size), &bytes).image.unwrap();
                let (w, h) = (img.width(), img.height());
                let side = w.min(h);
                let cropped = img.crop_imm((w - side) / 2, (h - side) / 2, side, side);
                let target = size.min(side);
                let want = image::imageops::thumbnail(&cropped.to_rgba8(), target, target);
                let want =
                    ColorImage::from_rgba_unmultiplied([want.width() as usize, want.height() as usize], want.as_raw());
                assert_eq!(got.size, want.size);
                assert!(got.pixels == want.pixels, "{:?} at {size}", img.color());
            }
        }
    }
}

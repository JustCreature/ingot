//! Turns JPEG bytes into textures off the UI thread, keeping recently used ones.

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex, mpsc},
};

use eframe::egui::{self, ColorImage, TextureHandle, TextureOptions};
use ingot_core::{AssetKey, preview};
use ingot_develop::orient_exif;

const DECODE_THREADS: usize = 3;
/// Textures kept before the least recently used go. Enough for two screens of grid.
const MAX_TEXTURES: usize = 400;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Tier {
    /// The tiny image in the file header, there immediately.
    Header,
    Grid,
    Preview,
}

struct Job {
    key: AssetKey,
    tier: Tier,
    bytes: Vec<u8>,
    orientation: u16,
}

struct Decoded {
    key: AssetKey,
    tier: Tier,
    image: Option<ColorImage>,
}

pub fn decode_jpeg(bytes: &[u8], orientation: u16) -> Result<ColorImage, String> {
    let image = preview::decompress(bytes, usize::MAX).map_err(|err| err.to_string())?;
    let upright = orient_exif(image.pixels, image.width, image.height, orientation);
    Ok(ColorImage::from_rgb(
        [upright.width, upright.height],
        &upright.rgb,
    ))
}

pub struct ImageCache {
    textures: HashMap<(AssetKey, Tier), (TextureHandle, u64)>,
    pending: HashSet<(AssetKey, Tier)>,
    failed: HashSet<(AssetKey, Tier)>,
    jobs: mpsc::Sender<Job>,
    done: mpsc::Receiver<Decoded>,
    clock: u64,
}

impl ImageCache {
    pub fn new(ctx: &egui::Context) -> Self {
        let (jobs, job_rx) = mpsc::channel::<Job>();
        let (done_tx, done) = mpsc::channel();
        let job_rx = Arc::new(Mutex::new(job_rx));
        for _ in 0..DECODE_THREADS {
            let (job_rx, done_tx, ctx) = (Arc::clone(&job_rx), done_tx.clone(), ctx.clone());
            std::thread::spawn(move || {
                loop {
                    let job = match job_rx.lock() {
                        Ok(rx) => rx.recv(),
                        Err(_) => return,
                    };
                    let Ok(job) = job else { return };
                    let image = decode_jpeg(&job.bytes, job.orientation).ok();
                    let sent = done_tx.send(Decoded {
                        key: job.key,
                        tier: job.tier,
                        image,
                    });
                    if sent.is_err() {
                        return;
                    }
                    ctx.request_repaint();
                }
            });
        }
        ImageCache {
            textures: HashMap::new(),
            pending: HashSet::new(),
            failed: HashSet::new(),
            jobs,
            done,
            clock: 0,
        }
    }

    /// Moves finished decodes to the GPU and drops the oldest textures.
    pub fn collect(&mut self, ctx: &egui::Context) {
        while let Ok(decoded) = self.done.try_recv() {
            let id = (decoded.key, decoded.tier);
            self.pending.remove(&id);
            match decoded.image {
                Some(image) => {
                    let name = format!("{:?}-{:?}", id.0.stem, id.1);
                    let texture = ctx.load_texture(name, image, TextureOptions::LINEAR);
                    self.clock += 1;
                    self.textures.insert(id, (texture, self.clock));
                }
                None => {
                    self.failed.insert(id);
                }
            }
        }
        if self.textures.len() > MAX_TEXTURES {
            let mut ages: Vec<u64> = self.textures.values().map(|(_, used)| *used).collect();
            ages.sort_unstable();
            let cutoff = ages[self.textures.len() - MAX_TEXTURES];
            self.textures.retain(|_, (_, used)| *used >= cutoff);
        }
    }

    /// The texture if it is ready. If not, `bytes` is called for the image, which is
    /// queued for decoding unless it has none yet.
    pub fn get(
        &mut self,
        key: &AssetKey,
        tier: Tier,
        orientation: u16,
        bytes: impl FnOnce() -> Option<Vec<u8>>,
    ) -> Option<TextureHandle> {
        let id = (key.clone(), tier);
        self.clock += 1;
        if let Some((texture, used)) = self.textures.get_mut(&id) {
            *used = self.clock;
            return Some(texture.clone());
        }
        if !self.pending.contains(&id)
            && !self.failed.contains(&id)
            && let Some(bytes) = bytes()
        {
            self.pending.insert(id);
            let _ = self.jobs.send(Job {
                key: key.clone(),
                tier,
                bytes,
                orientation,
            });
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jpeg(width: usize, height: usize) -> Vec<u8> {
        preview::compress(turbojpeg::Image {
            pixels: vec![128u8; width * height * 3],
            width,
            pitch: width * 3,
            height,
            format: turbojpeg::PixelFormat::RGB,
        })
        .unwrap()
    }

    #[test]
    fn decoding_applies_the_orientation() {
        let bytes = jpeg(40, 20);
        assert_eq!(decode_jpeg(&bytes, 1).unwrap().size, [40, 20]);
        assert_eq!(decode_jpeg(&bytes, 6).unwrap().size, [20, 40]);
        assert_eq!(decode_jpeg(&bytes, 8).unwrap().size, [20, 40]);
    }

    #[test]
    fn broken_bytes_are_an_error_not_a_panic() {
        assert!(decode_jpeg(b"not a jpeg", 1).is_err());
    }
}

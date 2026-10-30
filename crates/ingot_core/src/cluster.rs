//! Groups photos of the same scene, for culling.
//!
//! Every photo gets a [`Fingerprint`] from an image that is already in memory, so
//! clustering never reads the card. Grouping then walks the photos in capture order
//! and starts a new group where a photo differs too much from the one before it. It
//! is cheap enough to rerun on every move of the sensitivity slider, because a higher
//! sensitivity can only add breaks: groups only ever split as it rises.

use std::ops::Range;

use chrono::NaiveDateTime;

const HASH_SIDE: usize = 32;
const HASH_BITS_SIDE: usize = 8;
const COLOR_SIDE: usize = 4;
const COLOR_LEN: usize = COLOR_SIDE * COLOR_SIDE * 3;

/// Photos further apart than this are separate groups, however alike they look.
pub const MAX_GROUP_GAP_SECONDS: i64 = 30 * 60;
pub const DEFAULT_SENSITIVITY: f32 = 20.0;

/// What a photo looks like, in a form that compares quickly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fingerprint {
    /// A DCT perceptual hash: the structure of the image, so where things are.
    phash: u64,
    /// Average color of a 4x4 grid: the overall scene.
    color: [u8; COLOR_LEN],
}

/// Area-averages an image into `side * side` RGB cells.
fn shrink(rgb: &[u8], width: usize, height: usize, side: usize) -> Vec<[f32; 3]> {
    let mut sums = vec![[0.0f32; 3]; side * side];
    let mut counts = vec![0.0f32; side * side];
    for y in 0..height {
        let cell_y = y * side / height;
        for x in 0..width {
            let cell = cell_y * side + x * side / width;
            let at = (y * width + x) * 3;
            sums[cell][0] += f32::from(rgb[at]);
            sums[cell][1] += f32::from(rgb[at + 1]);
            sums[cell][2] += f32::from(rgb[at + 2]);
            counts[cell] += 1.0;
        }
    }
    for (sum, count) in sums.iter_mut().zip(&counts) {
        let count = count.max(1.0);
        sum[0] /= count;
        sum[1] /= count;
        sum[2] /= count;
    }
    sums
}

/// The classic pHash: the low frequencies of a 32x32 DCT, each compared with their
/// median. Brightness and small shifts barely move those frequencies.
fn perceptual_hash(gray: &[f32]) -> u64 {
    let n = HASH_SIDE;
    let cosines: Vec<f32> = (0..HASH_BITS_SIDE * n)
        .map(|i| {
            let (u, x) = (i / n, i % n);
            (std::f32::consts::PI * u as f32 * (2 * x + 1) as f32 / (2 * n) as f32).cos()
        })
        .collect();
    let cosine = |u: usize, x: usize| cosines[u * n + x];

    let mut rows = vec![0.0f32; HASH_BITS_SIDE * n];
    for y in 0..n {
        for u in 0..HASH_BITS_SIDE {
            rows[y * HASH_BITS_SIDE + u] = (0..n).map(|x| gray[y * n + x] * cosine(u, x)).sum();
        }
    }
    let mut coefficients = [0.0f32; HASH_BITS_SIDE * HASH_BITS_SIDE];
    for v in 0..HASH_BITS_SIDE {
        for u in 0..HASH_BITS_SIDE {
            coefficients[v * HASH_BITS_SIDE + u] = (0..n)
                .map(|y| rows[y * HASH_BITS_SIDE + u] * cosine(v, y))
                .sum();
        }
    }

    // The first coefficient is only the average brightness, so it can't vote.
    let mut sorted: Vec<f32> = coefficients[1..].to_vec();
    sorted.sort_by(f32::total_cmp);
    let median = sorted[sorted.len() / 2];
    coefficients
        .iter()
        .enumerate()
        .skip(1)
        .fold(0u64, |hash, (bit, value)| {
            if *value > median {
                hash | (1u64 << bit)
            } else {
                hash
            }
        })
}

impl Fingerprint {
    /// Fingerprints an interleaved 8-bit RGB image. `None` if it is empty or the
    /// buffer doesn't match its size.
    pub fn from_rgb(rgb: &[u8], width: usize, height: usize) -> Option<Self> {
        if width == 0 || height == 0 || rgb.len() != width * height * 3 {
            return None;
        }
        let gray: Vec<f32> = shrink(rgb, width, height, HASH_SIDE)
            .iter()
            .map(|p| 0.2126 * p[0] + 0.7152 * p[1] + 0.0722 * p[2])
            .collect();
        let mut color = [0u8; COLOR_LEN];
        for (cell, p) in shrink(rgb, width, height, COLOR_SIDE).iter().enumerate() {
            for c in 0..3 {
                color[cell * 3 + c] = p[c].round() as u8;
            }
        }
        Some(Fingerprint {
            phash: perceptual_hash(&gray),
            color,
        })
    }

    /// Differing structure bits, 0 to 63.
    pub fn structure_distance(&self, other: &Fingerprint) -> u32 {
        (self.phash ^ other.phash).count_ones()
    }

    /// Mean color difference, 0 to 1.
    pub fn color_distance(&self, other: &Fingerprint) -> f32 {
        let total: u32 = self
            .color
            .iter()
            .zip(&other.color)
            .map(|(a, b)| u32::from(a.abs_diff(*b)))
            .sum();
        total as f32 / (COLOR_LEN as f32 * 255.0)
    }
}

/// Whether two photos count as the same scene at a sensitivity from 0 to 100. Low
/// values compare only the scene's colors. Raising the value tightens that and brings
/// in structure, so where the subject stands starts to matter. Both limits only ever
/// tighten as sensitivity rises, which is what makes groups split and never merge.
pub fn looks_alike(a: &Fingerprint, b: &Fingerprint, sensitivity: f32) -> bool {
    let s = sensitivity.clamp(0.0, 100.0) / 100.0;
    let color_limit = 0.16 * (1.0 - s).powf(1.5);
    let structure_limit = 64.0 - 62.0 * s;
    a.color_distance(b) <= color_limit && a.structure_distance(b) as f32 <= structure_limit
}

/// One photo to group. A photo whose fingerprint isn't ready yet stays with the one
/// before it until it is.
#[derive(Clone, Copy, Debug)]
pub struct ClusterItem {
    pub taken: Option<NaiveDateTime>,
    pub fingerprint: Option<Fingerprint>,
}

/// Splits items, already in capture order, into groups of neighbors.
pub fn cluster_ranges(items: &[ClusterItem], sensitivity: f32) -> Vec<Range<usize>> {
    let mut groups = vec![];
    let mut start = 0;
    let mut last_seen: Option<Fingerprint> = None;
    for (index, item) in items.iter().enumerate() {
        let gap = index > 0
            && match (items[index - 1].taken, item.taken) {
                (Some(before), Some(now)) => {
                    (now - before).num_seconds().abs() > MAX_GROUP_GAP_SECONDS
                }
                _ => false,
            };
        let differs = match (&last_seen, &item.fingerprint) {
            (Some(before), Some(now)) => !looks_alike(before, now, sensitivity),
            _ => false,
        };
        if gap || differs {
            groups.push(start..index);
            start = index;
        }
        if gap {
            last_seen = None;
        }
        if item.fingerprint.is_some() {
            last_seen = item.fingerprint;
        }
    }
    if !items.is_empty() {
        groups.push(start..items.len());
    }
    groups
}

/// Groups items given in any order: they are put in capture order first, with the key
/// breaking ties, so the result doesn't depend on the order they arrived in.
pub fn cluster_any_order<K: Ord + Clone>(
    items: &[(K, ClusterItem)],
    sensitivity: f32,
) -> Vec<Vec<K>> {
    let mut sorted: Vec<&(K, ClusterItem)> = items.iter().collect();
    sorted.sort_by(|a, b| (a.1.taken, &a.0).cmp(&(b.1.taken, &b.0)));
    let ordered: Vec<ClusterItem> = sorted.iter().map(|(_, item)| *item).collect();
    cluster_ranges(&ordered, sensitivity)
        .into_iter()
        .map(|range| sorted[range].iter().map(|(key, _)| key.clone()).collect())
        .collect()
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use chrono::{Duration, NaiveDate};

    use super::*;
    use crate::preview;

    fn time(seconds: i64) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 5, 22)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap()
            + Duration::seconds(seconds)
    }

    /// A made-up scene: sky, ground and a red block at `subject_x`.
    fn scene(sky: [u8; 3], ground: [u8; 3], subject_x: usize, noise: u32) -> Vec<u8> {
        let (w, h) = (160, 120);
        let mut rgb = Vec::with_capacity(w * h * 3);
        let mut state = noise.wrapping_mul(2_654_435_761).wrapping_add(1);
        for y in 0..h {
            for x in 0..w {
                let mut color = if y < h / 2 { sky } else { ground };
                if (subject_x..subject_x + 30).contains(&x) && (40..100).contains(&y) {
                    color = [200, 40, 40];
                }
                for value in color {
                    state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    let wobble = if noise == 0 {
                        0
                    } else {
                        (state >> 28) as i32 - 8
                    };
                    rgb.push((i32::from(value) + wobble).clamp(0, 255) as u8);
                }
            }
        }
        rgb
    }

    fn print(rgb: &[u8]) -> Fingerprint {
        Fingerprint::from_rgb(rgb, 160, 120).unwrap()
    }

    fn item(seconds: i64, rgb: &[u8]) -> ClusterItem {
        ClusterItem {
            taken: Some(time(seconds)),
            fingerprint: Some(print(rgb)),
        }
    }

    const SNOW: ([u8; 3], [u8; 3]) = ([210, 215, 225], [240, 240, 245]);
    const FOREST: ([u8; 3], [u8; 3]) = ([40, 90, 50], [60, 45, 30]);

    fn sizes(groups: &[Range<usize>]) -> Vec<usize> {
        groups.iter().map(Range::len).collect()
    }

    #[test]
    fn near_duplicates_group_and_different_scenes_do_not() {
        let items = [
            item(0, &scene(SNOW.0, SNOW.1, 50, 1)),
            item(1, &scene(SNOW.0, SNOW.1, 50, 2)),
            item(2, &scene(SNOW.0, SNOW.1, 52, 3)),
            item(60, &scene(FOREST.0, FOREST.1, 90, 4)),
            item(61, &scene(FOREST.0, FOREST.1, 90, 5)),
        ];
        assert_eq!(sizes(&cluster_ranges(&items, DEFAULT_SENSITIVITY)), [3, 2]);
    }

    #[test]
    fn identical_images_are_alike_at_every_sensitivity() {
        let image = scene(SNOW.0, SNOW.1, 50, 0);
        for s in [0.0, 20.0, 50.0, 100.0] {
            assert!(looks_alike(&print(&image), &print(&image), s), "{s}");
        }
        assert_eq!(print(&image).structure_distance(&print(&image)), 0);
    }

    #[test]
    fn raising_sensitivity_only_ever_splits_groups() {
        let items: Vec<ClusterItem> = (0..24)
            .map(|i| {
                let scenes = [SNOW, FOREST];
                let (a, b) = scenes[i / 12];
                item(i as i64, &scene(a, b, 10 + i * 5, i as u32 + 1))
            })
            .collect();
        let mut previous = cluster_ranges(&items, 0.0);
        for step in 1..=100 {
            let groups = cluster_ranges(&items, step as f32);
            assert!(groups.len() >= previous.len(), "at {step}");
            // Every new group sits entirely inside one old group.
            for group in &groups {
                assert!(
                    previous
                        .iter()
                        .any(|old| old.start <= group.start && group.end <= old.end),
                    "at {step}: {group:?} crosses {previous:?}"
                );
            }
            previous = groups;
        }
    }

    #[test]
    fn low_sensitivity_groups_a_moving_subject_and_high_splits_it() {
        let items = [
            item(0, &scene(SNOW.0, SNOW.1, 5, 1)),
            item(1, &scene(SNOW.0, SNOW.1, 120, 2)),
        ];
        assert_eq!(cluster_ranges(&items, 0.0).len(), 1);
        assert_eq!(cluster_ranges(&items, 100.0).len(), 2);
    }

    #[test]
    fn result_does_not_depend_on_arrival_order() {
        let pictures = [
            (0, scene(SNOW.0, SNOW.1, 50, 1)),
            (1, scene(SNOW.0, SNOW.1, 50, 2)),
            (2, scene(FOREST.0, FOREST.1, 90, 3)),
            (2, scene(FOREST.0, FOREST.1, 90, 4)),
            (9, scene(SNOW.0, SNOW.1, 50, 5)),
        ];
        let items: Vec<(usize, ClusterItem)> = pictures
            .iter()
            .enumerate()
            .map(|(id, (seconds, rgb))| (id, item(*seconds, rgb)))
            .collect();
        let expected = cluster_any_order(&items, DEFAULT_SENSITIVITY);
        assert_eq!(expected, [vec![0, 1], vec![2, 3], vec![4]]);

        let mut reversed = items.clone();
        reversed.reverse();
        assert_eq!(cluster_any_order(&reversed, DEFAULT_SENSITIVITY), expected);
        let mut rotated = items;
        rotated.rotate_left(2);
        assert_eq!(cluster_any_order(&rotated, DEFAULT_SENSITIVITY), expected);
    }

    #[test]
    fn a_burst_stays_together_across_a_pause_but_not_across_a_long_gap() {
        let same = scene(SNOW.0, SNOW.1, 50, 1);
        let pause = [item(0, &same), item(300, &same), item(600, &same)];
        assert_eq!(sizes(&cluster_ranges(&pause, DEFAULT_SENSITIVITY)), [3]);

        let gap = MAX_GROUP_GAP_SECONDS + 1;
        let split = [item(0, &same), item(gap, &same)];
        assert_eq!(sizes(&cluster_ranges(&split, DEFAULT_SENSITIVITY)), [1, 1]);
    }

    #[test]
    fn empty_single_and_undated_inputs_are_fine() {
        assert!(cluster_ranges(&[], DEFAULT_SENSITIVITY).is_empty());
        let one = [item(0, &scene(SNOW.0, SNOW.1, 50, 1))];
        assert_eq!(sizes(&cluster_ranges(&one, DEFAULT_SENSITIVITY)), [1]);

        let mut undated = one[0];
        undated.taken = None;
        let two = [undated, undated];
        assert_eq!(sizes(&cluster_ranges(&two, DEFAULT_SENSITIVITY)), [2]);
    }

    #[test]
    fn photos_still_waiting_for_a_fingerprint_stay_with_their_neighbors() {
        let snow = item(0, &scene(SNOW.0, SNOW.1, 50, 1));
        let waiting = ClusterItem {
            taken: Some(time(1)),
            fingerprint: None,
        };
        let forest = item(2, &scene(FOREST.0, FOREST.1, 90, 2));
        // The comparison skips the gap and still sees the change of scene.
        assert_eq!(
            sizes(&cluster_ranges(
                &[snow, waiting, forest],
                DEFAULT_SENSITIVITY
            )),
            [2, 1]
        );
        assert_eq!(sizes(&cluster_ranges(&[waiting, waiting], 100.0)), [2]);
    }

    #[test]
    fn bad_image_buffers_give_no_fingerprint() {
        assert!(Fingerprint::from_rgb(&[], 0, 0).is_none());
        assert!(Fingerprint::from_rgb(&[1, 2, 3], 2, 2).is_none());
    }

    fn fixture_rgb(name: &str) -> (Vec<u8>, usize, usize) {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("testdata/test_exif_read")
            .join(name);
        let bytes = std::fs::read(path).unwrap();
        let image = preview::decompress(&bytes, 512).unwrap();
        (image.pixels, image.width, image.height)
    }

    /// Slightly brighter and shifted right, like the next frame of a handheld burst.
    fn next_frame(rgb: &[u8], width: usize, height: usize) -> Vec<u8> {
        let shift = width / 100;
        let mut out = vec![0u8; rgb.len()];
        for y in 0..height {
            for x in 0..width {
                let from = (y * width + x.saturating_sub(shift)) * 3;
                let to = (y * width + x) * 3;
                for c in 0..3 {
                    out[to + c] = (f32::from(rgb[from + c]) * 1.03).min(255.0) as u8;
                }
            }
        }
        out
    }

    #[test]
    fn real_photos_a_burst_groups_and_different_shots_do_not() {
        let (a, w, h) = fixture_rgb("IMG_1800.JPG");
        let (b, bw, bh) = fixture_rgb("IMG_1868.JPG");
        let first = Fingerprint::from_rgb(&a, w, h).unwrap();
        let burst = Fingerprint::from_rgb(&next_frame(&a, w, h), w, h).unwrap();
        let other = Fingerprint::from_rgb(&b, bw, bh).unwrap();
        println!(
            "burst: color {:.3} structure {}; other: color {:.3} structure {}",
            first.color_distance(&burst),
            first.structure_distance(&burst),
            first.color_distance(&other),
            first.structure_distance(&other),
        );

        assert!(looks_alike(&first, &burst, DEFAULT_SENSITIVITY));
        assert!(looks_alike(&first, &burst, 60.0));
        assert!(first.structure_distance(&burst) < first.structure_distance(&other));
        assert!(first.color_distance(&burst) < first.color_distance(&other));
        assert!(!looks_alike(&first, &other, DEFAULT_SENSITIVITY));
    }
}

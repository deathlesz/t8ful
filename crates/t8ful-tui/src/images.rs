//! Terminal image previews.
//!
//! Decoding and protocol encoding run on at most two worker threads, never on
//! the UI thread. The UI keeps a bounded cache of encoded previews and only
//! asks for a new one when a transcript image becomes visible in the layout.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};

use ratatui::layout::Size;
use ratatui_image::Resize;
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::protocol::Protocol;

/// Previews never grow beyond these bounds, even on a wide terminal.
pub const MAX_PREVIEW_COLUMNS: u16 = 100;
pub const MAX_PREVIEW_ROWS: u16 = 24;
/// A single decoded image may not exceed this much RGBA memory.
pub const MAX_DECODED_BYTES: u64 = 64 * 1024 * 1024;
/// The encoded-preview LRU is bounded by the same budget.
pub const CACHE_BYTES: usize = 64 * 1024 * 1024;

const WORKERS: usize = 2;

/// Identity of one preview: the session-owned file and the cell rectangle it
/// was encoded for. A resize creates a new key rather than mutating a protocol
/// the renderer may still be holding.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PreviewKey {
    pub path: PathBuf,
    pub columns: u16,
    pub rows: u16,
}

/// The result of one background decode.
pub enum PreviewOutcome {
    Ready {
        protocol: Protocol,
        decoded_bytes: u64,
    },
    Failed(String),
}

struct Job {
    epoch: u64,
    key: PreviewKey,
    picker: Picker,
}

struct PreviewResult {
    epoch: u64,
    key: PreviewKey,
    outcome: PreviewOutcome,
}

struct CacheEntry {
    outcome: PreviewOutcome,
    cost: usize,
}

/// The environment inputs that decide a graphics protocol.
///
/// Kept separate from the process environment so the decision is a pure,
/// testable function. [`ImageEnv::from_environment`] is the only reader.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImageEnv {
    pub protocol: Option<String>,
    pub term: String,
    pub term_program: String,
    pub lc_terminal: String,
    pub tmux: bool,
    pub kitty_window: bool,
}

impl ImageEnv {
    pub fn from_environment() -> Self {
        Self {
            protocol: std::env::var("T8FUL_IMAGE_PROTOCOL").ok(),
            term: std::env::var("TERM").unwrap_or_default(),
            term_program: std::env::var("TERM_PROGRAM").unwrap_or_default(),
            lc_terminal: std::env::var("LC_TERMINAL").unwrap_or_default(),
            tmux: std::env::var_os("TMUX").is_some(),
            kitty_window: std::env::var_os("KITTY_WINDOW_ID").is_some(),
        }
    }
}

/// Pick a graphics protocol from the environment instead of querying stdin.
///
/// The upstream query blocks a raw stdin read in a detached thread; when a
/// terminal does not answer, that thread keeps competing with the event reader
/// after the timeout. Environment detection is bounded by construction and
/// falls back to halfblocks.
pub fn picker_from_environment() -> Picker {
    picker_for(&ImageEnv::from_environment())
}

/// Build the picker for an environment.
///
/// The protocol choice is [`protocol_for`]. The picker constructor still reads
/// the process environment for its tmux flag (ratatui-image offers no setter),
/// but tests exercise `protocol_for` so they never query the developer's tmux.
pub fn picker_for(env: &ImageEnv) -> Picker {
    let mut picker = Picker::halfblocks();

    picker.set_protocol_type(protocol_for(env));

    picker
}

/// The pure protocol decision.
fn protocol_for(env: &ImageEnv) -> ProtocolType {
    if let Some(value) = env.protocol.as_deref() {
        return match value.trim().to_ascii_lowercase().as_str() {
            "kitty" => ProtocolType::Kitty,
            "sixel" => ProtocolType::Sixel,
            "iterm2" => ProtocolType::Iterm2,
            _ => ProtocolType::Halfblocks,
        };
    }

    // ratatui-image wraps graphics in the tmux passthrough sequence from
    // exactly this predicate. Enabling graphics while tmux is present but the
    // predicate does not hold would emit an unwrapped kitty/sixel sequence and
    // garble the screen, so the two decisions must stay in sync.
    let tmux_wrap = env.term.starts_with("tmux") || env.term_program == "tmux";

    if env.tmux && !tmux_wrap {
        return ProtocolType::Halfblocks;
    }

    let term = env.term.to_ascii_lowercase();
    let program = env.term_program.to_ascii_lowercase();
    let lc_terminal = env.lc_terminal.to_ascii_lowercase();
    let kitty = term.contains("kitty")
        || program.contains("kitty")
        || lc_terminal.contains("kitty")
        || (!env.tmux && env.kitty_window);

    if kitty {
        ProtocolType::Kitty
    } else if term.contains("sixel") {
        ProtocolType::Sixel
    } else if program.contains("iterm") || lc_terminal.contains("iterm") {
        ProtocolType::Iterm2
    } else {
        ProtocolType::Halfblocks
    }
}

/// Cache and worker pool for transcript image previews.
pub struct ImagePreviews {
    picker: Picker,
    jobs: Vec<Sender<Job>>,
    next_job: usize,
    results: Receiver<PreviewResult>,
    cache: HashMap<PreviewKey, CacheEntry>,
    order: VecDeque<PreviewKey>,
    cost: usize,
    pending: HashSet<PreviewKey>,
    epoch: u64,
}

impl std::fmt::Debug for ImagePreviews {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImagePreviews")
            .field("cached", &self.cache.len())
            .field("pending", &self.pending.len())
            .field("epoch", &self.epoch)
            .finish()
    }
}

impl Default for ImagePreviews {
    fn default() -> Self {
        Self::new()
    }
}

impl ImagePreviews {
    pub fn new() -> Self {
        let (result_tx, results) = channel();
        let mut jobs = Vec::with_capacity(WORKERS);

        for index in 0..WORKERS {
            let (job_tx, job_rx) = channel::<Job>();
            let result_tx = result_tx.clone();

            // A worker that cannot start simply leaves the other one; its
            // sender stays alive and jobs sent there are never answered. That
            // is still a bounded fallback: the UI shows a placeholder forever
            // rather than blocking.
            let _ = std::thread::Builder::new()
                .name(format!("t8ful-image-{index}"))
                .spawn(move || {
                    while let Ok(job) = job_rx.recv() {
                        let outcome = decode_preview(&job);
                        let _ = result_tx.send(PreviewResult {
                            epoch: job.epoch,
                            key: job.key,
                            outcome,
                        });
                    }
                });
            jobs.push(job_tx);
        }

        Self {
            picker: Picker::halfblocks(),
            jobs,
            next_job: 0,
            results,
            cache: HashMap::new(),
            order: VecDeque::new(),
            cost: 0,
            pending: HashSet::new(),
            epoch: 0,
        }
    }

    /// Replace the detected picker. Previews encoded for the previous picker
    /// are dropped, and in-flight work is ignored.
    pub fn set_picker(&mut self, picker: Picker) {
        let current = self.picker.font_size();
        let next = picker.font_size();
        let changed = picker.protocol_type() != self.picker.protocol_type()
            || next.width != current.width
            || next.height != current.height;

        self.picker = picker;

        if changed {
            self.reset();
        }
    }

    pub fn picker(&self) -> &Picker {
        &self.picker
    }

    pub fn font_size(&self) -> (u16, u16) {
        let size = self.picker.font_size();

        (size.width, size.height)
    }

    /// A session change invalidates every preview and every late worker result.
    pub fn invalidate(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        self.reset();
    }

    fn reset(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        self.cache.clear();
        self.order.clear();
        self.cost = 0;
        self.pending.clear();
    }

    /// Ask for a preview unless it is cached or already in flight.
    pub fn request(&mut self, path: &Path, columns: u16, rows: u16) -> PreviewKey {
        let key = PreviewKey {
            path: path.to_path_buf(),
            columns: columns.max(1),
            rows: rows.max(1),
        };

        if self.cache.contains_key(&key) || self.pending.contains(&key) || self.jobs.is_empty() {
            return key;
        }

        self.pending.insert(key.clone());
        let job = Job {
            epoch: self.epoch,
            key: key.clone(),
            picker: self.picker.clone(),
        };
        let sender = &self.jobs[self.next_job % self.jobs.len()];

        self.next_job = self.next_job.wrapping_add(1);

        if sender.send(job).is_err() {
            self.pending.remove(&key);
        }

        key
    }

    pub fn outcome(&self, key: &PreviewKey) -> Option<&PreviewOutcome> {
        self.cache.get(key).map(|entry| &entry.outcome)
    }

    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Drain finished decodes. Returns true when the cache changed and the
    /// affected entry layouts must be rebuilt.
    pub fn apply_results(&mut self) -> bool {
        let mut changed = false;

        while let Ok(result) = self.results.try_recv() {
            self.pending.remove(&result.key);

            if result.epoch != self.epoch {
                continue;
            }

            let cost = match &result.outcome {
                PreviewOutcome::Ready { decoded_bytes, .. } => *decoded_bytes as usize,
                PreviewOutcome::Failed(_) => 0,
            };

            self.store(result.key, result.outcome, cost);
            changed = true;
        }

        changed
    }

    fn store(&mut self, key: PreviewKey, outcome: PreviewOutcome, cost: usize) {
        if self.cache.contains_key(&key) {
            return;
        }

        self.cost = self.cost.saturating_add(cost);
        self.order.push_back(key.clone());
        self.cache.insert(key, CacheEntry { outcome, cost });
        self.evict();
    }

    fn evict(&mut self) {
        while self.cost > CACHE_BYTES {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };

            if let Some(entry) = self.cache.remove(&oldest) {
                self.cost = self.cost.saturating_sub(entry.cost);
            }
        }
    }
}

/// The largest cell rectangle that preserves the image's pixel aspect ratio
/// inside `max_columns` by `max_rows`.
pub fn fit_preview(
    width: u32,
    height: u32,
    font: (u16, u16),
    max_columns: u16,
    max_rows: u16,
) -> (u16, u16) {
    if width == 0 || height == 0 || max_columns == 0 || max_rows == 0 {
        return (1, 1);
    }

    let font_width = f64::from(font.0.max(1));
    let font_height = f64::from(font.1.max(1));
    let image_aspect = (f64::from(width) * font_height) / (f64::from(height) * font_width);
    let max_columns = f64::from(max_columns);
    let max_rows = f64::from(max_rows);
    let mut columns = max_columns;
    let mut rows = columns / image_aspect;

    if rows > max_rows {
        rows = max_rows;
        columns = rows * image_aspect;
    }

    (
        (columns.ceil() as u16).clamp(1, max_columns as u16),
        (rows.ceil() as u16).clamp(1, max_rows as u16),
    )
}

pub fn human_bytes(bytes: u64) -> String {
    match bytes {
        0..=1023 => format!("{bytes} B"),
        1024..=1_048_575 => format!("{:.1} KiB", bytes as f64 / 1024.0),
        _ => format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0)),
    }
}

pub fn media_label(media_type: &str) -> &'static str {
    match media_type.trim().to_ascii_lowercase().as_str() {
        "image/png" => "PNG",
        "image/jpeg" | "image/jpg" => "JPEG",
        "image/webp" => "WEBP",
        "image/gif" => "GIF",
        _ => "image",
    }
}

fn decoded_size(width: u32, height: u32) -> u64 {
    u64::from(width) * u64::from(height) * 4
}

/// Largest edge a source image may declare.
///
/// A preview is a few hundred cells wide, so more pixels buy no visible detail,
/// and the bound makes a tiny file that declares enormous dimensions fail while
/// its header is read instead of during decode.
/// Limits handed to the decoder, so an oversized image is rejected before its
/// pixels are materialized: decoding first and checking afterwards would only
/// move the allocation bound to the image crate's own default.
///
/// Only the decoded-size budget decides. A per-edge cap would reject a wide
/// panorama that fits the budget comfortably, and its preview would be scaled to
/// the same few cells as any other image.
fn preview_limits() -> image::Limits {
    let mut limits = image::Limits::default();

    limits.max_alloc = Some(MAX_DECODED_BYTES);

    limits
}

fn decode_preview(job: &Job) -> PreviewOutcome {
    let mut reader = match image::ImageReader::open(&job.key.path)
        .and_then(|reader| reader.with_guessed_format())
    {
        Ok(reader) => reader,
        Err(error) => {
            return PreviewOutcome::Failed(format!("cannot open the image: {error}"));
        }
    };

    reader.limits(preview_limits());

    let image = match reader.decode() {
        Ok(image) => image,
        Err(error) => {
            return PreviewOutcome::Failed(format!("cannot decode the image: {error}"));
        }
    };
    let (width, height) = (image.width(), image.height());
    let decoded_bytes = decoded_size(width, height);

    if decoded_bytes > MAX_DECODED_BYTES {
        return PreviewOutcome::Failed(format!(
            "preview unavailable: {} x {} decodes to more than {} MiB",
            width,
            height,
            MAX_DECODED_BYTES / (1024 * 1024)
        ));
    }

    let size = Size::new(job.key.columns.max(1), job.key.rows.max(1));

    match job.picker.new_protocol(image, size, Resize::Fit(None)) {
        Ok(protocol) => PreviewOutcome::Ready {
            protocol,
            decoded_bytes,
        },
        Err(error) => PreviewOutcome::Failed(format!("cannot encode the preview: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use ratatui_image::picker::Picker;

    use super::{
        CACHE_BYTES, ImageEnv, ImagePreviews, MAX_PREVIEW_COLUMNS, MAX_PREVIEW_ROWS, PreviewKey,
        PreviewOutcome, fit_preview, human_bytes, media_label,
    };
    use ratatui_image::picker::ProtocolType;

    fn image_env() -> ImageEnv {
        ImageEnv::default()
    }

    fn protocol_for(env: &ImageEnv) -> ProtocolType {
        super::protocol_for(env)
    }

    fn key(name: &str) -> PreviewKey {
        PreviewKey {
            path: PathBuf::from(name),
            columns: 10,
            rows: 5,
        }
    }

    #[test]
    fn previews_fit_the_cell_box_without_changing_aspect() {
        assert_eq!(
            fit_preview(100, 100, (10, 20), MAX_PREVIEW_COLUMNS, MAX_PREVIEW_ROWS),
            (48, 24)
        );
        assert_eq!(
            fit_preview(100, 10, (10, 20), MAX_PREVIEW_COLUMNS, MAX_PREVIEW_ROWS),
            (100, 5)
        );
        assert_eq!(
            fit_preview(10, 100, (10, 20), MAX_PREVIEW_COLUMNS, MAX_PREVIEW_ROWS),
            (5, 24)
        );
        assert_eq!(
            fit_preview(0, 0, (10, 20), MAX_PREVIEW_COLUMNS, MAX_PREVIEW_ROWS),
            (1, 1)
        );
    }

    #[test]
    fn kitty_detection_covers_tmux_passthrough_and_direct_terminals() {
        let mut env = image_env();

        env.tmux = true;
        env.term = "xterm-kitty".to_owned();
        env.term_program = "tmux".to_owned();

        assert_eq!(protocol_for(&env), ProtocolType::Kitty);

        let mut env = image_env();

        env.term = "xterm-kitty".to_owned();
        assert_eq!(protocol_for(&env), ProtocolType::Kitty);

        env.term = "screen-256color".to_owned();
        env.kitty_window = true;
        assert_eq!(protocol_for(&env), ProtocolType::Kitty);

        env.kitty_window = false;
        env.lc_terminal = "kitty".to_owned();
        assert_eq!(protocol_for(&env), ProtocolType::Kitty);
    }

    #[test]
    fn tmux_without_a_wrapping_outer_terminal_stays_on_halfblocks() {
        let mut env = image_env();

        env.tmux = true;
        env.term = "tmux-256color".to_owned();

        assert_eq!(protocol_for(&env), ProtocolType::Halfblocks);

        let mut env = image_env();

        env.tmux = true;
        env.term = "xterm-kitty".to_owned();
        env.kitty_window = true;

        assert_eq!(protocol_for(&env), ProtocolType::Halfblocks);
    }

    #[test]
    fn sixel_iterm2_and_the_override_select_their_protocols() {
        let mut env = image_env();

        env.term = "xterm-sixel".to_owned();
        assert_eq!(protocol_for(&env), ProtocolType::Sixel);

        env.term = "xterm-256color".to_owned();
        env.term_program = "iTerm.app".to_owned();
        assert_eq!(protocol_for(&env), ProtocolType::Iterm2);

        env.term_program.clear();
        env.lc_terminal = "iTerm2".to_owned();
        assert_eq!(protocol_for(&env), ProtocolType::Iterm2);

        let mut env = image_env();

        env.tmux = true;
        env.term = "xterm-kitty".to_owned();
        env.protocol = Some("halfblocks".to_owned());
        assert_eq!(protocol_for(&env), ProtocolType::Halfblocks);

        env.protocol = Some("sixel".to_owned());
        assert_eq!(protocol_for(&env), ProtocolType::Sixel);

        env.protocol = Some("kitty".to_owned());
        assert_eq!(protocol_for(&env), ProtocolType::Kitty);
    }

    #[test]
    fn sizes_and_formats_are_human_readable() {
        assert_eq!(human_bytes(12), "12 B");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(2 * 1024 * 1024), "2.0 MiB");
        assert_eq!(media_label("image/png"), "PNG");
        assert_eq!(media_label("image/gif"), "GIF");
        assert_eq!(media_label("application/octet-stream"), "image");
    }

    #[test]
    fn the_cache_evicts_the_oldest_entry_over_the_budget() {
        let mut previews = ImagePreviews::new();
        let first = key("first.png");
        let second = key("second.png");

        previews.store(
            first.clone(),
            PreviewOutcome::Failed("first".to_owned()),
            CACHE_BYTES,
        );
        previews.store(
            second.clone(),
            PreviewOutcome::Failed("second".to_owned()),
            CACHE_BYTES,
        );

        assert!(previews.outcome(&first).is_none(), "the oldest should go");
        assert!(previews.outcome(&second).is_some());
    }

    #[test]
    fn invalidating_a_session_clears_cached_previews() {
        let mut previews = ImagePreviews::new();
        let stale = key("stale.png");

        previews.store(stale.clone(), PreviewOutcome::Failed("entry".to_owned()), 0);
        previews.invalidate();

        assert!(previews.outcome(&stale).is_none());
    }

    #[test]
    fn a_result_from_an_older_epoch_is_ignored() {
        let mut previews = ImagePreviews::new();
        let stale = key("stale.png");
        let old_epoch = previews.epoch;

        previews.invalidate();
        previews.pending.insert(stale.clone());

        let (sender, receiver) = std::sync::mpsc::channel();

        previews.results = receiver;
        sender
            .send(super::PreviewResult {
                epoch: old_epoch,
                key: stale.clone(),
                outcome: PreviewOutcome::Failed("stale".to_owned()),
            })
            .expect("send");

        assert!(!previews.apply_results());
        assert!(previews.outcome(&stale).is_none());
        assert!(!previews.pending.contains(&stale));
    }

    #[test]
    fn an_identical_picker_keeps_the_cache() {
        let mut previews = ImagePreviews::new();
        let cached = key("cached.png");

        previews.store(
            cached.clone(),
            PreviewOutcome::Failed("entry".to_owned()),
            0,
        );
        previews.set_picker(Picker::halfblocks());

        assert!(previews.outcome(&cached).is_some());
    }

    fn save_fixture(path: &std::path::Path, format: image::ImageFormat) {
        let image = image::RgbImage::from_fn(4, 4, |x, y| {
            image::Rgb([(x * 40) as u8, (y * 40) as u8, 128])
        });
        image::DynamicImage::ImageRgb8(image)
            .save_with_format(path, format)
            .expect("write fixture");
    }

    fn wait_for<'a>(previews: &'a mut ImagePreviews, key: &PreviewKey) -> &'a PreviewOutcome {
        for _ in 0..200 {
            if previews.apply_results() {
                break;
            }

            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        previews.outcome(key).expect("a preview outcome")
    }

    #[test]
    fn png_jpeg_webp_and_gif_fixtures_decode() {
        let dir = tempfile::tempdir().expect("temp dir");

        for (name, format) in [
            ("fixture.png", image::ImageFormat::Png),
            ("fixture.jpg", image::ImageFormat::Jpeg),
            ("fixture.webp", image::ImageFormat::WebP),
            ("fixture.gif", image::ImageFormat::Gif),
        ] {
            let path = dir.path().join(name);

            save_fixture(&path, format);

            let mut previews = ImagePreviews::new();
            let key = previews.request(&path, 4, 2);

            assert!(
                matches!(wait_for(&mut previews, &key), PreviewOutcome::Ready { .. }),
                "{name} did not decode"
            );
        }
    }

    #[test]
    fn corrupt_and_missing_images_report_a_useful_failure() {
        let dir = tempfile::tempdir().expect("temp dir");
        let corrupt = dir.path().join("broken.png");
        let missing = dir.path().join("missing.png");

        std::fs::write(&corrupt, b"not a png").expect("write");

        for path in [&corrupt, &missing] {
            let mut previews = ImagePreviews::new();
            let key = previews.request(path, 4, 2);

            match wait_for(&mut previews, &key) {
                PreviewOutcome::Failed(message) => {
                    assert!(!message.is_empty(), "{message}");
                }
                PreviewOutcome::Ready { .. } => panic!("{path:?} unexpectedly decoded"),
            }
        }
    }

    #[test]
    fn the_decoded_limit_is_four_bytes_per_pixel() {
        assert_eq!(super::decoded_size(10, 10), 400);
        assert!(super::decoded_size(4097, 4096) > super::MAX_DECODED_BYTES);
    }

    fn chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();

        bytes.extend_from_slice(&(data.len() as u32).to_be_bytes());
        bytes.extend_from_slice(kind);
        bytes.extend_from_slice(data);

        let mut crc = 0xffff_ffff_u32;

        for byte in kind.iter().chain(data) {
            crc ^= u32::from(*byte);

            for _ in 0..8 {
                crc = if crc & 1 == 1 {
                    0xedb8_8320 ^ (crc >> 1)
                } else {
                    crc >> 1
                };
            }
        }

        bytes.extend_from_slice(&(crc ^ 0xffff_ffff).to_be_bytes());

        bytes
    }

    /// A PNG whose header claims `width` x `height` with almost no pixel data.
    fn png_declaring(width: u32, height: u32) -> Vec<u8> {
        let mut header = Vec::new();

        header.extend_from_slice(&width.to_be_bytes());
        header.extend_from_slice(&height.to_be_bytes());
        // 8-bit RGBA, deflate, adaptive filtering, no interlace.
        header.extend_from_slice(&[8, 6, 0, 0, 0]);

        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();

        png.extend_from_slice(&chunk(b"IHDR", &header));
        png.extend_from_slice(&chunk(b"IDAT", &[0x78, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00]));
        png.extend_from_slice(&chunk(b"IEND", &[]));

        png
    }

    #[test]
    fn an_image_whose_header_declares_huge_dimensions_is_rejected_unallocated() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("huge.png");

        std::fs::write(&path, png_declaring(20_000, 20_000)).expect("write fixture");

        let file_bytes = std::fs::metadata(&path).expect("metadata").len();

        assert!(
            file_bytes < 1024,
            "the fixture must stay tiny: {file_bytes}"
        );

        let mut previews = ImagePreviews::new();
        let key = previews.request(&path, 4, 2);

        match wait_for(&mut previews, &key) {
            // The decoded-size budget is the only limit, so the rejection comes
            // from the allocator guard rather than from a per-edge cap.
            PreviewOutcome::Failed(message) => assert!(
                message.contains("limit"),
                "the decoder must reject the declared size, not allocate it: {message}"
            ),
            PreviewOutcome::Ready { .. } => {
                panic!("a 20000 x 20000 header must not reach the decode buffer")
            }
        }
    }
}

//! Poster fetching, decoding and rendering, shared by every app.
//!
//! A peer of `crate::net` rather than something an app owns: all three backends
//! show artwork, and one cache serving all of them means switching tabs does not
//! refetch. Unlike the REST clients this module does import `ratatui`, because it
//! owns the handoff to the image widget.

mod cache;
mod fetch;
pub mod key;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::layout::{Rect, Size};
use ratatui_image::Image;
use ratatui_image::picker::{Capability, Picker, ProtocolType};
use ratatui_image::protocol::Protocol;
use serde::{Deserialize, Serialize};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};

use crate::app::AppId;

pub use key::{ImageKey, PosterSize, Source};

/// Concurrent HTTP fetches. Four rather than more: these are single self-hosted
/// servers, and a decorative image does not justify hammering one.
const HTTP_CONCURRENCY: usize = 4;

/// Concurrent decode-and-encode jobs. `spawn_blocking`'s pool is 512 threads, so
/// without a bound a screenful of Sixel encodes would saturate the machine.
const DECODE_CONCURRENCY: usize = 2;

/// In-flight requests per app. Over the cap a draw asks for nothing new and the
/// row gets its poster a frame or two later — backpressure driven by what is
/// actually on screen, which is what keeps a held PageDown cheap.
const MAX_PENDING: usize = 32;

/// Cached entries, by protocol. A Sixel or kitty payload runs to hundreds of
/// kilobytes where a halfblock grid is nearer ten, so one bound would be either
/// wasteful or wrong. Keeping this generous is not only a speed matter: under
/// kitty a cache hit reuses the same image id and its already-latched transmit
/// flag, where a miss retransmits the whole image and orphans the old one in the
/// terminal's store.
const MAX_ENTRIES_PROTOCOL: usize = 64;
const MAX_ENTRIES_HALFBLOCKS: usize = 256;

/// How long a missing poster is remembered, so one 404 is not re-requested on
/// every scroll past the row.
const ABSENT_TTL: Duration = Duration::from_secs(600);

/// Shorter for an *arr proxy path: the server only keeps its side of that
/// mapping for 24 hours, so a failure there is likely transient rather than a
/// genuine absence of artwork.
const ABSENT_TTL_PROXY: Duration = Duration::from_secs(60);

/// Detected protocol, for the Settings row to show alongside `Auto`. A global for
/// the same reason as `CURRENT_MODE`: `setting_row` is a free function.
static DETECTED: AtomicU8 = AtomicU8::new(0);

/// Active `ImageMode`. A process-wide atomic for the same reason `ui::theme`
/// keeps the palette in one: it is read from the render path and from free
/// functions in the Settings tab that hold no handle, and it is a lone `u8` with
/// no ordering relationship to other memory, so `Relaxed` is sufficient.
///
/// Only the *mode* lives here. The cache, its semaphores and its senders are
/// real state and are injected as an `Arc` instead, like `Arc<Mutex<Config>>`.
static CURRENT_MODE: AtomicU8 = AtomicU8::new(ImageMode::Auto as u8);

/// How posters are rendered, if at all.
///
/// Three states rather than a bool because "the terminal can do Sixel but I would
/// rather it didn't" is a real preference: Sixel encoding is the slowest path and
/// its payloads are orders of magnitude larger than a halfblock grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ImageMode {
    /// Detect the terminal's graphics protocol once at startup (kitty, iTerm2 or
    /// Sixel), falling back to unicode halfblocks where there is none.
    #[default]
    Auto,
    /// Force halfblocks even where a protocol was detected.
    Halfblocks,
    /// Draw no artwork, and reserve no space for it: every view lays out exactly
    /// as it did before posters existed.
    Off,
}

impl ImageMode {
    /// Every mode, in selector order.
    pub const ALL: [ImageMode; 3] = [ImageMode::Auto, ImageMode::Halfblocks, ImageMode::Off];

    /// Human-readable name shown in the Settings tab.
    pub fn title(self) -> &'static str {
        match self {
            ImageMode::Auto => "Auto",
            ImageMode::Halfblocks => "Halfblocks",
            ImageMode::Off => "Off",
        }
    }
}

/// The mode the user has selected.
pub fn mode() -> ImageMode {
    match CURRENT_MODE.load(Ordering::Relaxed) {
        1 => ImageMode::Halfblocks,
        2 => ImageMode::Off,
        _ => ImageMode::Auto,
    }
}

/// Switch the mode. Cheap and lock-free; the next frame renders it.
pub fn set_mode(mode: ImageMode) {
    CURRENT_MODE.store(mode as u8, Ordering::Relaxed);
}

/// Set the initial mode at startup. A named alias for `set_mode` that documents
/// the one-time call in `main`, mirroring `ui::theme::init`.
pub fn init(mode: ImageMode) {
    set_mode(mode);
}

/// Whether views should reserve space for artwork at all.
///
/// Note this is only about the user's choice, not the terminal's: halfblocks work
/// everywhere, so there is no terminal on which posters are impossible. A `false`
/// here means "reserve nothing", and every view then lays out exactly as it did
/// before posters existed — which is different from a poster that is merely still
/// loading, where the space is reserved and left blank.
pub fn enabled() -> bool {
    mode() != ImageMode::Off
}

/// Name of the graphics protocol detected at startup, for the Settings row.
/// "halfblocks" until detection has run.
pub fn detected_protocol() -> &'static str {
    match DETECTED.load(Ordering::Relaxed) {
        1 => "sixel",
        2 => "kitty",
        3 => "iterm2",
        _ => "halfblocks",
    }
}

fn store_detected(protocol: ProtocolType) {
    let code = match protocol {
        ProtocolType::Halfblocks => 0,
        ProtocolType::Sixel => 1,
        ProtocolType::Kitty => 2,
        ProtocolType::Iterm2 => 3,
    };
    DETECTED.store(code, Ordering::Relaxed);
}

/// The colour transparent artwork should composite against.
///
/// This matters because halfblocks is the only thing in the app that paints a cell
/// *background* — everything else sets a foreground and lets the terminal's own
/// background show through. Left unset, transparency composites against
/// ratatui-image's default of fully transparent black, which lands as a black
/// rectangle in the middle of a light theme.
///
/// The terminal's reported background is the right answer when there is one, since
/// it is the surface the surrounding text sits on. The near-white fallback follows
/// from both shipped themes being light; `Palette` has no background field to
/// derive anything better from.
fn detected_background(picker: &Picker) -> image::Rgba<u8> {
    const LIGHT_FALLBACK: image::Rgba<u8> = image::Rgba([0xf5, 0xf5, 0xf5, u8::MAX]);
    picker
        .capabilities()
        .iter()
        .find_map(|capability| match capability {
            Capability::Background(red, green, blue) => {
                Some(image::Rgba([*red, *green, *blue, u8::MAX]))
            }
            _ => None,
        })
        .unwrap_or(LIGHT_FALLBACK)
}

/// Where a poster is fetched from, and the header that authorises it.
///
/// Published by each app once it has connected and withdrawn on removal, so the
/// cache can never build a URL for a backend the user has not configured.
pub struct SourceAuth {
    /// The normalized host. Kept whole (base path included) because the Jellyfin
    /// arm appends to it; the *arr arms reduce it to an origin themselves.
    pub host: String,
    /// Header name and value. A credential only ever travels as a header, never
    /// in a URL, so it cannot reach a log line that prints one.
    header: (&'static str, String),
}

impl SourceAuth {
    /// Jellyfin authorises with its `MediaBrowser` header, built by the client so
    /// the format lives in exactly one place.
    pub fn jellyfin(host: String, auth_header: String) -> Self {
        Self {
            host,
            header: ("Authorization", auth_header),
        }
    }

    /// Radarr and Sonarr authorise with `X-Api-Key`.
    pub fn arr(host: String, api_key: String) -> Self {
        Self {
            host,
            header: ("X-Api-Key", api_key),
        }
    }
}

/// Hand-written so a credential cannot reach a log through a derived `Debug`,
/// mirroring `jellyfin::Client` and `arr::Transport`.
impl std::fmt::Debug for SourceAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SourceAuth")
            .field("host", &self.host)
            .field("header", &format_args!("{}: <redacted>", self.header.0))
            .finish()
    }
}

/// Called when a poster for an app has landed, so the app can wake the event
/// loop with its own message type.
///
/// The shell only redraws in response to an event, so without this a poster would
/// not appear until the next 250ms tick. A boxed closure rather than an
/// `AppSender` because each app's `Msg` enum is private to it.
type Waker = Arc<dyn Fn() + Send + Sync>;

/// One cache slot: the poster is ready, on its way, or known not to exist.
enum Entry {
    /// A job is queued or running. Never evicted — dropping one would orphan the
    /// job and the next draw would queue it again, forever.
    Pending,
    Ready {
        protocol: Arc<Protocol>,
        /// Logical clock stamp, for evicting the least recently drawn.
        used: u64,
    },
    /// No artwork, or a failure. Expires so a transient error is retried
    /// eventually, but not on the very next frame.
    Absent { until: Instant },
}

/// A queued poster job. Everything the worker needs is captured here so it never
/// has to take the cache lock just to find out what to fetch.
struct Job {
    app: AppId,
    key: ImageKey,
    size: PosterSize,
    /// The app's cancel generation when this was queued; a job whose generation
    /// has moved on is dropped instead of completing into a view that is gone.
    cancel_gen: u64,
    url: String,
    header: (&'static str, String),
    /// Cloned rather than shared: `Picker` is small and `Clone`, and copying it
    /// keeps the encode off the cache lock entirely.
    picker: Picker,
    /// Where the downloaded bytes live on disk, or `None` when running
    /// memory-only (no cache directory, or under test).
    cache_path: Option<PathBuf>,
}

struct Inner {
    /// What detection found, kept so a flip back to `Auto` restores it.
    detected: Picker,
    /// The picker in use, which is `detected` forced to halfblocks under
    /// `ImageMode::Halfblocks`.
    picker: Picker,
    /// The mode this state was built for, so a flip is noticed on the next draw.
    mode: ImageMode,
    entries: HashMap<(ImageKey, PosterSize), Entry>,
    /// Monotonic draw counter backing the eviction order.
    clock: u64,
    sources: HashMap<Source, SourceAuth>,
    cancel: HashMap<AppId, Arc<AtomicU64>>,
    pending: HashMap<AppId, usize>,
    wakers: HashMap<AppId, Waker>,
    /// Set when a poster has landed and the app has not redrawn since. Coalesces
    /// the wake-ups: a screenful arriving together sends one message, not twenty.
    dirty: HashMap<AppId, bool>,
    /// Root of the on-disk byte cache, `None` when memory-only.
    cache_dir: Option<PathBuf>,
    /// Jobs queued over this instance's life. Only read by tests, which use it to
    /// prove a repeated draw does not re-queue work.
    #[cfg(test)]
    enqueued: u64,
}

/// The shared poster cache.
///
/// Held as `Arc<Images>`, never `Arc<Mutex<Images>>`: every method takes `&self`
/// and locks internally. That is load-bearing rather than stylistic — several of
/// the list renderers are `&self`, and internal locking means adding posters
/// changes none of their signatures.
pub struct Images {
    inner: Mutex<Inner>,
    /// Unbounded because `draw` can neither block nor await. The real bound is
    /// `MAX_PENDING`, applied before anything is queued.
    jobs: mpsc::UnboundedSender<Job>,
}

impl Images {
    /// Detect the terminal's capabilities and start the fetch worker.
    ///
    /// Must be called after entering the alternate screen but *before* the input
    /// thread starts: detection writes a query to stdout and reads the reply from
    /// stdin, and the input thread owns stdin once it is running.
    pub fn start(mode: ImageMode) -> Arc<Self> {
        // Bounded internally at two seconds, degrading to halfblocks. The error
        // arm matters: a console whose mode cannot be read must not stop isamedia
        // from starting.
        let mut picker = Picker::from_query_stdio().unwrap_or_else(|err| {
            tracing::debug!(%err, "no terminal graphics detected, using halfblocks");
            Picker::halfblocks()
        });
        picker.set_background_color(Some(detected_background(&picker)));
        store_detected(picker.protocol_type());
        let cache_dir = cache::dir();
        let (images, jobs) = Self::with_picker(picker, mode, cache_dir.clone());
        tokio::spawn(run_worker(images.clone(), jobs));
        // Trim the cache once, in the background. Never per write: that would turn
        // scrolling a library into a stream of directory walks.
        if let Some(dir) = cache_dir {
            tokio::task::spawn_blocking(move || cache::prune(&dir));
        }
        images
    }

    fn with_picker(
        picker: Picker,
        mode: ImageMode,
        cache_dir: Option<PathBuf>,
    ) -> (Arc<Self>, mpsc::UnboundedReceiver<Job>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut inner = Inner {
            detected: picker.clone(),
            picker,
            mode,
            entries: HashMap::new(),
            clock: 0,
            sources: HashMap::new(),
            cancel: HashMap::new(),
            pending: HashMap::new(),
            wakers: HashMap::new(),
            dirty: HashMap::new(),
            cache_dir,
            #[cfg(test)]
            enqueued: 0,
        };
        inner.apply_mode(mode);
        (
            Arc::new(Self {
                inner: Mutex::new(inner),
                jobs: tx,
            }),
            rx,
        )
    }

    /// A cache with no worker and no terminal, for tests. Jobs are queued and
    /// never run, so a poster stays pending, which is what the layout code sees
    /// on a cold cache anyway.
    #[cfg(test)]
    pub fn for_tests() -> Arc<Self> {
        let (images, rx) = Self::with_picker(Picker::halfblocks(), ImageMode::Auto, None);
        drop(rx);
        images
    }

    /// The terminal's cell size in pixels, for `crate::ui::poster::split`.
    pub fn cell_px(&self) -> (u16, u16) {
        let font = self.inner.lock().unwrap().picker.font_size();
        (font.width, font.height)
    }

    /// Publish a backend's host and credential, or withdraw them on removal.
    pub fn set_source(&self, source: Source, auth: Option<SourceAuth>) {
        let mut inner = self.inner.lock().unwrap();
        match auth {
            Some(auth) => {
                inner.sources.insert(source, auth);
            }
            None => {
                inner.sources.remove(&source);
            }
        }
    }

    /// Register how to wake an app when one of its posters lands. Called once per
    /// app at construction, so it survives a reconnect replacing the browse.
    pub fn set_waker(&self, app: AppId, waker: Waker) {
        self.inner.lock().unwrap().wakers.insert(app, waker);
    }

    /// The cancel generation for an app, allocated on first use. Bump it when the
    /// list under the posters is being replaced — never on plain cursor movement,
    /// which would cancel the very posters the scroll is trying to load.
    pub fn cancel_gen(&self, app: AppId) -> Arc<AtomicU64> {
        self.inner
            .lock()
            .unwrap()
            .cancel
            .entry(app)
            .or_default()
            .clone()
    }

    /// Draw the poster for `key` into `slot`, scheduling the fetch if it is not
    /// ready. Draws nothing while it loads or when there is no artwork.
    ///
    /// Never blocks and never awaits, so it is safe on the render thread. The
    /// caller reserves `slot` regardless of what this does, which is what keeps
    /// text from reflowing when a poster lands.
    pub fn draw(&self, frame: &mut Frame, slot: Rect, app: AppId, key: &ImageKey) {
        if !enabled() || slot.width == 0 || slot.height == 0 {
            return;
        }
        let Some(protocol) = self.poster(app, key, PosterSize::for_slot(slot)) else {
            return;
        };
        // A protocol wider than its area draws nothing at all for Sixel, and for
        // iTerm2 could paint outside it, so never hand one an area it does not
        // fit. `needs_placeholder` is the crate's own version of this check.
        if protocol.needs_placeholder(slot).is_some() {
            return;
        }
        let encoded = protocol.size();
        let area = Rect::new(slot.x, slot.y, encoded.width, encoded.height).intersection(slot);
        frame.render_widget(Image::new(protocol.as_ref()), area);
    }

    /// Look the poster up, queueing the work on a miss. `None` means "draw
    /// nothing this frame".
    fn poster(&self, app: AppId, key: &ImageKey, size: PosterSize) -> Option<Arc<Protocol>> {
        let mut inner = self.inner.lock().unwrap();
        inner.sync_mode();
        // This frame is the redraw a wake-up would have asked for, so re-arm the
        // coalescing flag: the next poster to land sends a fresh message.
        inner.dirty.insert(app, false);
        inner.clock += 1;
        let now = inner.clock;
        let cache_key = (key.clone(), size);

        match inner.entries.get_mut(&cache_key) {
            Some(Entry::Ready { protocol, used }) => {
                *used = now;
                return Some(protocol.clone());
            }
            // Already queued or running: this is what makes fast scrolling cheap.
            // A row drawn forty times while its poster loads queues exactly once.
            Some(Entry::Pending) => return None,
            Some(Entry::Absent { until }) if *until > Instant::now() => return None,
            // An expired negative entry: fall through and retry.
            Some(_) => {
                inner.entries.remove(&cache_key);
            }
            None => {}
        }

        let job = inner.build_job(app, key, size)?;
        inner.entries.insert(cache_key, Entry::Pending);
        *inner.pending.entry(app).or_insert(0) += 1;
        #[cfg(test)]
        {
            inner.enqueued += 1;
        }
        drop(inner);
        // A send failure means the worker is gone (only in tests, where the
        // receiver is dropped); the entry stays pending, which draws nothing.
        let _ = self.jobs.send(job);
        None
    }

    fn is_stale(&self, job: &Job) -> bool {
        let inner = self.inner.lock().unwrap();
        inner
            .cancel
            .get(&job.app)
            .is_some_and(|counter| counter.load(Ordering::Relaxed) != job.cancel_gen)
    }

    /// Resolve a pending entry back to vacant, so the next draw can retry it.
    /// Every worker exit path must resolve its entry one way or another: an entry
    /// left pending forever is the one way this cache can leak.
    fn abandon(&self, job: &Job) {
        let mut inner = self.inner.lock().unwrap();
        inner.entries.remove(&(job.key.clone(), job.size));
        inner.release_pending(job.app);
    }

    fn mark_absent(&self, job: &Job) {
        let mut inner = self.inner.lock().unwrap();
        let until = Instant::now() + absent_ttl(&job.key);
        inner
            .entries
            .insert((job.key.clone(), job.size), Entry::Absent { until });
        inner.release_pending(job.app);
    }

    fn mark_ready(&self, job: &Job, protocol: Protocol) {
        // The cache is written before the wake-up is sent, so the redraw it
        // triggers already sees the poster.
        let waker = {
            let mut inner = self.inner.lock().unwrap();
            let used = inner.clock;
            inner.entries.insert(
                (job.key.clone(), job.size),
                Entry::Ready {
                    protocol: Arc::new(protocol),
                    used,
                },
            );
            inner.release_pending(job.app);
            inner.evict_if_needed();
            inner.take_waker(job.app)
        };
        // Called outside the lock: the closure sends on a channel, and the render
        // thread may be waiting on this same mutex.
        if let Some(waker) = waker {
            waker();
        }
    }
}

impl Inner {
    /// Notice a mode flip and rebuild for it. Called from every draw, so the
    /// Settings tab only has to set the global; the running apps need no code for
    /// the change at all.
    fn sync_mode(&mut self) {
        let mode = mode();
        if mode == self.mode {
            return;
        }
        self.mode = mode;
        // Cached payloads are specific to the protocol that encoded them, so a
        // flip invalidates all of them. In-flight jobs are cancelled rather than
        // completing into the old protocol.
        self.entries.clear();
        self.pending.clear();
        for counter in self.cancel.values() {
            counter.fetch_add(1, Ordering::Relaxed);
        }
        self.apply_mode(mode);
    }

    fn apply_mode(&mut self, mode: ImageMode) {
        self.picker = self.detected.clone();
        if mode == ImageMode::Halfblocks {
            // `set_protocol_type` rather than `Picker::halfblocks()`: the latter
            // hard-codes a 10x20 font size, where keeping the detected one is what
            // makes the halfblock aspect ratio right.
            self.picker.set_protocol_type(ProtocolType::Halfblocks);
        }
    }

    /// Assemble a job, or `None` when there is nothing to fetch: no configured
    /// host, no same-origin URL, or too much already in flight for this app.
    fn build_job(&mut self, app: AppId, key: &ImageKey, size: PosterSize) -> Option<Job> {
        if self.pending.get(&app).copied().unwrap_or(0) >= MAX_PENDING {
            return None;
        }
        let auth = self.sources.get(&key.source())?;
        let url = key.url(&auth.host)?;
        let cancel_gen = self
            .cancel
            .get(&app)
            .map_or(0, |counter| counter.load(Ordering::Relaxed));
        let cache_path = self
            .cache_dir
            .as_ref()
            .map(|dir| cache::entry_path(dir, key, &auth.host));
        Some(Job {
            app,
            key: key.clone(),
            size,
            cancel_gen,
            url,
            header: (auth.header.0, auth.header.1.clone()),
            picker: self.picker.clone(),
            cache_path,
        })
    }

    /// The app's waker, but only on the first landing since its last redraw, so a
    /// screenful of posters arriving together produces one wake-up.
    fn take_waker(&mut self, app: AppId) -> Option<Waker> {
        let dirty = self.dirty.entry(app).or_insert(false);
        if *dirty {
            return None;
        }
        *dirty = true;
        self.wakers.get(&app).cloned()
    }

    fn release_pending(&mut self, app: AppId) {
        if let Some(count) = self.pending.get_mut(&app) {
            *count = count.saturating_sub(1);
        }
    }

    /// Drop the least recently drawn ready entries once over budget. Pending
    /// entries are never candidates.
    fn evict_if_needed(&mut self) {
        let limit = if self.picker.protocol_type() == ProtocolType::Halfblocks {
            MAX_ENTRIES_HALFBLOCKS
        } else {
            MAX_ENTRIES_PROTOCOL
        };
        while self.entries.len() > limit {
            let oldest = self
                .entries
                .iter()
                .filter_map(|(cache_key, entry)| match entry {
                    Entry::Ready { used, .. } => Some((*used, cache_key.clone())),
                    _ => None,
                })
                .min_by_key(|(used, _)| *used)
                .map(|(_, cache_key)| cache_key);
            match oldest {
                Some(cache_key) => {
                    self.entries.remove(&cache_key);
                }
                // Nothing evictable left: everything is pending or negative.
                None => break,
            }
        }
    }
}

/// How long to remember that a poster is missing.
fn absent_ttl(key: &ImageKey) -> Duration {
    let proxied = match key {
        ImageKey::Radarr { path } | ImageKey::Sonarr { path } => path.contains("/MediaCoverProxy/"),
        ImageKey::Jellyfin { .. } => false,
    };
    if proxied {
        ABSENT_TTL_PROXY
    } else {
        ABSENT_TTL
    }
}

/// Pull jobs, bounding how many run at once. Acquiring the permit here rather
/// than inside the task is what applies the bound: the loop parks until a slot
/// frees instead of spawning a task per queued job.
async fn run_worker(images: Arc<Images>, mut jobs: mpsc::UnboundedReceiver<Job>) {
    let client = match fetch::client() {
        Ok(client) => client,
        Err(err) => {
            tracing::error!(%err, "could not build the poster http client; artwork disabled");
            return;
        }
    };
    let http = Arc::new(Semaphore::new(HTTP_CONCURRENCY));
    let decode = Arc::new(Semaphore::new(DECODE_CONCURRENCY));
    while let Some(job) = jobs.recv().await {
        if images.is_stale(&job) {
            images.abandon(&job);
            continue;
        }
        let Ok(permit) = http.clone().acquire_owned().await else {
            break;
        };
        tokio::spawn(run_job(
            images.clone(),
            client.clone(),
            decode.clone(),
            job,
            permit,
        ));
    }
}

async fn run_job(
    images: Arc<Images>,
    client: reqwest::Client,
    decode: Arc<Semaphore>,
    job: Job,
    _http_permit: OwnedSemaphorePermit,
) {
    // Disk first: a hit skips the network entirely, which is what makes a restart
    // feel instant instead of re-downloading everything on screen.
    let cached = match job.cache_path.clone() {
        Some(path) => tokio::task::spawn_blocking(move || cache::read(&path))
            .await
            .ok()
            .flatten(),
        None => None,
    };
    let from_disk = cached.is_some();
    let bytes = match cached {
        Some(bytes) => bytes,
        None => {
            let Some(bytes) = fetch::fetch(&client, &job.url, (job.header.0, &job.header.1)).await
            else {
                images.mark_absent(&job);
                return;
            };
            if let Some(path) = job.cache_path.clone() {
                let to_write = bytes.clone();
                tokio::task::spawn_blocking(move || cache::write(&path, &to_write));
            }
            bytes
        }
    };
    // Re-check after the round trip: the user may have moved on while it ran, and
    // encoding is the expensive half.
    if images.is_stale(&job) {
        images.abandon(&job);
        return;
    }
    let Ok(_decode_permit) = decode.acquire_owned().await else {
        images.abandon(&job);
        return;
    };
    let size = Size::new(job.size.cols, job.size.rows);
    let picker = job.picker.clone();
    // Decode, resize and quantise are all CPU-bound, so they go to the blocking
    // pool rather than tying up a runtime worker.
    match tokio::task::spawn_blocking(move || fetch::encode(&picker, &bytes, size)).await {
        Ok(Some(protocol)) => images.mark_ready(&job, protocol),
        Ok(None) if from_disk => {
            // Cached bytes that no longer decode: the file is damaged, so drop it
            // and leave the entry vacant rather than negative, so the next draw
            // refetches instead of waiting out the absent TTL.
            if let Some(path) = job.cache_path.clone() {
                tokio::task::spawn_blocking(move || cache::forget(&path));
            }
            images.abandon(&job);
        }
        Ok(None) => images.mark_absent(&job),
        Err(err) => {
            tracing::debug!(%err, "poster encode task failed");
            images.abandon(&job);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_mode_serialises_as_kebab_case() {
        // The wire format is a user-editable config value, so the spellings are
        // a contract: `images = "halfblocks"`, not `"Halfblocks"`.
        for (mode, expected) in [
            (ImageMode::Auto, "\"auto\""),
            (ImageMode::Halfblocks, "\"halfblocks\""),
            (ImageMode::Off, "\"off\""),
        ] {
            assert_eq!(serde_json::to_string(&mode).unwrap(), expected);
            assert_eq!(
                serde_json::from_str::<ImageMode>(expected).unwrap(),
                mode,
                "round trip failed for {expected}"
            );
        }
    }

    #[test]
    fn image_mode_rejects_an_unknown_value() {
        // No #[serde(other)]: a typo in the config should be reported rather
        // than silently becoming a mode the user did not ask for. Same choice as
        // `Theme`.
        assert!(serde_json::from_str::<ImageMode>("\"halfblock\"").is_err());
    }

    /// A cache with a Radarr host registered, so `build_job` can produce work.
    fn ready_cache() -> Arc<Images> {
        let images = Images::for_tests();
        images.set_source(
            Source::Radarr,
            Some(SourceAuth::arr(
                "https://example.com".into(),
                "test-key".into(),
            )),
        );
        images
    }

    fn cover(n: u32) -> ImageKey {
        ImageKey::Radarr {
            path: format!("/MediaCover/{n}/poster.jpg"),
        }
    }

    const SIZE: PosterSize = PosterSize { cols: 4, rows: 3 };

    // Note: none of these touch the `CURRENT_MODE` global. Tests run in parallel
    // threads, so mutating it here would make unrelated tests flaky.

    #[test]
    fn a_repeatedly_drawn_row_queues_its_poster_once() {
        // The property that makes fast scrolling affordable: a row redrawn while
        // its poster loads must not queue the work again each frame.
        let images = ready_cache();
        for _ in 0..40 {
            assert!(images.poster("radarr", &cover(1), SIZE).is_none());
        }
        assert_eq!(images.inner.lock().unwrap().enqueued, 1);
    }

    #[test]
    fn nothing_is_queued_without_a_configured_host() {
        // No credential published yet (or the backend was removed): there must be
        // no request, and no pending entry left behind either.
        let images = Images::for_tests();
        assert!(images.poster("radarr", &cover(1), SIZE).is_none());
        let inner = images.inner.lock().unwrap();
        assert_eq!(inner.enqueued, 0);
        assert!(inner.entries.is_empty());
    }

    #[test]
    fn a_cdn_cover_path_is_never_queued() {
        // "Local only" at the cache boundary: an absolute third-party URL yields
        // no fetchable URL, so no job is built.
        let images = ready_cache();
        let key = ImageKey::Radarr {
            path: "https://image.tmdb.org/t/p/original/abc.jpg".into(),
        };
        assert!(images.poster("radarr", &key, SIZE).is_none());
        assert_eq!(images.inner.lock().unwrap().enqueued, 0);
    }

    #[test]
    fn in_flight_work_is_capped_per_app() {
        // Holding PageDown must not queue an unbounded pile of requests. Over the
        // cap a draw asks for nothing new; the row gets its poster once the queue
        // drains, which is backpressure driven by what is on screen.
        let images = ready_cache();
        for n in 0..(MAX_PENDING as u32 * 3) {
            assert!(images.poster("radarr", &cover(n), SIZE).is_none());
        }
        let inner = images.inner.lock().unwrap();
        assert_eq!(inner.enqueued, MAX_PENDING as u64);
        assert_eq!(inner.pending.get("radarr").copied(), Some(MAX_PENDING));
    }

    #[test]
    fn a_different_size_is_a_separate_entry() {
        // Why the size is in the key: a resize must re-encode rather than reuse a
        // payload built for the old cell count. That is also what makes resize
        // handling free — it is just a cache miss.
        let images = ready_cache();
        images.poster("radarr", &cover(1), SIZE);
        images.poster(
            "radarr",
            &cover(1),
            PosterSize {
                cols: 8,
                rows: SIZE.rows,
            },
        );
        assert_eq!(images.inner.lock().unwrap().enqueued, 2);
    }

    #[test]
    fn eviction_drops_the_oldest_ready_entry_and_never_a_pending_one() {
        // Evicting a pending entry would orphan its job, and the next draw would
        // queue it again — forever. Only ready entries are candidates, even when
        // that means briefly sitting above the budget.
        let images = ready_cache();
        let protocol = tiny_protocol();
        let mut inner = images.inner.lock().unwrap();
        inner.entries.insert((cover(9_999), SIZE), Entry::Pending);
        for n in 0..=(MAX_ENTRIES_HALFBLOCKS as u32) {
            inner.entries.insert(
                (cover(n), SIZE),
                Entry::Ready {
                    protocol: protocol.clone(),
                    used: u64::from(n),
                },
            );
        }

        inner.evict_if_needed();

        assert!(
            !inner.entries.contains_key(&(cover(0), SIZE)),
            "the least recently drawn poster should go first"
        );
        assert!(
            inner
                .entries
                .contains_key(&(cover(MAX_ENTRIES_HALFBLOCKS as u32), SIZE)),
            "the most recently drawn poster should survive"
        );
        assert!(
            matches!(
                inner.entries.get(&(cover(9_999), SIZE)),
                Some(Entry::Pending)
            ),
            "a pending entry must never be sacrificed to the budget"
        );
    }

    /// A real encoded protocol. Halfblocks is pure cell writes, so this needs no
    /// terminal; the `Arc` is then cloned to fill the cache cheaply.
    fn tiny_protocol() -> Arc<Protocol> {
        let mut png = Vec::new();
        image::DynamicImage::new_rgb8(4, 6)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .expect("encoding a test png");
        Arc::new(
            fetch::encode(&Picker::halfblocks(), &png, Size::new(2, 2))
                .expect("halfblocks always encodes"),
        )
    }

    fn far_future() -> Instant {
        Instant::now() + Duration::from_secs(3600)
    }

    #[test]
    fn an_expired_absent_entry_is_retried() {
        let images = ready_cache();
        images.inner.lock().unwrap().entries.insert(
            (cover(1), SIZE),
            Entry::Absent {
                until: Instant::now(),
            },
        );
        // Already expired, so the next draw queues a fresh attempt.
        assert!(images.poster("radarr", &cover(1), SIZE).is_none());
        assert_eq!(images.inner.lock().unwrap().enqueued, 1);
    }

    #[test]
    fn a_live_absent_entry_is_not_retried() {
        let images = ready_cache();
        images.inner.lock().unwrap().entries.insert(
            (cover(1), SIZE),
            Entry::Absent {
                until: far_future(),
            },
        );
        assert!(images.poster("radarr", &cover(1), SIZE).is_none());
        assert_eq!(
            images.inner.lock().unwrap().enqueued,
            0,
            "a known-missing poster must not be re-requested on every scroll past it"
        );
    }

    #[test]
    fn a_proxied_cover_expires_sooner_than_a_missing_one() {
        // An *arr proxy mapping only lives 24h server-side, so a failure there is
        // probably transient and worth retrying long before a real 404 is.
        assert_eq!(absent_ttl(&cover(1)), ABSENT_TTL);
        assert_eq!(
            absent_ttl(&ImageKey::Radarr {
                path: "/MediaCoverProxy/deadbeef/poster.jpg".into()
            }),
            ABSENT_TTL_PROXY
        );
    }

    #[test]
    fn a_terminal_that_reports_no_background_gets_a_light_one() {
        // Not black: transparent artwork would otherwise composite to a black
        // rectangle, and both shipped themes are light.
        let background = detected_background(&Picker::halfblocks());
        assert_eq!(background, image::Rgba([0xf5, 0xf5, 0xf5, u8::MAX]));
    }

    #[test]
    fn source_auth_debug_never_prints_the_credential() {
        let auth = SourceAuth::arr("https://example.com".into(), "sekret-key".into());
        let shown = format!("{auth:?}");
        assert!(!shown.contains("sekret-key"), "{shown}");
        assert!(shown.contains("<redacted>"), "{shown}");
        let jellyfin = SourceAuth::jellyfin(
            "https://example.com".into(),
            "MediaBrowser Token=\"tok3n\"".into(),
        );
        assert!(!format!("{jellyfin:?}").contains("tok3n"));
    }

    #[test]
    fn all_modes_are_listed_once_with_titles() {
        assert_eq!(ImageMode::ALL.len(), 3);
        assert_eq!(ImageMode::default(), ImageMode::Auto);
        let titles: Vec<_> = ImageMode::ALL.iter().map(|mode| mode.title()).collect();
        assert_eq!(titles, ["Auto", "Halfblocks", "Off"]);
    }
}

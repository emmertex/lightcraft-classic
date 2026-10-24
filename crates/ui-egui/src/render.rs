//! Background rendering: engine [`RenderJob`]s run on a [`JobPool`]; results become textures.
//!
//! Each visible thing that needs pixels is a *slot* (a grid thumbnail, the loupe, the "before"
//! image…). Requests are deduplicated per slot (a newer request replaces a queued older one) and
//! prioritised (loupe first, then on-screen thumbnails, then prefetch); thumbnails scrolled far
//! out of view are dropped from the queue. Thumbnail jobs hit the engine's preview cache (memory +
//! disk) before rendering. On wasm the jobs run inline, one per frame, unless the host installs a
//! [`RenderOffload`] (the browser build's Web Workers): then queued jobs are handed to it as it has
//! room, and its results are collected every frame.
//!
//! Stand-ins ([`QuickJob`]s, once per photo and settings): [`Slot::Preview`] holds what the loupe
//! shows until [`Slot::Main`] has the photo's render (cached view render, embedded camera JPEG or
//! a thumbnail render); [`Slot::ThumbQuick`] holds a raw's embedded preview in the grid until its
//! rendered thumbnail arrives (a cached thumbnail found by the quick job becomes the
//! [`Slot::Thumb`] texture directly).

use std::collections::HashMap;

use lcc_catalog::PhotoId;
use lcc_engine::Session;
use lcc_engine::media::{QuickJob, QuickSource, RenderJob, RenderResult};
use lcc_engine::pipeline::StageCache;
use lcc_preview::JobPool;
use lcc_raster::Histogram;
use std::sync::Arc;

/// The histogram a texture keeps: the loupe's (`Main`, and the `Preview` standing in for it) with its waveform
/// planes, which only the Histogram panel draws, every other slot without them (they are most of a histogram's
/// size, and thumbnails are many).
fn keep_waveform(slot: Slot, h: lcc_raster::Histogram) -> lcc_raster::Histogram {
    if matches!(slot, Slot::Main | Slot::Preview) { h } else { h.without_waveform() }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Slot {
    Thumb(PhotoId),
    /// A thumbnail's stand-in (embedded preview of an unedited raw).
    ThumbQuick(PhotoId),
    Main,
    /// A zoomed view while a slider drags: the window of the frame that is on screen, rendered in
    /// one draft job with its own stage cache (each tile apart would redo every stage per tile) and
    /// drawn over `Main` until the tiles of the final look arrive. By pane ([`PANE_AFTER`]…).
    Window(u8),
    /// The loupe's stand-in until `Main` has the photo.
    Preview,
    Before,
    Compare(u8),
    /// Background preparation of a neighbouring photo (no texture: its decoded source and view
    /// render are cached by the engine). 0 = next, 1 = previous.
    Prefetch(u8),
    /// A variant thumbnail (profile / preset browsers), by its job key ([`Session::variant_job`]).
    /// Kept in a bounded LRU ([`VARIANT_TEXTURES`]).
    Variant(u64),
    /// The loupe while hovering a preset or profile: the photo with the hovered look (nothing is
    /// committed).
    Hover,
    /// An import candidate's thumbnail in the import review dialog (by index).
    Import(u32),
    /// The second window's view of the active photo.
    Second,
    /// A tile of a zoomed view (see [`crate::region::TileCache`]): the pane it is shown in
    /// ([`PANE_AFTER`]…) and its place in the grid. Its result goes to [`Renderer::tiles`], not
    /// [`Renderer::textures`].
    Tile {
        pane: u8,
        col: u16,
        row: u16,
    },
    /// A slideshow slide (the one up, the one fading out, the next ones prepared in advance; see
    /// [`crate::panels::slideshow`]): a photo's, or a title screen.
    Slide(lcc_engine::slideshow::Item),
    /// The small composed slide in Settings ▸ Slideshow (the active photo with the layout being
    /// edited).
    SlidePreview,
    /// A slider drag's live HDR histogram: a small full-quality render of an HDR edit, beside the
    /// drafts (which have no HDR histogram), with no texture ([`Renderer::live_hdr_histogram`]).
    HdrHistogram,
    /// The Histogram panel's own small render of its photo, when no view of it carries what the
    /// panel's mode draws (a grid thumbnail has no waveform, a cached one no histogram), with no
    /// texture ([`Renderer::panel_histogram`]).
    PanelHistogram,
    /// An HDR render (linear float, highlights above SDR white) of an HDR edit, for an HDR display
    /// ([`crate::HdrOutput`]), for one view ([`HDR_LOUPE`]…): its result waits in
    /// [`Renderer::hdr_view_result`] for the upload.
    HdrView(u8),
}

/// The views an HDR edit is shown in HDR in ([`Slot::HdrView`], [`crate::HdrOutput::upload`]):
/// the loupe's whole frame, its Before side, the second window, the zoomed window of a pane
/// (`HDR_ZOOMED + pane`, see [`PANE_AFTER`]…) and the Compare / Reference photos
/// (`HDR_COMPARE + i`).
pub const HDR_LOUPE: u8 = 0;
pub const HDR_BEFORE: u8 = 1;
pub const HDR_SECOND: u8 = 2;
pub const HDR_COMPARE: u8 = 8;
pub const HDR_ZOOMED: u8 = 32;
/// A view's HDR texture ([`Slot::HdrView`]), drawn in place of (or over) its 8-bit render.
#[derive(Clone, Copy, Debug)]
pub struct HdrView {
    pub photo: PhotoId,
    /// The [`Slot::HdrView`] job key it was rendered for.
    pub key: u64,
    pub tex: egui::TextureId,
    pub size: [usize; 2],
}

/// What a view's HDR frame covers: a frame held while its update renders must show the same
/// part of the photo in the same shape (a crop, rotation or flip drops it).
#[derive(Clone, Copy, PartialEq)]
struct HdrGeometry {
    photo: PhotoId,
    full: Option<(usize, usize)>,
    window: Option<lcc_pipeline::PixelWindow>,
    /// The crop applied (`None`: the crop isn't, as in the crop tool).
    crop: Option<lcc_develop::Crop>,
    orientation: lcc_geom::Orientation,
}

/// The panes a zoomed view's tiles and drag windows belong to: the loupe (its After and Before
/// sides) and the Compare / Reference views' two photos.
pub const PANE_AFTER: u8 = 0;
pub const PANE_BEFORE: u8 = 1;
/// The first Compare / Reference pane (`PANE_COMPARE + i` for [`Slot::Compare`]`(i)`).
pub const PANE_COMPARE: u8 = 2;

/// Where a [`Slot::Window`] render sits: the photo, the zoomed frame, the window of it rendered,
/// and the develop settings' hash it was asked for with.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WindowGeom {
    pub photo: PhotoId,
    pub full: (usize, usize),
    pub window: lcc_engine::pipeline::PixelWindow,
    pub look: u64,
}

pub struct Tex {
    pub key: u64,
    pub photo: PhotoId,
    pub tex: egui::TextureHandle,
    pub size: [usize; 2],
    pub histogram: Option<Histogram>,
    pub ms: f64,
    /// Set for stand-ins: where the image came from.
    pub quick: Option<QuickSource>,
    /// CPU copy of the pixels (only with [`Renderer::keep_pixels`]; for headless screenshots).
    pub pixels: Option<std::sync::Arc<egui::ColorImage>>,
    /// What went wrong without failing the render (e.g. a plug-in filter skipped).
    pub warnings: Vec<String>,
}

/// Runs render jobs outside this thread's job pool (the browser build: Web Workers, each with its
/// own wasm instance). The [`Renderer`] keeps the queue (priorities, per-slot de-duplication) and
/// hands jobs over one at a time.
pub trait RenderOffload {
    /// Start `job` if an executor is free; otherwise give it back (`Some`: it stays queued).
    fn try_start(&mut self, slot: Slot, job: RenderJob) -> Option<RenderJob>;
    /// Finished jobs since the last call: (slot, result, run time in ms).
    fn finished(&mut self) -> Vec<(Slot, RenderResult, f64)>;
}

/// A job's result as the pool hands it back: the render and its texture pixels, converted on the
/// worker (`None` for prefetches and failures). A 10 MP zoom window's conversion took a few ms of
/// the UI thread on every slider step.
struct Finished {
    result: RenderResult,
    color: Option<Arc<egui::ColorImage>>,
}

/// Immutable identity captured when a request is submitted, not when its result is collected.
struct Requested {
    id: u64,
    source: Option<lcc_catalog::Source>,
}

struct Queued {
    slot: Slot,
    priority: u32,
    seq: u64,
    job: RenderJob,
}

/// The texture of a render: its pixels as egui wants them. Renders are opaque, so the common case
/// is a plain copy; only a pixel with alpha pays for premultiplying. (The generic conversion, on
/// the UI thread, was most of the time a 5 MP zoom window took to show.)
pub(crate) fn color_image(img: &lcc_raster::Rgba8) -> egui::ColorImage {
    let pixels = img
        .data
        .iter()
        .map(|p| if p[3] == 255 { egui::Color32::from_rgb(p[0], p[1], p[2]) } else { egui::Color32::from_rgba_unmultiplied(p[0], p[1], p[2], p[3]) })
        .collect();
    egui::ColorImage { size: [img.width, img.height], source_size: egui::vec2(img.width as f32, img.height as f32), pixels }
}

/// How far over the budget the open photo's window and before caches may run before they are cut.
const HARD_FACTOR: usize = 4;

/// Which stage caches to clear, in order, so that the rest fit `budget` bytes.
///
/// What nothing is looking at goes first, the cheapest to rebuild and least reused first (hover,
/// compare, second window, background work). The open photo's own views are what a slider drag
/// reuses on every tick, and a cache cleared every frame is rebuilt every frame (a drag on a
/// 2800 px canvas got 3.6x slower), so they stay — counted and reported — up to
/// [`HARD_FACTOR`] times the budget; past that (a runaway: 8:1 on a huge canvas, both sides of
/// Before/After) the before side's and the zoom window's go, the before side first. The
/// whole-frame view's cache is never named. Empty caches are never named.
///
/// An HDR view's cache ([`Slot::HdrView`]) lives as long as its 8-bit view's (a drag reuses it
/// as much), but the HDR caches together hold no more than `budget` on top: past it they go,
/// the idle ones first and the loupe's last.
pub(crate) fn stage_trim_order(sizes: &[(Slot, usize)], budget: usize) -> Vec<Slot> {
    let view_slot = |s: Slot| match s {
        Slot::HdrView(HDR_LOUPE) => Slot::Main,
        Slot::HdrView(HDR_BEFORE) => Slot::Before,
        Slot::HdrView(HDR_SECOND) => Slot::Second,
        Slot::HdrView(v) if v >= HDR_ZOOMED => Slot::Window(v - HDR_ZOOMED),
        Slot::HdrView(v) if v >= HDR_COMPARE => Slot::Compare(v - HDR_COMPARE),
        _ => s,
    };
    let idle_rank = |s: Slot| match view_slot(s) {
        Slot::Hover => 0,
        Slot::Compare(_) => 2,
        Slot::Second => 3,
        _ => 5,
    };
    let view_rank = |s: Slot| match view_slot(s) {
        Slot::Window(PANE_BEFORE) => Some(0),
        Slot::Before => Some(1),
        Slot::Window(_) => Some(2),
        _ => None,
    };
    let is_view = |s: Slot| matches!(view_slot(s), Slot::Main) || view_rank(s).is_some();
    let mut held: usize = sizes.iter().map(|(_, b)| *b).fold(0, usize::saturating_add);
    let mut out = Vec::new();
    let mut idle: Vec<(Slot, usize)> = sizes.iter().copied().filter(|(s, b)| *b > 0 && !is_view(*s)).collect();
    idle.sort_by_key(|(s, _)| idle_rank(*s));
    for (slot, bytes) in idle {
        if held <= budget {
            break;
        }
        held = held.saturating_sub(bytes);
        out.push(slot);
    }
    let hard = budget.saturating_mul(HARD_FACTOR);
    let mut views: Vec<(Slot, usize, u8)> = sizes.iter().filter_map(|(s, b)| view_rank(*s).filter(|_| *b > 0).map(|r| (*s, *b, r))).collect();
    views.sort_by_key(|(_, _, r)| *r);
    for (slot, bytes, _) in views {
        if held <= hard {
            break;
        }
        held = held.saturating_sub(bytes);
        out.push(slot);
    }
    let hdr_rank = |s: Slot| match s {
        Slot::HdrView(HDR_LOUPE) => 5,
        Slot::HdrView(v) if v == HDR_ZOOMED + PANE_AFTER => 4,
        Slot::HdrView(HDR_BEFORE) => 3,
        Slot::HdrView(v) if v == HDR_ZOOMED + PANE_BEFORE => 2,
        Slot::HdrView(HDR_SECOND) => 1,
        _ => 0,
    };
    let mut hdr: Vec<(Slot, usize)> = sizes.iter().copied().filter(|(s, b)| *b > 0 && matches!(s, Slot::HdrView(_)) && !out.contains(s)).collect();
    let mut hdr_held = hdr.iter().map(|(_, b)| *b).fold(0, usize::saturating_add);
    hdr.sort_by_key(|(s, _)| hdr_rank(*s));
    for (slot, bytes) in hdr {
        if hdr_held <= budget {
            break;
        }
        hdr_held = hdr_held.saturating_sub(bytes);
        out.push(slot);
    }
    out
}

/// The share of the memory budget the per-view stage caches (the loupe's, the zoom window's, the
/// before / hover renders') may hold together before the idle ones are cleared (see
/// [`stage_trim_order`] for the open photo's own): they are the decoded-image sized buffers that the
/// engine's own caches do not count.
const STAGE_BUDGET_SHARE: usize = 4;

/// Variant thumbnails kept as textures (LRU), when nothing asks for more.
pub const VARIANT_TEXTURES: usize = 96;
/// The most a view can raise that to ([`Renderer::want_variants`]): a screenful of small faces is a few hundred pictures of
/// a few tens of kilobytes each.
pub const MAX_VARIANT_TEXTURES: usize = 800;
/// Priority of variant thumbnail jobs: below on-screen grid thumbnails and the loupe.
const VARIANT_PRIORITY: u32 = 6;

pub struct Renderer {
    #[cfg(test)]
    pub(crate) thumb_jobs_built: usize,
    requests: HashMap<Slot, Requested>,
    #[cfg(test)]
    failure_messages: Vec<String>,
    /// Request id of the pixels each interactive view (loupe, before, hover) shows: a draft that
    /// finishes after a newer request was made still replaces older pixels, so a long slider drag
    /// keeps updating the loupe instead of waiting for the drag to pause.
    shown_ids: HashMap<Slot, u64>,
    /// Requests made before this id belong to a library or preview cache that is gone.
    epoch: u64,
    preview_generation: Option<(std::sync::Weak<lcc_preview::PreviewCache>, u64)>,
    /// Last thumbnail inputs; weak identities do not retain photo histories or force deep clones.
    /// (photo, size bucket, job key, whether the request went through an embedded stand-in)
    thumb_inputs: HashMap<PhotoId, (std::sync::Weak<lcc_catalog::Photo>, usize, u64, bool)>,
    /// Photos whose grid thumbnail is the camera's embedded preview itself (no render behind it).
    embedded_thumbs: HashMap<PhotoId, (std::sync::Weak<lcc_catalog::Photo>, usize, u64)>,
    pool: JobPool<Slot, Finished>,
    /// Jobs run elsewhere (see [`RenderOffload`]); `queue` holds the ones not started yet.
    offload: Option<Box<dyn RenderOffload>>,
    queue: Vec<Queued>,
    seq: u64,
    /// Slot → (key, priority) of the request in flight (queued or running).
    pending: HashMap<Slot, (u64, u32)>,
    pub textures: HashMap<Slot, Tex>,
    /// The last [`Slot::HdrHistogram`] result: (photo, job key, histogram).
    pub live_hdr_histogram: Option<(PhotoId, u64, Histogram)>,
    /// The last [`Slot::PanelHistogram`] result: (photo, job key, histogram with its waveform).
    pub panel_histogram: Option<(PhotoId, u64, Histogram)>,
    /// The [`Slot::HdrView`] renders not yet uploaded: view → (photo, job key, linear float image).
    pub hdr_view_result: HashMap<u8, (PhotoId, u64, lcc_pipeline::DeepImage)>,
    /// Each view's uploaded HDR texture.
    pub hdr_view: HashMap<u8, HdrView>,
    hdr_geometry: HashMap<u8, HdrGeometry>,
    /// How long each view's latest HDR render took (ms): what a high quality drag times when the
    /// HDR texture is what shows it.
    pub hdr_view_ms: HashMap<u8, f64>,
    pub last_main_ms: f64,
    /// Jobs finished since start (for inspect/perf).
    pub completed: u64,
    /// Keep a CPU copy of every texture so the UI can be rasterized headlessly (set when the app
    /// is driven by the control channel).
    pub keep_pixels: bool,
    /// Per-view intermediate results (loupe and "before"), so slider drags redo only what changed.
    stages: HashMap<Slot, Arc<StageCache>>,
    /// Slot → key of the last quick job requested for it (each is tried once).
    quick_tried: HashMap<Slot, u64>,
    /// Jobs that failed (unreadable or missing file…), by slot and job key, with the error and
    /// the photo: not requested again until the key changes (an edit, another size) — the grid
    /// asks every frame.
    failed: HashMap<Slot, (u64, String, u64, PhotoId)>,
    /// The catalog revision seen at the last poll: a failure is retried once the catalog changes
    /// (a relinked or re-imported file).
    catalog_rev: u64,
    /// Prefetch slot → key of the last job submitted (each runs once).
    prefetched: HashMap<Slot, u64>,
    /// Variant key → frame it was last asked for (LRU of [`Slot::Variant`] textures).
    variant_used: HashMap<u64, u64>,
    /// How many variants the last frame's views showed at once (see [`Self::want_variants`]).
    variant_want: usize,
    /// Frames polled so far.
    frame: u64,
    /// Slideshow slides may be held ([`Self::release_slides`] has something to do).
    slides_live: bool,
    /// Stage caches cleared to stay in budget (see [`stage_trim_order`]).
    stages_trimmed: u64,
    /// A budget other than the memory budget's share (tests).
    #[cfg(test)]
    pub(crate) stage_budget_override: Option<usize>,
    /// Tiles of zoomed views, by what they show (see [`crate::region`]).
    pub tiles: crate::region::TileCache<Tex>,
    /// Tile slot → (job key, what it shows, window rendered, tile within it) of its request.
    tile_meta: HashMap<Slot, (u64, crate::region::TileKey, lcc_engine::pipeline::PixelWindow, lcc_engine::pipeline::PixelWindow)>,
    /// Request id → where a [`Slot::Window`] request's pixels go (a late draft is still shown, so
    /// each request keeps its own; bounded).
    window_req: HashMap<u64, WindowGeom>,
    /// Where each [`Slot::Window`] texture's pixels go.
    window_shown: HashMap<Slot, WindowGeom>,
    /// The monitor profile textures are made for (`app.displayProfile`; `None`: sRGB).
    display: Option<Arc<lcc_engine::display::Display>>,
    /// Since when nothing has been pending, and whether the GPU pool was trimmed since.
    #[cfg(not(target_arch = "wasm32"))]
    idle: Option<(std::time::Instant, bool)>,
    /// Surface is extended linear sRGB (macOS EDR / Windows scRGB): skip monitor profile
    /// conversion, let the compositor do it (P2-3).
    surface_is_extended_linear: bool,
}

/// After this long without renders the GPU renderer's pool of recycled buffers is freed.
#[cfg(not(target_arch = "wasm32"))]
const IDLE_TRIM: std::time::Duration = std::time::Duration::from_secs(3);

impl Slot {
    /// An interactive view of the open photo: slider drags redo only the stages they feed, and a
    /// draft that finishes late still replaces older pixels.
    pub fn is_view(self) -> bool {
        matches!(self, Slot::Main | Slot::Window(_) | Slot::Before | Slot::Hover | Slot::HdrHistogram | Slot::HdrView(_))
    }
}

impl Default for Renderer {
    fn default() -> Self {
        let threads = if cfg!(target_arch = "wasm32") { 0 } else { JobPool::<Slot, Finished>::default_threads().min(6) };
        Self::with_worker_threads(threads)
    }
}

impl Renderer {
    /// Explicit worker count for hosts and bounded validation runs (one to six on native).
    /// Browser hosts continue to use inline jobs or their installed worker offload.
    pub fn with_worker_threads(threads: usize) -> Self {
        let threads = if cfg!(target_arch = "wasm32") { 0 } else { threads.clamp(1, 6) };
        Renderer {
            #[cfg(test)]
            thumb_jobs_built: 0,
            requests: HashMap::new(),
            #[cfg(test)]
            failure_messages: Vec::new(),
            shown_ids: HashMap::new(),
            epoch: 0,
            preview_generation: None,
            pool: JobPool::new(threads),
            thumb_inputs: HashMap::new(),
            embedded_thumbs: HashMap::new(),
            offload: None,
            queue: Vec::new(),
            seq: 0,
            pending: HashMap::new(),
            textures: HashMap::new(),
            live_hdr_histogram: None,
            panel_histogram: None,
            hdr_view_result: HashMap::new(),
            hdr_view: HashMap::new(),
            hdr_geometry: HashMap::new(),
            hdr_view_ms: HashMap::new(),
            last_main_ms: 0.0,
            completed: 0,
            keep_pixels: false,
            stages: HashMap::new(),
            tiles: Default::default(),
            tile_meta: HashMap::new(),
            window_req: HashMap::new(),
            window_shown: HashMap::new(),
            quick_tried: HashMap::new(),
            failed: HashMap::new(),
            catalog_rev: 0,
            prefetched: HashMap::new(),
            variant_used: HashMap::new(),
            variant_want: 0,
            frame: 0,
            slides_live: false,
            stages_trimmed: 0,
            #[cfg(test)]
            stage_budget_override: None,
            display: None,
            #[cfg(not(target_arch = "wasm32"))]
            idle: None,
            surface_is_extended_linear: false,
        }
    }

    /// Set whether the surface is extended linear sRGB (macOS EDR / Windows scRGB). When true,
    /// skip monitor profile conversion and let the compositor do it (P2-3).
    pub fn set_surface_extended_linear(&mut self, extended: bool) {
        if self.surface_is_extended_linear != extended {
            self.surface_is_extended_linear = extended;
            // Forget all textures so they are re-rendered without the display profile.
            self.forget_all();
        }
    }
}

impl Renderer {
    /// Would asking for this thumbnail again (at `priority`) change nothing?
    /// The size bucket of the thumbnail this photo has or is getting, while the photo is unchanged.
    pub fn thumb_bucket_kept(&self, photo: &Arc<lcc_catalog::Photo>) -> Option<usize> {
        self.thumb_inputs.get(&photo.id).filter(|(old, ..)| old.as_ptr() == Arc::as_ptr(photo)).map(|(_, size, ..)| *size)
    }

    pub fn thumb_current(&self, photo: &Arc<lcc_catalog::Photo>, bucket: usize, priority: u32) -> bool {
        let Some(&(ref old, size, key, quick)) = self.thumb_inputs.get(&photo.id) else { return false };
        // a larger thumbnail serves a smaller request: two views of the same photo asking for different sizes (the
        // Slideshow and Print modules' filmstrip and their own pictures) would otherwise each replace the other's
        // and render it again on every frame
        if old.as_ptr() != Arc::as_ptr(photo) || size < bucket {
            return false;
        }
        let slot = Slot::Thumb(photo.id);
        if quick && !self.textures.contains_key(&slot) {
            // Stand-in path (`grid::request_thumb`): the embedded preview is still on its way, or
            // the real thumbnail is requested at the background priority whatever the caller's.
            let quick_slot = Slot::ThumbQuick(photo.id);
            if let Some(&(k, p)) = self.pending.get(&quick_slot)
                && k == key
            {
                return p == priority.saturating_add(1) || !self.is_queued(quick_slot);
            }
            return !self.request_needed(slot, key, crate::panels::grid::BACKGROUND_THUMB_PRIORITY);
        }
        !self.request_needed(slot, key, priority)
    }

    /// Does a larger thumbnail of this photo, as it is now, already serve a request for `bucket`? It
    /// is shown, or rendering, at the larger size. Two views asking for one photo at different sizes
    /// (the slideshow or print preview and the navigator, say) otherwise replaced each other's request
    /// for the one thumbnail slot every frame: neither render was ever kept, and the photo rendered
    /// non-stop while the module was open. The larger size wins and the smaller view scales it down.
    pub fn thumb_covers(&self, photo: &Arc<lcc_catalog::Photo>, bucket: usize) -> bool {
        let Some(&(ref old, size, key, _)) = self.thumb_inputs.get(&photo.id) else { return false };
        old.as_ptr() == Arc::as_ptr(photo) && size > bucket && self.wanted(Slot::Thumb(photo.id)) == Some(key)
    }

    /// Remember the inputs of the thumbnail just requested (`quick`: through a stand-in).
    /// Whether the camera's embedded preview *is* this photo's thumbnail: its quick render arrived
    /// (so `Slot::ThumbQuick` holds the picture) and no edit has moved the photo on to a settings
    /// hash of its own. The grid stops here instead of decoding the raw behind a thumbnail the
    /// embedded JPEG already covers — see `grid::skips_the_real_render`.
    pub fn thumb_quick_is_final(&self, photo: &Arc<lcc_catalog::Photo>, bucket: usize, priority: u32) -> bool {
        let Some((old, size, key)) = self.embedded_thumbs.get(&photo.id) else { return false };
        let slot = Slot::ThumbQuick(photo.id);
        // a queued stand-in at another priority isn't settled yet: the grid moves it (both ways)
        let requeue = self.pending.get(&slot).is_some_and(|&(k, p)| k == *key && p != priority.saturating_add(1) && self.is_queued(slot));
        old.as_ptr() == Arc::as_ptr(photo) && *size >= bucket && !requeue && !self.request_needed(slot, *key, priority)
    }

    /// Whether this photo's embedded-preview thumbnail is pending at the requested priority, or
    /// already running. A queued priority change must reach the grid's stand-in request.
    pub fn thumb_quick_pending(&self, photo: &Arc<lcc_catalog::Photo>, bucket: usize, priority: u32) -> bool {
        let Some((old, size, key, quick)) = self.thumb_inputs.get(&photo.id) else { return false };
        let slot = Slot::ThumbQuick(photo.id);
        old.as_ptr() == Arc::as_ptr(photo)
            && *size == bucket
            && *quick
            && self.pending.get(&slot).is_some_and(|&(k, p)| {
                // a queued stand-in follows the grid's priority both ways (scrolling demotes it too);
                // `request_quick` only reprioritizes it, so this never restarts work
                k == *key && (p == priority.saturating_add(1) || !self.is_queued(slot))
            })
    }

    /// Remember that the camera's embedded preview is this photo's final thumbnail, so the grid's
    /// per-frame requests stop at it (the quick render's key, which [`Self::thumb_quick_is_final`]
    /// compares against).
    pub fn remember_embedded_thumb(&mut self, photo: &Arc<lcc_catalog::Photo>, bucket: usize, key: u64) {
        let slot = Slot::Thumb(photo.id);
        if self.pending.get(&slot).is_some_and(|(pending_key, _)| *pending_key != key) {
            // An edit can still be rendering when Undo restores this embedded-final thumbnail.
            // Retire its full-render identity before it can replace the correct stand-in.
            self.pending.remove(&slot);
            self.requests.remove(&slot);
            self.pool.reprioritize(|s, p| if *s == slot { None } else { Some(p) });
            self.queue.retain(|q| q.slot != slot);
        }
        self.remember_thumb(photo, bucket, key, true);
        self.embedded_thumbs.insert(photo.id, (Arc::downgrade(photo), bucket, key));
    }

    pub fn remember_thumb(&mut self, photo: &Arc<lcc_catalog::Photo>, bucket: usize, key: u64, quick: bool) {
        if self.thumb_inputs.len() >= 1024 && !self.thumb_inputs.contains_key(&photo.id) {
            self.thumb_inputs.retain(|_, (p, _, _, _)| p.strong_count() > 0);
            if self.thumb_inputs.len() >= 1024
                && let Some(id) = self.thumb_inputs.keys().next().copied()
            {
                self.thumb_inputs.remove(&id);
            }
        }
        self.thumb_inputs.insert(photo.id, (Arc::downgrade(photo), bucket, key, quick));
    }

    /// Views that render straight into the display profile's primaries (wide gamut); everything
    /// else is rendered for sRGB and converted ([`lcc_engine::display::present`]).
    fn renders_for_display(slot: Slot) -> bool {
        matches!(slot, Slot::Main | Slot::Before | Slot::Hover | Slot::Compare(_) | Slot::Second | Slot::Slide(_))
    }

    /// `job` as [`Self::request`] would run it for `slot` (its key included): views render for
    /// the display profile in use.
    pub fn job_for(&self, slot: Slot, job: RenderJob) -> RenderJob {
        // When the surface is extended linear sRGB (macOS EDR / Windows scRGB), skip monitor
        // profile conversion: the compositor owns it (P2-3).
        if Self::renders_for_display(slot) && !self.surface_is_extended_linear { job.with_display(self.display.clone()) } else { job }
    }

    /// The display profile in use (id).
    pub fn display_id(&self) -> Option<u64> {
        self.display.as_ref().map(|d| d.id())
    }

    /// The display profile textures are made for (`None`: sRGB).
    pub fn display(&self) -> Option<&Arc<lcc_engine::display::Display>> {
        self.display.as_ref()
    }

    /// Show previews through `display` from now on (`app.displayProfile`): when it changes, every
    /// texture is made again for the new one.
    pub fn set_display(&mut self, display: Option<Arc<lcc_engine::display::Display>>) {
        if display.as_ref().map(|d| d.id()) != self.display_id() {
            self.forget_all();
        }
        self.display = display;
    }

    /// Run `job` for `slot` and make its image ready for `display` (on the worker thread). An
    /// HDR view's result is uploaded from its float samples, so it gets no 8-bit texture pixels.
    fn run_for(slot: Slot, job: impl FnOnce() -> RenderResult, display: &Option<Arc<lcc_engine::display::Display>>) -> Finished {
        let mut r = job();
        if let Some(d) = display {
            lcc_engine::display::present(&mut r, d);
        }
        let texture = !matches!(slot, Slot::HdrView(_));
        let color = r.rendered.as_ref().ok().filter(|_| texture).map(|x| Arc::new(color_image(&x.image)));
        Finished { result: r, color }
    }

    fn request_needed(&self, slot: Slot, key: u64, priority: u32) -> bool {
        // A matching old texture must not mask a different newer request (e.g. undo).
        if let Some(&(k, p)) = self.pending.get(&slot) {
            // a queued job moves up for a more urgent ask, never down: two views asking for the same thumbnail at
            // different priorities (the Slideshow and Print modules and their filmstrip) took it in turns every frame
            return k != key || (p < priority && self.is_queued(slot));
        }
        if self.textures.get(&slot).is_some_and(|t| t.key == key) || self.failed.get(&slot).is_some_and(|f| f.0 == key && f.2 == self.catalog_rev) {
            return false;
        }
        true
    }
    /// Free a view slot's texture and stage cache (the view is no longer shown).
    pub fn release(&mut self, slot: Slot) {
        self.textures.remove(&slot);
        self.stages.remove(&slot);
        self.window_shown.remove(&slot);
    }

    /// Run jobs through `offload` from now on (instead of the job pool / inline).
    pub fn set_offload(&mut self, offload: Box<dyn RenderOffload>) {
        self.offload = Some(offload);
    }

    fn is_queued(&self, slot: Slot) -> bool {
        self.queue.iter().any(|q| q.slot == slot) || self.pool.is_queued(slot)
    }

    /// Is the slot's current texture (or pending request) already for `key`?
    /// Key of what `slot` shows or is rendering next (the pending request wins).
    pub fn wanted(&self, slot: Slot) -> Option<u64> {
        self.pending.get(&slot).map(|p| p.0).or_else(|| self.textures.get(&slot).map(|t| t.key))
    }

    pub fn is_current(&self, slot: Slot, key: u64) -> bool {
        self.wanted(slot) == Some(key)
    }

    /// Retained HDR frames must cover the same region. Retire older requests on navigation.
    pub fn prepare_hdr_view(&mut self, view: u8, job: &RenderJob) {
        let r = &job.request;
        let geometry = HdrGeometry {
            photo: job.photo,
            // Whole-frame drafts can change resolution without changing their coverage.
            full: r.window.map(|_| (r.max_w, r.max_h)),
            window: r.window,
            crop: r.apply_crop.then_some(job.settings.crop),
            orientation: job.settings.orientation,
        };
        if self.hdr_geometry.get(&view) != Some(&geometry) {
            self.hdr_geometry.insert(view, geometry);
            self.hdr_view.remove(&view);
            self.hdr_view_result.remove(&view);
            let slot = Slot::HdrView(view);
            self.requests.remove(&slot);
            self.pending.remove(&slot);
            self.shown_ids.insert(slot, lcc_preview::next_tick());
        }
    }

    /// View `view`'s HDR frame of `photo` to draw for the render `key`: that render's, or an
    /// older one while `key` is still rendering. Once nothing renders `key` (it failed, or was
    /// retired with no replacement) an older frame is stale and the view shows its 8-bit render.
    pub fn hdr_frame(&self, view: u8, photo: PhotoId, key: u64) -> Option<HdrView> {
        let held = self.hdr_view.get(&view).filter(|v| v.photo == photo)?;
        (held.key == key || self.pending.get(&Slot::HdrView(view)).is_some_and(|p| p.0 == key)).then_some(*held)
    }

    /// Request a render for `slot` (no-op if already current or pending at the same priority).
    pub fn request(&mut self, slot: Slot, job: RenderJob, priority: u32) {
        if matches!(slot, Slot::Slide(_)) {
            self.slides_live = true;
        }
        let mut job = self.job_for(slot, job);
        if !self.request_needed(slot, job.key, priority) {
            return;
        }
        self.pending.insert(slot, (job.key, priority));
        job.request_id = lcc_preview::next_tick();
        self.requests.insert(slot, Requested { id: job.request_id, source: Some(job.origin.clone()) });
        // (an offload keeps its own per-view stage caches; the flag tells it to)
        let job = if slot.is_view() { job.with_stages(self.stages.entry(slot).or_default().clone()) } else { job };
        if self.offload.is_some() {
            self.seq += 1;
            self.queue.retain(|q| q.slot != slot);
            self.queue.push(Queued { slot, priority, seq: self.seq, job });
            self.dispatch(); // an idle worker starts now rather than next frame
            return;
        }
        let key = job.key;
        let background = matches!(slot, Slot::Thumb(_) | Slot::ThumbQuick(_) | Slot::Prefetch(_));
        let display = self.display.clone();
        self.pool.submit(
            slot,
            key,
            priority,
            Box::new(move || {
                let run = || if background { lcc_engine::memory::in_background(|| job.run()) } else { job.run() };
                Self::run_for(slot, run, &display)
            }),
        );
    }

    /// Request a stand-in for `slot` (once per job key).
    pub fn request_quick(&mut self, slot: Slot, mut job: QuickJob, priority: u32) {
        if self.quick_tried.get(&slot) == Some(&job.key) {
            if self.pending.get(&slot).is_some_and(|p| p.0 == job.key && p.1 != priority) {
                // Scrolling changes a stand-in's priority too. Keep the original closure and
                // request identity; running or completed work must never be started again.
                let mut queued = false;
                self.pool.reprioritize(|s, p| {
                    if *s == slot {
                        queued = true;
                        Some(priority)
                    } else {
                        Some(p)
                    }
                });
                if queued && let Some(p) = self.pending.get_mut(&slot) {
                    p.1 = priority;
                }
            }
            return;
        }
        self.quick_tried.insert(slot, job.key);
        self.pending.insert(slot, (job.key, priority));
        job.request_id = lcc_preview::next_tick();
        let source = job
            .small
            .as_ref()
            .map(|job| job.origin.clone())
            .or_else(|| job.embedded.as_ref().map(|(path, _, _)| lcc_catalog::Source::File { path: path.clone() }));
        self.requests.insert(slot, Requested { id: job.request_id, source });
        let key = job.key;
        let display = self.display.clone();
        self.pool.submit(slot, key, priority, Box::new(move || Self::run_for(slot, || job.run(), &display)));
    }

    /// Render a slideshow title screen into `slot` (once per screen key, like [`Self::request`]):
    /// a slide with no photo, composed on a worker (on wasm: the job pool's inline runs, not the
    /// offload, which only knows photo renders).
    pub fn request_title(&mut self, slot: Slot, compose: Arc<lcc_engine::slide_title::TitleCompose>, priority: u32) {
        if matches!(slot, Slot::Slide(_)) {
            self.slides_live = true;
        }
        let key = compose.key();
        if !self.request_needed(slot, key, priority) {
            return;
        }
        self.pending.insert(slot, (key, priority));
        let request_id = lcc_preview::next_tick();
        self.requests.insert(slot, Requested { id: request_id, source: None });
        let display = self.display.clone();
        self.pool.submit(slot, key, priority, Box::new(move || Self::run_for(slot, || compose.result(request_id), &display)));
    }

    /// Prepare a photo in the background (a [`Slot::Prefetch`] job runs once per key; a newer one
    /// replaces it while queued). Memory stays bounded by the engine's source caches.
    pub fn prefetch(&mut self, slot: Slot, mut job: RenderJob, priority: u32) {
        if self.prefetched.get(&slot) == Some(&job.key) {
            return;
        }
        self.prefetched.insert(slot, job.key);
        self.pending.insert(slot, (job.key, priority));
        job.request_id = lcc_preview::next_tick();
        self.requests.insert(slot, Requested { id: job.request_id, source: Some(job.origin.clone()) });
        let key = job.key;
        self.pool.submit(slot, key, priority, Box::new(move || Finished { result: lcc_engine::memory::in_background(|| job.run()), color: None }));
    }

    /// Drop the slideshow slides other than `keep`: their textures and their requests that have
    /// not started (a slide that is no longer near is not worth rendering).
    pub fn release_slides(&mut self, keep: &[lcc_engine::slideshow::Item]) {
        if !self.slides_live {
            return;
        }
        let gone = |s: &Slot| matches!(s, Slot::Slide(id) if !keep.contains(id));
        self.textures.retain(|s, _| !gone(s));
        let _ = self.pool.reprioritize(|s, p| if gone(s) { None } else { Some(p) });
        self.queue.retain(|q| !gone(&q.slot));
        // (a render already running finishes unclaimed: its result is dropped)
        self.pending.retain(|s, _| !gone(s));
        self.requests.retain(|s, _| !gone(s));
        self.failed.retain(|s, _| !gone(s));
        self.slides_live = !keep.is_empty();
    }

    /// Is a request for `slot` queued or running?
    /// Drop every texture and cached stage (another library was opened: photo ids changed meaning).
    pub fn forget_all(&mut self) {
        self.requests.clear();
        self.shown_ids.clear();
        self.epoch = lcc_preview::next_tick();
        self.preview_generation = None;
        self.pool.reprioritize(|_, _| None);
        self.thumb_inputs.clear();
        self.textures.clear();
        self.stages.clear();
        self.quick_tried.clear();
        self.failed.clear();
        self.prefetched.clear();
        self.pending.clear();
        self.queue.clear();
        self.tiles.clear();
        self.tile_meta.clear();
        self.window_req.clear();
        self.window_shown.clear();
        self.hdr_view.clear();
        self.hdr_view_result.clear();
        self.hdr_geometry.clear();
        self.hdr_view_ms.clear();
    }

    /// Ask for the drag window of `pane` ([`Slot::Window`]): `job` renders `geom.window` of the
    /// frame, with the pane's own stage cache, so a drag redoes only the stages it feeds.
    pub fn request_window(&mut self, pane: u8, job: RenderJob, geom: WindowGeom, priority: u32) {
        let slot = Slot::Window(pane);
        self.request(slot, job, priority);
        if let Some(id) = self.requests.get(&slot).map(|r| r.id) {
            if self.window_req.len() >= 32 {
                // the oldest requests go: their results are superseded by now
                let mut ids: Vec<u64> = self.window_req.keys().copied().collect();
                ids.sort_unstable();
                for old in ids.into_iter().take(16) {
                    self.window_req.remove(&old);
                }
            }
            self.window_req.insert(id, geom);
        }
    }

    /// The drag window `pane` shows, and where its pixels go.
    pub fn window_texture(&self, pane: u8) -> Option<(&Tex, WindowGeom)> {
        let slot = Slot::Window(pane);
        Some((self.textures.get(&slot)?, *self.window_shown.get(&slot)?))
    }

    /// Ask for tile `key`: `job` renders `window` of the frame, of which `tile` is kept. Nothing
    /// happens when the tile is held or already asked for.
    pub fn request_tile(
        &mut self,
        key: crate::region::TileKey,
        job: RenderJob,
        window: lcc_engine::pipeline::PixelWindow,
        tile: lcc_engine::pipeline::PixelWindow,
        priority: u32,
    ) {
        if self.tiles.has_render(&key) {
            return;
        }
        let slot = Slot::Tile { pane: key.pane, col: key.tile.col, row: key.tile.row };
        self.tile_meta.insert(slot, (job.key, key, window, tile));
        self.request(slot, job, priority);
    }

    /// Show `img` for tile `key` until its render arrives (a part of a 1:1 preview).
    pub fn tile_placeholder(&mut self, ctx: &egui::Context, key: crate::region::TileKey, img: &lcc_raster::Rgba8) {
        if self.tiles.contains(&key) {
            return;
        }
        let tex = self.tile_tex(ctx, key, img, 0.0);
        self.tiles.insert(key, tex, img.width.saturating_mul(img.height).saturating_mul(4), true);
    }

    fn tile_tex(&self, ctx: &egui::Context, key: crate::region::TileKey, img: &lcc_raster::Rgba8, ms: f64) -> Tex {
        let color = std::sync::Arc::new(color_image(img));
        let pixels = self.keep_pixels.then(|| color.clone());
        let tex = ctx.load_texture(format!("tile {:?} {:?}", key.tile, key.pane), color, egui::TextureOptions::LINEAR);
        Tex { key: key.look, photo: key.photo, tex, size: [img.width, img.height], histogram: None, ms, quick: None, pixels, warnings: Vec::new() }
    }

    /// Drop queued tile renders `keep` says no to (tiles scrolled away). Running ones finish and
    /// are kept: their pixels are good.
    pub fn cancel_tiles(&mut self, keep: impl Fn(Slot) -> bool) {
        let drop_it = |s: &Slot| matches!(s, Slot::Tile { .. }) && !keep(*s);
        let mut dropped = self.pool.reprioritize(|s, p| if drop_it(s) { None } else { Some(p) });
        self.queue.retain(|q| {
            if drop_it(&q.slot) {
                dropped.push(q.slot);
                false
            } else {
                true
            }
        });
        for s in dropped {
            self.pending.remove(&s);
            self.requests.remove(&s);
            self.tile_meta.remove(&s);
        }
    }

    /// Tile renders queued or running.
    pub fn tiles_pending(&self) -> usize {
        self.pending.keys().filter(|s| matches!(s, Slot::Tile { .. })).count()
    }

    pub fn is_pending(&self, slot: Slot) -> bool {
        self.pending.contains_key(&slot)
    }

    /// The app is closing: drop the jobs that have not started and wait up to `timeout` for the
    /// ones running on the worker threads. Returns whether all of them have ended. Nothing is
    /// rendered afterwards.
    pub fn shutdown(&mut self, timeout: std::time::Duration) -> bool {
        self.queue.clear();
        self.pool.shutdown(timeout)
    }

    /// Worker threads that are still alive (0 before the first render).
    pub fn live_workers(&self) -> usize {
        self.pool.live_workers()
    }

    /// The texture to show for a grid/filmstrip thumbnail: the rendered one, else its stand-in.
    pub fn thumb(&self, id: PhotoId) -> Option<&Tex> {
        self.textures.get(&Slot::Thumb(id)).or_else(|| self.textures.get(&Slot::ThumbQuick(id)))
    }

    pub fn queued(&self) -> usize {
        self.pool.queued() + self.queue.len()
    }

    /// Hand queued jobs to the offload (best first) while it has room.
    fn dispatch(&mut self) {
        let Some(off) = self.offload.as_mut() else { return };
        while let Some(i) = self.queue.iter().enumerate().max_by_key(|(_, q)| (q.priority, q.seq)).map(|(i, _)| i) {
            let q = self.queue.swap_remove(i);
            if let Some(job) = off.try_start(q.slot, q.job) {
                self.queue.push(Queued { job, ..q });
                break;
            }
        }
    }

    /// Requests queued or running.
    pub fn in_flight(&self) -> usize {
        self.pending.len()
    }

    /// Why the last render for `slot` failed (until a render with another key succeeds).
    pub fn failure(&self, slot: Slot) -> Option<&str> {
        self.failed.get(&slot).map(|f| f.1.as_str())
    }

    /// The job key of `slot`'s last failed render.
    pub fn failed_key(&self, slot: Slot) -> Option<u64> {
        self.failed.get(&slot).map(|f| f.0)
    }

    /// Why the last render of `photo` for `slot` failed: a view slot shows one photo after
    /// another, and another photo's error isn't this one's.
    pub fn failure_of(&self, slot: Slot, photo: PhotoId) -> Option<&str> {
        self.failed.get(&slot).filter(|f| f.3 == photo).map(|f| f.1.as_str())
    }

    /// Whether `slot`'s pending render asks again for the key that last failed (a library change
    /// retries failures): what is on screen stays what the failure left until it answers.
    pub fn retrying(&self, slot: Slot) -> bool {
        self.pending.get(&slot).zip(self.failed.get(&slot)).is_some_and(|(p, f)| p.0 == f.0)
    }

    /// The slots with a render pending (diagnostics: `ui.inspect`).
    pub fn pending_slots(&self) -> Vec<String> {
        self.pending.keys().map(|s| format!("{s:?}")).collect()
    }

    /// Thumbnail textures currently loaded.
    /// A variant thumbnail ([`Slot::Variant`]): its texture once rendered; until then the job is
    /// queued at low priority (below on-screen grid thumbnails). Call it every frame the variant
    /// is visible: variants not asked for during the last frames leave the queue, and textures
    /// beyond [`VARIANT_TEXTURES`] are evicted least recently used first.
    pub fn variant(&mut self, job: RenderJob) -> Option<&Tex> {
        let key = job.key;
        let slot = Slot::Variant(key);
        self.variant_used.insert(key, self.frame);
        if !self.textures.contains_key(&slot) {
            self.request(slot, job, VARIANT_PRIORITY);
        }
        self.textures.get(&slot)
    }

    /// A view that shows `n` variants at once (the People view's faces) says so each frame, so the cache holds them all:
    /// with a fixed budget a screen of more than [`VARIANT_TEXTURES`] pictures evicts, and re-requests, the same few every
    /// frame, and those tiles stay blank. The budget is the most asked for in the last frame, a quarter more, up to
    /// [`MAX_VARIANT_TEXTURES`].
    pub fn want_variants(&mut self, n: usize) {
        self.variant_want = self.variant_want.max(n);
    }

    /// How many variant textures are kept now.
    fn variant_budget(&self) -> usize {
        VARIANT_TEXTURES.max(self.variant_want + self.variant_want / 4).min(MAX_VARIANT_TEXTURES)
    }

    /// Variant thumbnail textures currently loaded.
    pub fn variant_textures(&self) -> usize {
        self.textures.keys().filter(|s| matches!(s, Slot::Variant(_))).count()
    }

    /// Drop variant jobs nobody asked for in the last frames, and the least recently used
    /// variant textures beyond the budget.
    fn evict_variants(&mut self) {
        let now = self.frame;
        let stale = |used: Option<&u64>| used.is_none_or(|f| now.saturating_sub(*f) > 2);
        let used = &self.variant_used;
        let dropped = self.pool.reprioritize(|s, p| match s {
            Slot::Variant(k) if stale(used.get(k)) => None,
            _ => Some(p),
        });
        for s in dropped {
            self.pending.remove(&s);
            self.requests.remove(&s);
        }
        let requests = &mut self.requests;
        let pending = &mut self.pending;
        self.queue.retain(|q| match q.slot {
            Slot::Variant(k) if stale(used.get(&k)) => {
                pending.remove(&q.slot);
                requests.remove(&q.slot);
                false
            }
            _ => true,
        });
        let budget = self.variant_budget();
        self.variant_want = 0;
        let n = self.variant_textures();
        if n > budget {
            let mut have: Vec<(u64, u64)> = self
                .textures
                .keys()
                .filter_map(|s| if let Slot::Variant(k) = s { Some((self.variant_used.get(k).copied().unwrap_or(0), *k)) } else { None })
                .collect();
            have.sort_unstable();
            for (_, k) in have.into_iter().take(n - budget) {
                self.textures.remove(&Slot::Variant(k));
                self.variant_used.remove(&k);
            }
        }
        if self.variant_used.len() > 4 * budget {
            let textures = &self.textures;
            let pending = &self.pending;
            self.variant_used.retain(|k, _| textures.contains_key(&Slot::Variant(*k)) || pending.contains_key(&Slot::Variant(*k)));
        }
    }

    pub fn thumb_textures(&self) -> usize {
        self.textures.keys().filter(|s| matches!(s, Slot::Thumb(_) | Slot::ThumbQuick(_))).count()
    }

    /// Collect finished jobs into textures. Returns true if anything changed.
    pub fn poll(&mut self, ctx: &egui::Context, session: &mut Session) -> bool {
        let cache = &session.media.rendered;
        let generation = cache.generation();
        if self.preview_generation.as_ref().is_some_and(|(old, g)| old.as_ptr() != Arc::as_ptr(cache) || *g != generation) {
            self.forget_all();
        }
        self.preview_generation = Some((Arc::downgrade(cache), generation));
        self.catalog_rev = session.catalog.revision;
        self.frame += 1;
        self.trim_stages();
        // wasm: run one job per frame on this thread, timed with the host clock
        #[cfg(target_arch = "wasm32")]
        let inline_ms = {
            let t0 = crate::now_ms();
            self.pool.run_inline(1);
            crate::now_ms() - t0
        };
        #[cfg(not(target_arch = "wasm32"))]
        let inline_ms = 0.0;
        let changed = self.collect(ctx, session, inline_ms);
        self.evict_variants();
        #[cfg(not(target_arch = "wasm32"))]
        self.trim_when_idle(ctx);
        if !self.pending.is_empty() {
            ctx.request_repaint_after(std::time::Duration::from_millis(16));
        }
        changed
    }

    /// In a slider drag, give the loupe's draft (just requested) until `budget` to finish, so that it
    /// is drawn in this frame rather than picked up at the start of the next one. Only when the last
    /// loupe render took well under the budget (a GPU draft takes a few milliseconds; a CPU render
    /// that would only make the frame late is not waited for).
    #[cfg(not(target_arch = "wasm32"))]
    pub fn wait_for_draft(&mut self, ctx: &egui::Context, session: &mut Session, budget: std::time::Duration) {
        if self.offload.is_some() || self.last_main_ms * 1.5 >= budget.as_secs_f64() * 1e3 {
            return;
        }
        let Some(deadline) = std::time::Instant::now().checked_add(budget) else { return };
        while self.is_pending(Slot::Main) {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() || !self.pool.wait(left) {
                break;
            }
            self.collect(ctx, session, 0.0);
        }
    }

    /// Turn finished jobs into textures. Returns true if anything changed.
    fn collect(&mut self, ctx: &egui::Context, session: &mut Session, inline_ms: f64) -> bool {
        let mut finished = Vec::new();
        while let Some(done) = self.pool.try_recv() {
            finished.push((done.slot, done.result.result, done.result.color, if done.ms > 0.0 { done.ms } else { inline_ms }));
        }
        if let Some(off) = self.offload.as_mut() {
            finished.extend(off.finished().into_iter().map(|(slot, r, ms)| (slot, r, None, ms)));
        }
        self.dispatch();
        let mut changed = false;
        for (mut slot, mut r, mut color, ms) in finished {
            self.completed += 1;
            // made for another display profile than the one in use: obsolete
            if r.display.is_some_and(|id| Some(id) != self.display_id()) {
                continue;
            }
            // Ignore obsolete pixels, failures and decoded sources, even for the same render key;
            // an interactive view still takes a superseded draft that is newer than what it
            // shows, and the source it decoded: a drag supersedes every request before it
            // finishes, and a source thrown away with them is decoded again for every tick.
            let source = if self.requests.get(&slot).map(|request| request.id) != Some(r.request_id) {
                let newer_draft = slot.is_view()
                    && self.requests.contains_key(&slot)
                    && r.request_id >= self.epoch
                    && self.shown_ids.get(&slot).is_none_or(|shown| r.request_id > *shown)
                    && r.rendered.is_ok();
                if !newer_draft {
                    continue;
                }
                session.accept(&r);
                None // superseded successful drafts have no failure diagnostic
            } else {
                let source = self.requests.remove(&slot).and_then(|request| request.source);
                session.accept(&r);
                source
            };
            if slot.is_view() {
                self.shown_ids.insert(slot, r.request_id);
            }
            // (results from an offload, which doesn't know the display profile: converted here)
            if let Some(d) = &self.display {
                let before = r.display;
                lcc_engine::display::present(&mut r, d);
                if r.display != before {
                    color = None; // converted after the worker made its texture pixels
                }
            }
            if self.pending.get(&slot).is_some_and(|p| p.0 == r.key) {
                self.pending.remove(&slot);
            }
            let rendered = match r.rendered {
                Ok(x) => {
                    self.failed.remove(&slot);
                    x
                }
                Err(e) => {
                    if !matches!(slot, Slot::Prefetch(_)) {
                        let message = render_failure_message(slot, r.photo, source.as_ref(), &e);
                        #[cfg(test)]
                        self.failure_messages.push(message.clone());
                        log::warn!("{message}");
                        self.failed.insert(slot, (r.key, e, self.catalog_rev, r.photo));
                    }
                    // (an HDR view's older frame no longer shows what is asked for)
                    if let Slot::HdrView(view) = slot {
                        self.hdr_view.remove(&view);
                        self.hdr_view_result.remove(&view);
                    }
                    continue;
                }
            };
            if matches!(slot, Slot::Prefetch(_)) {
                continue;
            }
            if let Slot::Window(_) = slot {
                match self.window_req.get(&r.request_id) {
                    Some(g) => {
                        self.window_shown.insert(slot, *g);
                    }
                    None => continue,
                }
            }
            if let Slot::Tile { .. } = slot {
                if let Some(&(key, tile_key, window, tile)) = self.tile_meta.get(&slot)
                    && key == r.key
                    && let Some(img) = crate::region::crop_tile(&rendered.image, window, tile)
                {
                    let tex = self.tile_tex(ctx, tile_key, &img, ms);
                    self.tiles.insert(tile_key, tex, img.width.saturating_mul(img.height).saturating_mul(4), false);
                    changed = true;
                }
                continue;
            }
            if let Slot::HdrView(view) = slot {
                if let Some(deep) = rendered.deep {
                    self.hdr_view_result.insert(view, (r.photo, r.key, deep));
                    self.hdr_view_ms.insert(view, ms);
                    changed = true;
                }
                continue;
            }
            if slot == Slot::PanelHistogram {
                self.panel_histogram = Some((r.photo, r.key, rendered.histogram));
                changed = true;
                continue;
            }
            if slot == Slot::HdrHistogram {
                if rendered.histogram.hdr.is_some() {
                    self.live_hdr_histogram = Some((r.photo, r.key, rendered.histogram));
                    changed = true;
                }
                continue;
            }
            if let Slot::ThumbQuick(id) = slot {
                if self.textures.contains_key(&Slot::Thumb(id)) {
                    continue; // the real thumbnail won the race
                }
                if r.quick == Some(QuickSource::Cached) {
                    // the photo's own cached thumbnail: final
                    slot = Slot::Thumb(id);
                }
            }
            if let Slot::Thumb(id) = slot {
                self.textures.remove(&Slot::ThumbQuick(id));
            }
            let img = &rendered.image;
            let color = color.unwrap_or_else(|| Arc::new(color_image(img)));
            // the loupe's render stays readable on the CPU for the colour readout under the pointer
            let pixels = (self.keep_pixels || slot == Slot::Main).then(|| color.clone());
            let name = format!("{slot:?}");
            match self.textures.get_mut(&slot) {
                Some(t) => {
                    t.tex.set(color, egui::TextureOptions::LINEAR);
                    t.pixels = pixels;
                    t.key = r.key;
                    t.photo = r.photo;
                    t.size = [img.width, img.height];
                    t.histogram = Some(keep_waveform(slot, rendered.histogram));
                    t.warnings = rendered.warnings;
                    t.ms = ms;
                    t.quick = r.quick;
                }
                None => {
                    let tex = ctx.load_texture(name, color, egui::TextureOptions::LINEAR);
                    self.textures.insert(
                        slot,
                        Tex {
                            key: r.key,
                            photo: r.photo,
                            tex,
                            size: [img.width, img.height],
                            histogram: Some(keep_waveform(slot, rendered.histogram)),
                            ms,
                            quick: r.quick,
                            pixels,
                            warnings: rendered.warnings,
                        },
                    );
                }
            }
            if slot == Slot::Main {
                self.last_main_ms = ms;
            }
            changed = true;
        }
        changed
    }

    /// Free the GPU renderer's recycled buffers once nothing has rendered for [`IDLE_TRIM`] (they
    /// are reallocated by the next render; while working, renders reuse them).
    #[cfg(not(target_arch = "wasm32"))]
    fn trim_when_idle(&mut self, ctx: &egui::Context) {
        if !self.pending.is_empty() {
            self.idle = None;
            return;
        }
        let now = std::time::Instant::now();
        let (since, trimmed) = *self.idle.get_or_insert((now, false));
        if trimmed {
            return;
        }
        let waited = now.duration_since(since);
        if waited >= IDLE_TRIM {
            lcc_engine::gpu::trim_pool(0);
            lcc_engine::memory::release();
            self.idle = Some((since, true));
        } else {
            ctx.request_repaint_after(IDLE_TRIM - waited);
        }
    }

    /// Bytes the per-view stage caches (CPU images and GPU buffers) may hold together.
    pub fn stage_budget(&self) -> usize {
        #[cfg(test)]
        if let Some(b) = self.stage_budget_override {
            return b;
        }
        lcc_engine::memory::budget() / STAGE_BUDGET_SHARE
    }

    /// Clear stage caches, least useful first, until what they hold fits [`Self::stage_budget`].
    fn trim_stages(&mut self) {
        let sizes: Vec<(Slot, usize)> = self.stages.iter().map(|(slot, c)| (*slot, c.bytes() + lcc_engine::gpu::stage_bytes(c))).collect();
        for slot in stage_trim_order(&sizes, self.stage_budget()) {
            if let Some(c) = self.stages.get(&slot) {
                c.clear();
                self.stages_trimmed += 1;
            }
        }
    }

    /// What the renderer holds: per-view stage caches (CPU images and GPU buffers) and textures.
    pub fn memory(&self) -> serde_json::Value {
        let cpu: usize = self.stages.values().map(|s| s.bytes()).sum();
        let gpu: usize = self.stages.values().map(|s| lcc_engine::gpu::stage_bytes(s)).sum();
        let tex: usize = self.textures.values().map(|t| t.size[0] * t.size[1] * 4).sum();
        let copies: usize = self.textures.values().filter_map(|t| t.pixels.as_ref()).map(|p| p.pixels.len() * 4).sum();
        serde_json::json!({
            "stageCaches": {"count": self.stages.len(), "cpuBytes": cpu, "gpuBytes": gpu, "budgetBytes": self.stage_budget(), "trimmed": self.stages_trimmed, "sharedSourceBytes": lcc_engine::gpu::shared_source_bytes()},
            "textures": {"count": self.textures.len(), "bytes": tex, "cpuCopyBytes": copies},
            "tiles": {"count": self.tiles.len(), "bytes": self.tiles.bytes(), "budgetBytes": self.tiles.budget()},
        })
    }

    /// CPU copies of the current textures by id (see [`Self::keep_pixels`]).
    pub fn cpu_textures(&self) -> HashMap<egui::TextureId, crate::softpaint::CpuTexture> {
        self.textures
            .values()
            .chain(self.tiles.values())
            .filter_map(|t| Some((t.tex.id(), crate::softpaint::CpuTexture::linear(t.pixels.clone()?))))
            .collect()
    }

    /// Drop the import review thumbnails (a new review, or the dialog closed).
    pub fn forget_imports(&mut self) {
        self.pool.reprioritize(|s, p| if matches!(s, Slot::Import(_)) { None } else { Some(p) });
        self.queue.retain(|q| !matches!(q.slot, Slot::Import(_)));
        self.pending.retain(|s, _| !matches!(s, Slot::Import(_)));
        self.requests.retain(|s, _| !matches!(s, Slot::Import(_)));
        self.textures.retain(|s, _| !matches!(s, Slot::Import(_)));
        self.quick_tried.retain(|s, _| !matches!(s, Slot::Import(_)));
    }

    /// Drop thumbnails that aren't in `keep` (bounded memory for huge libraries), and queued
    /// thumbnail jobs for photos that scrolled out of `keep`.
    pub fn evict_thumbs(&mut self, keep: &std::collections::HashSet<PhotoId>, max: usize) {
        self.thumb_inputs.retain(|id, _| keep.contains(id));
        let dropped = self.pool.reprioritize(|s, p| match s {
            Slot::Thumb(id) | Slot::ThumbQuick(id) if !keep.contains(id) && p <= 11 => None,
            _ => Some(p),
        });
        for s in dropped {
            self.pending.remove(&s);
            self.requests.remove(&s);
            self.quick_tried.remove(&s);
        }
        let requests = &mut self.requests;
        let pending = &mut self.pending;
        self.queue.retain(|q| match q.slot {
            Slot::Thumb(id) if !keep.contains(&id) && q.priority <= 10 => {
                pending.remove(&q.slot);
                requests.remove(&q.slot);
                false
            }
            _ => true,
        });
        let thumbs = self.thumb_textures();
        if thumbs <= max {
            return;
        }
        self.textures.retain(|s, _| !matches!(s, Slot::Thumb(id) | Slot::ThumbQuick(id) if !keep.contains(id)));
        self.quick_tried.retain(|s, _| !matches!(s, Slot::ThumbQuick(id) if !keep.contains(id)));
    }
}

/// Include the source identity in diagnostics for every catalog-backed render slot (#1890).
/// Debug-format paths so whitespace/control characters cannot forge another log entry.
fn render_failure_message(slot: Slot, photo: PhotoId, source: Option<&lcc_catalog::Source>, error: &str) -> String {
    match source {
        Some(lcc_catalog::Source::File { path }) => format!("render {slot:?}, {photo:?}, file {path:?}: {error}"),
        Some(lcc_catalog::Source::Demo { scene }) => format!("render {slot:?}, {photo:?}, demo scene {scene}: {error}"),
        None => format!("render {slot:?}, {photo:?}, source unavailable: {error}"),
    }
}

#[cfg(test)]
mod thumbnail_tests {
    use super::*;
    use lcc_catalog::{Catalog, Op, Photo, Source};

    #[test]
    fn render_failures_identify_the_file_for_thumbnail_and_view_slots() {
        let source = Source::File { path: "/photos/trip/IMG_0042.nef".into() };
        for slot in [Slot::Thumb(PhotoId(42)), Slot::Main, Slot::Before, Slot::Compare(1), Slot::Window(0)] {
            let message = render_failure_message(slot, PhotoId(42), Some(&source), "unsupported compression");
            assert!(message.contains("/photos/trip/IMG_0042.nef"), "{message}");
            assert!(message.contains("PhotoId(42)"), "{message}");
            assert!(message.ends_with(": unsupported compression"), "{message}");
        }
    }

    #[test]
    fn render_failure_paths_are_escaped_and_missing_sources_still_identified() {
        let source = Source::File { path: "line\nbreak.nef".into() };
        let message = render_failure_message(Slot::Main, PhotoId(42), Some(&source), "decode failed");
        assert!(!message.contains('\n'));
        assert!(message.contains("line\\nbreak.nef"));
        assert!(render_failure_message(Slot::Main, PhotoId(42), None, "decode failed").contains("PhotoId(42), source unavailable"));
        let demo = Source::Demo { scene: 7 };
        assert!(render_failure_message(Slot::Main, PhotoId(42), Some(&demo), "decode failed").contains("demo scene 7"));
    }

    struct Blocked;
    impl RenderOffload for Blocked {
        fn try_start(&mut self, _: Slot, job: RenderJob) -> Option<RenderJob> {
            Some(job)
        }
        fn finished(&mut self) -> Vec<(Slot, RenderResult, f64)> {
            vec![]
        }
    }
    fn photo() -> Arc<Photo> {
        Arc::new(Photo::new(PhotoId(1), Source::File { path: "audit.jpg".into() }, "audit.jpg", "JPEG", 6000, 4000, "2026-01-01"))
    }
    #[test]
    fn snapshot_tracks_cow_edits_undo_reload_and_replacement() {
        let mut catalog = Catalog::default();
        catalog.apply(Op::AddPhoto { photo: Box::new((*photo()).clone()) }).unwrap();
        let old = catalog.photo(PhotoId(1)).unwrap().clone();
        let mut r = Renderer::default();
        r.remember_thumb(&old, 384, 77, false);
        r.pending.insert(Slot::Thumb(PhotoId(1)), (77, 10));
        assert!(r.thumb_current(&old, 384, 10));
        assert!(!r.thumb_current(&old, 512, 10));
        let mut settings = lcc_develop::DevelopSettings::default();
        settings.light.exposure = 1.0;
        let undo = catalog.apply(Op::SetDevelop { id: PhotoId(1), settings: Arc::new(settings), label: "edit".into(), edited: None }).unwrap();
        assert!(!r.thumb_current(catalog.photo(PhotoId(1)).unwrap(), 384, 10));
        catalog.apply(undo).unwrap();
        assert!(!r.thumb_current(catalog.photo(PhotoId(1)).unwrap(), 384, 10));
        catalog
            .apply(Op::SetContent {
                id: PhotoId(1),
                width: 6000,
                height: 4000,
                file_size: 99,
                content_hash: Some("changed".into()),
                preview_only: None,
                preview_checked: String::new(),
            })
            .unwrap();
        assert!(!r.thumb_current(catalog.photo(PhotoId(1)).unwrap(), 384, 10));
        assert!(!r.thumb_current(&photo(), 384, 10));
        r.forget_all();
        assert!(r.thumb_inputs.is_empty());
    }
    #[test]
    fn queue_promotions_running_work_and_cancellation() {
        let p = photo();
        let slot = Slot::Thumb(p.id);
        let mut session = Session::new();
        session.catalog.apply(Op::AddPhoto { photo: Box::new((*p).clone()) }).unwrap();
        let job = session.thumb_job(p.id, 384).unwrap();
        let key = job.key;
        let mut r = Renderer::default();
        r.set_offload(Box::new(Blocked));
        r.remember_thumb(&p, 384, key, false);
        r.request(slot, job, 5);
        assert!(r.thumb_current(&p, 384, 5));
        assert!(!r.thumb_current(&p, 384, 10));
        r.request(slot, session.thumb_job(p.id, 384).unwrap(), 10);
        assert_eq!(r.pending.get(&slot), Some(&(key, 10)));
        r.queue.clear(); // running work: cannot reprioritize, just as Renderer::request
        assert!(r.thumb_current(&p, 384, 5));
        r.pending.clear();
        assert!(!r.thumb_current(&p, 384, 10));
        r.evict_thumbs(&Default::default(), 600);
        assert!(r.thumb_inputs.is_empty());
    }
    fn queued_quick_thumbnail(id: PhotoId) -> (Arc<Photo>, QuickJob) {
        let mut session = Session::new();
        session.media.preview_loader = Some(Arc::new(|_, _| Some(lcc_raster::Rgba8 { width: 4, height: 4, data: vec![[200, 100, 50, 255]; 16] })));
        let mut raw = (*photo()).clone();
        raw.id = id;
        raw.kind = lcc_catalog::MediaKind::Raw;
        raw.develop = Arc::new(raw.camera_defaults());
        session.catalog.apply(Op::AddPhoto { photo: Box::new(raw) }).unwrap();
        let p = session.catalog.photo(id).unwrap().clone();
        let job = session.thumb_job(id, 384).unwrap();
        (p, session.quick_thumb_job(&job).unwrap())
    }

    #[test]
    fn queued_standin_promotes_from_prefetch_to_visible_without_restarting() {
        let (p, quick) = queued_quick_thumbnail(PhotoId(1));
        let (_, other) = queued_quick_thumbnail(PhotoId(2));
        let slot = Slot::ThumbQuick(p.id);
        // Deterministic queue: no native worker starts.
        let mut r = Renderer { pool: JobPool::new(0), ..Default::default() };
        r.remember_thumb(&p, 384, quick.key, true);
        r.request_quick(slot, quick.clone(), 6); // grid prefetch: 5 + 1
        r.request_quick(Slot::ThumbQuick(PhotoId(2)), other, 9); // filmstrip: 8 + 1
        let request_id = r.requests[&slot].id;
        assert!(r.thumb_current(&p, 384, 5));
        assert!(!r.thumb_current(&p, 384, 10));
        r.request_quick(slot, quick.clone(), 11); // visible grid: 10 + 1
        assert!(r.thumb_current(&p, 384, 10));
        r.request_quick(slot, quick.clone(), 11);
        assert_eq!(r.requests[&slot].id, request_id);
        assert_eq!(r.pool.queued(), 2);
        r.pool.run_inline(1);
        let done = r.pool.try_recv().unwrap();
        assert_eq!(done.slot, slot);
        assert_eq!(done.result.result.request_id, request_id);
        assert_eq!(done.result.result.quick, Some(QuickSource::Embedded));
        // A finished result awaiting collection also must not start another extraction.
        r.request_quick(slot, quick, 6);
        assert!(r.thumb_current(&p, 384, 5));
        assert_eq!(r.pool.queued(), 1);
    }

    #[test]
    fn queued_standin_demotes_when_it_leaves_the_visible_grid() {
        let (p, quick) = queued_quick_thumbnail(PhotoId(1));
        let (_, other) = queued_quick_thumbnail(PhotoId(2));
        let slot = Slot::ThumbQuick(p.id);
        let mut r = Renderer { pool: JobPool::new(0), ..Default::default() };
        r.remember_thumb(&p, 384, quick.key, true);
        r.request_quick(slot, quick.clone(), 11);
        r.request_quick(Slot::ThumbQuick(PhotoId(2)), other, 9);
        assert!(!r.thumb_current(&p, 384, 5));
        r.request_quick(slot, quick, 6);
        assert!(r.thumb_current(&p, 384, 5));
        r.pool.run_inline(1);
        assert_eq!(r.pool.try_recv().unwrap().slot, Slot::ThumbQuick(PhotoId(2)));
    }

    #[test]
    fn failures_retry_after_revision_and_quick_only_is_not_final() {
        let p = photo();
        let slot = Slot::Thumb(p.id);
        let mut r = Renderer::default();
        r.remember_thumb(&p, 384, 77, false);
        r.pending.insert(Slot::ThumbQuick(p.id), (77, 11));
        assert!(!r.thumb_current(&p, 384, 10));
        r.failed.insert(slot, (77, "no decoder".into(), 0, p.id));
        assert!(r.thumb_current(&p, 384, 10));
        assert_eq!(r.failure_of(slot, p.id), Some("no decoder"));
        assert_eq!(r.failure_of(slot, PhotoId(p.id.0 + 1)), None, "another photo's error");
        r.catalog_rev = 1;
        assert!(!r.thumb_current(&p, 384, 10));
    }

    #[test]
    fn a_fast_draft_is_drawn_in_the_frame_that_asked_for_it() {
        let mut s = Session::new();
        s.media.file_loader = Some(Arc::new(|_, _, _| Ok((lcc_raster::Rgb32f::new(8, 8), Default::default()))));
        s.catalog.apply(Op::AddPhoto { photo: Box::new((*photo()).clone()) }).unwrap();
        let mut r = Renderer::with_worker_threads(1);
        let ctx = egui::Context::default();
        let budget = std::time::Duration::from_secs(10);
        // the last loupe render took longer than the budget (a CPU render): not waited for
        r.last_main_ms = 1e6;
        r.request(Slot::Main, s.render_job(PhotoId(1), 64, 64, false, true).unwrap(), 100);
        r.wait_for_draft(&ctx, &mut s, budget);
        assert!(r.is_pending(Slot::Main) && !r.textures.contains_key(&Slot::Main));
        // a fast one (GPU drafts): the texture is the new render when the wait returns
        r.last_main_ms = 1.0;
        r.request(Slot::Main, s.render_job(PhotoId(1), 32, 32, false, true).unwrap(), 100);
        let want = r.wanted(Slot::Main);
        r.wait_for_draft(&ctx, &mut s, budget);
        assert!(!r.is_pending(Slot::Main));
        assert_eq!(r.textures.get(&Slot::Main).map(|t| t.key), want);
    }

    #[derive(Default)]
    struct Work {
        jobs: Vec<(Slot, RenderJob)>,
        finished: Vec<(Slot, RenderResult, f64)>,
    }
    struct Manual(std::rc::Rc<std::cell::RefCell<Work>>);
    impl RenderOffload for Manual {
        fn try_start(&mut self, slot: Slot, job: RenderJob) -> Option<RenderJob> {
            self.0.borrow_mut().jobs.push((slot, job));
            None
        }
        fn finished(&mut self) -> Vec<(Slot, RenderResult, f64)> {
            std::mem::take(&mut self.0.borrow_mut().finished)
        }
    }
    fn fixture() -> (crate::LccApp, std::rc::Rc<std::cell::RefCell<Work>>, egui::Context) {
        fixture_with(Session::new(), (*photo()).clone())
    }
    fn fixture_with(mut s: Session, photo: Photo) -> (crate::LccApp, std::rc::Rc<std::cell::RefCell<Work>>, egui::Context) {
        s.media.file_loader = Some(Arc::new(|_, _, _| {
            let mut image = lcc_raster::Rgb32f::new(8, 8);
            image.data.fill([0.3, 0.2, 0.1]);
            Ok((image, Default::default()))
        }));
        // (`with_fs` sets the file system's thumbnail loader, which wins over `file_loader`: thumbnails use the stub too)
        s.media.reduced_loader = None;
        s.catalog.apply(Op::AddPhoto { photo: Box::new(photo) }).unwrap();
        let mut app = crate::LccApp::new(s, crate::Services::default());
        app.renderer.keep_pixels = true;
        let work = std::rc::Rc::new(std::cell::RefCell::new(Work::default()));
        app.renderer.set_offload(Box::new(Manual(work.clone())));
        let ctx = egui::Context::default();
        app.renderer.poll(&ctx, &mut app.session);
        (app, work, ctx)
    }
    fn take_job(work: &std::rc::Rc<std::cell::RefCell<Work>>) -> (Slot, RenderJob) {
        work.borrow_mut().jobs.pop().unwrap()
    }
    fn finish(app: &mut crate::LccApp, work: &std::rc::Rc<std::cell::RefCell<Work>>, ctx: &egui::Context, slot: Slot, result: RenderResult) {
        work.borrow_mut().finished.push((slot, result, 0.0));
        app.renderer.poll(ctx, &mut app.session);
    }
    fn request(app: &mut crate::LccApp) {
        crate::panels::grid::request_thumb(app, PhotoId(1), 256, 10);
    }

    fn relink_destination() -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("lc-render-relinked-{}-{}.nef", std::process::id(), lcc_preview::next_tick()));
        std::fs::write(&path, b"replacement source").unwrap();
        path
    }

    #[test]
    fn completed_failure_keeps_requested_source_after_relink() {
        let replacement = relink_destination();
        for slot in [Slot::Thumb(PhotoId(1)), Slot::Main, Slot::Before, Slot::Compare(1), Slot::Window(0)] {
            let (mut app, work, ctx) = fixture();
            app.session.media.file_loader = Some(Arc::new(|_, _, _| Err("simulated decode failure".into())));
            let job = app.session.render_job(PhotoId(1), 64, 64, false, true).unwrap();
            app.renderer.request(slot, job, 100);
            let (submitted, job) = take_job(&work);
            assert_eq!(submitted, slot);
            let result = job.run();
            assert!(result.rendered.is_err(), "the old request has already failed");
            app.session.execute("photo.relink", &serde_json::json!({"id": 1, "path": replacement})).unwrap();
            assert!(
                matches!(&app.session.catalog.photo(PhotoId(1)).unwrap().source, Source::File { path } if path.ends_with(replacement.file_name().unwrap().to_str().unwrap()))
            );
            finish(&mut app, &work, &ctx, slot, result);
            assert_eq!(app.renderer.failure(slot), Some("simulated decode failure"));
            assert!(!app.renderer.is_pending(slot));
            let message = app.renderer.failure_messages.last().expect("the collector emitted a diagnostic");
            assert!(message.contains("audit.jpg"), "{message}");
            assert!(!message.contains("lc-render-relinked-"), "the failed request did not read the replacement: {message}");
            assert!(message.ends_with(": simulated decode failure"), "{message}");
        }
        std::fs::remove_file(replacement).unwrap();
    }

    #[test]
    fn superseded_failure_does_not_take_the_new_requests_source() {
        let replacement = relink_destination();
        let (mut app, work, ctx) = fixture();
        app.session.media.file_loader = Some(Arc::new(|_, _, _| Err("simulated decode failure".into())));
        let slot = Slot::Main;
        let old_job = app.session.render_job(PhotoId(1), 64, 64, false, true).unwrap();
        app.renderer.request(slot, old_job, 100);
        let (_, old_job) = take_job(&work);
        let old_result = old_job.run();
        app.session.execute("photo.relink", &serde_json::json!({"id": 1, "path": replacement})).unwrap();
        let new_job = app.session.render_job(PhotoId(1), 64, 64, false, true).unwrap();
        app.renderer.request(slot, new_job, 100);
        let (_, new_job) = take_job(&work);
        finish(&mut app, &work, &ctx, slot, old_result);
        assert!(app.renderer.failure_messages.is_empty(), "obsolete failures stay suppressed");
        assert!(app.renderer.is_pending(slot));
        finish(&mut app, &work, &ctx, slot, new_job.run());
        let message = app.renderer.failure_messages.last().expect("new failure is reported");
        assert!(message.contains("lc-render-relinked-"), "{message}");
        assert!(!message.contains("audit.jpg"), "{message}");
        assert!(app.renderer.requests.is_empty(), "completed request identity is released");
        std::fs::remove_file(replacement).unwrap();
    }

    #[test]
    fn grid_queued_standin_changes_priority_without_restarting() {
        let mut s = Session::new();
        s.media.preview_loader = Some(Arc::new(|_, _| Some(lcc_raster::Rgba8 { width: 4, height: 4, data: vec![[200, 100, 50, 255]; 16] })));
        let mut raw = (*photo()).clone();
        raw.kind = lcc_catalog::MediaKind::Raw;
        raw.develop = Arc::new(raw.camera_defaults());
        let (mut app, work, ctx) = fixture_with(s, raw);
        app.renderer.pool = JobPool::new(0); // deterministic: no quick job starts until run_inline
        let id = PhotoId(1);
        let slot = Slot::ThumbQuick(id);

        crate::panels::grid::request_thumb(&mut app, id, 384, 5);
        let request_id = app.renderer.requests[&slot].id;
        assert_eq!(app.renderer.pending[&slot].1, 6);
        assert_eq!(app.renderer.pool.queued(), 1);
        crate::panels::grid::request_thumb(&mut app, id, 384, 10);
        assert_eq!(app.renderer.pending[&slot].1, 11, "the grid must promote its queued stand-in");
        assert_eq!(app.renderer.requests[&slot].id, request_id);
        assert_eq!(app.renderer.pool.queued(), 1);

        crate::panels::grid::request_thumb(&mut app, id, 384, 5);
        assert_eq!(app.renderer.pending[&slot].1, 6, "the grid must demote its queued stand-in");
        assert_eq!(app.renderer.requests[&slot].id, request_id);
        assert_eq!(app.renderer.pool.queued(), 1);
        let built = app.renderer.thumb_jobs_built;
        app.renderer.pool.run_inline(1);
        crate::panels::grid::request_thumb(&mut app, id, 384, 10);
        assert_eq!(app.renderer.pending[&slot].1, 6, "completed work must not be restarted");
        assert_eq!(app.renderer.requests[&slot].id, request_id);
        assert_eq!(app.renderer.pool.queued(), 0);
        app.renderer.poll(&ctx, &mut app.session);
        assert_eq!(app.renderer.textures[&slot].quick, Some(QuickSource::Embedded));

        crate::panels::grid::request_thumb(&mut app, id, 384, 10);
        crate::panels::grid::request_thumb(&mut app, id, 384, 5);
        assert_eq!(app.renderer.thumb_jobs_built, built);
        assert_eq!(app.renderer.pool.queued(), 0);
        assert!(!app.renderer.is_pending(Slot::Thumb(id)));
        assert!(!app.renderer.textures.contains_key(&Slot::Thumb(id)));
        assert!(work.borrow().jobs.is_empty(), "the final embedded thumbnail needs no real render");
    }

    #[test]
    fn unchanged_ready_and_running_frames_build_no_jobs() {
        let (mut app, work, ctx) = fixture();
        request(&mut app);
        assert_eq!(app.renderer.thumb_jobs_built, 1);
        for _ in 0..180 {
            request(&mut app);
        }
        assert_eq!(app.renderer.thumb_jobs_built, 1);
        let (slot, job) = take_job(&work);
        finish(&mut app, &work, &ctx, slot, job.run());
        for _ in 0..180 {
            request(&mut app);
        }
        assert_eq!(app.renderer.thumb_jobs_built, 1);
        assert!(work.borrow().jobs.is_empty());
    }

    /// Two views showing one photo at different sizes in the same frames (the slideshow preview and
    /// the navigator) share the larger thumbnail instead of replacing each other's render forever.
    #[test]
    fn views_asking_for_two_sizes_of_one_photo_share_the_larger_thumbnail() {
        let (mut app, work, ctx) = fixture();
        for _ in 0..60 {
            crate::panels::grid::request_thumb(&mut app, PhotoId(1), 512, 9);
            crate::panels::grid::request_thumb(&mut app, PhotoId(1), 256, 8);
        }
        assert_eq!(app.renderer.thumb_jobs_built, 1, "one render, not one per frame");
        let (slot, job) = take_job(&work);
        finish(&mut app, &work, &ctx, slot, job.run());
        assert!(!app.renderer.is_pending(Slot::Thumb(PhotoId(1))), "the render was kept");
        for _ in 0..60 {
            crate::panels::grid::request_thumb(&mut app, PhotoId(1), 256, 8);
            crate::panels::grid::request_thumb(&mut app, PhotoId(1), 512, 9);
        }
        assert_eq!(app.renderer.thumb_jobs_built, 1);
        assert!(work.borrow().jobs.is_empty());
    }

    #[test]
    fn a_slider_drag_leaves_the_edited_photos_thumbnail_until_it_ends() {
        let (mut app, work, ctx) = fixture();
        request(&mut app);
        let (slot, job) = take_job(&work);
        finish(&mut app, &work, &ctx, slot, job.run());
        app.session.execute("library.select", &serde_json::json!({"ids": [1], "active": 1})).unwrap();
        app.session.execute("develop.beginInteraction", &serde_json::json!({"label": "Exposure"})).unwrap();
        for v in [0.1, 0.2, 0.3] {
            app.session.execute("develop.set", &serde_json::json!({"control": "light.exposure", "value": v})).unwrap();
            app.renderer.poll(&ctx, &mut app.session);
            request(&mut app);
        }
        assert_eq!(app.renderer.thumb_jobs_built, 1, "the drag's frames render no thumbnail of the photo");
        assert!(work.borrow().jobs.is_empty());
        app.session.execute("develop.endInteraction", &serde_json::json!({})).unwrap();
        request(&mut app);
        assert_eq!(app.renderer.thumb_jobs_built, 2, "the thumbnail follows once the drag ends");
    }

    #[test]
    fn unchanged_queued_frames_build_no_jobs() {
        let (mut app, _, _) = fixture();
        app.renderer.set_offload(Box::new(Blocked));
        request(&mut app);
        for _ in 0..180 {
            request(&mut app);
        }
        assert_eq!(app.renderer.thumb_jobs_built, 1);
        assert_eq!(app.renderer.queue.len(), 1);
    }

    #[test]
    fn resizing_the_disk_cache_keeps_textures_and_skips_rebuilds() {
        let lib = std::env::temp_dir().join(format!("lc-ui-thumb-resize-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&lib);
        let mut s = Session::new().with_fs();
        s.open_library(&lib, false).unwrap();
        let (mut app, work, ctx) = fixture_with(s, (*photo()).clone());
        request(&mut app);
        let (slot, job) = take_job(&work);
        finish(&mut app, &work, &ctx, slot, job.run());
        assert!(app.renderer.thumb(PhotoId(1)).is_some());
        let cache = app.session.media.rendered.clone();
        app.session.execute("library.preferences", &serde_json::json!({"cacheMb": 300})).unwrap();
        app.renderer.poll(&ctx, &mut app.session);
        // thumbnails are keyed by content: a new budget leaves them (and the cache) valid
        assert!(app.renderer.thumb(PhotoId(1)).is_some());
        assert!(Arc::ptr_eq(&cache, &app.session.media.rendered));
        for _ in 0..30 {
            request(&mut app);
        }
        assert_eq!(app.renderer.thumb_jobs_built, 1);
        assert!(work.borrow().jobs.is_empty());
        // an explicit clear still drops them
        app.session.execute("library.clearPreviews", &serde_json::json!({})).unwrap();
        app.renderer.poll(&ctx, &mut app.session);
        assert!(app.renderer.thumb(PhotoId(1)).is_none());
        drop(app);
        let _ = std::fs::remove_dir_all(&lib);
    }

    /// A render that panics (a decoder bug) is a failure like any other: the slot stops waiting,
    /// the error is kept (not retried every frame), and the pool's workers live on. Before, the
    /// worker died without an answer and the slot stayed pending forever.
    #[test]
    fn a_panicking_render_is_a_failure_not_a_dead_worker() {
        let (mut app, _, ctx) = fixture_with(Session::new(), (*photo()).clone());
        app.session.media.file_loader = Some(Arc::new(|_, _, _| panic!("decoder bug")));
        app.renderer.offload = None; // the real job pool
        let slot = Slot::Thumb(PhotoId(1));
        let ask = |app: &mut crate::LccApp| {
            if let Some(job) = app.session.thumb_job(PhotoId(1), 256) {
                app.renderer.request(slot, job, 30);
            }
            app.renderer.poll(&ctx, &mut app.session);
        };
        ask(&mut app);
        let t0 = std::time::Instant::now();
        while app.renderer.is_pending(slot) {
            assert!(t0.elapsed() < std::time::Duration::from_secs(20), "the render never answered");
            std::thread::sleep(std::time::Duration::from_millis(5));
            app.renderer.poll(&ctx, &mut app.session);
        }
        assert_eq!(app.renderer.failure(slot), Some("the render failed unexpectedly: decoder bug"));
        let workers = app.renderer.live_workers();
        assert!(workers > 0, "the worker that panicked is still there");
        let done = app.renderer.completed;
        for _ in 0..30 {
            ask(&mut app);
        }
        assert!(!app.renderer.is_pending(slot) && app.renderer.completed == done, "no retry loop");
        assert_eq!(app.renderer.live_workers(), workers);
    }

    #[test]
    fn raw_quick_path_frames_build_no_jobs() {
        // an unedited raw: the grid shows its embedded preview (held here until released) and stops
        // there — the camera JPEG at 4 416 × 2 944 already covers a 512 px thumbnail, so decoding the
        // whole raw behind it only costs the file read (45 s a photo on a 1.2 MB/s share)
        let gate = Arc::new(std::sync::Mutex::new(()));
        let held = gate.lock().unwrap();
        let mut s = Session::new();
        let g = gate.clone();
        s.media.preview_loader = Some(Arc::new(move |_: &str, _| {
            drop(g.lock().unwrap_or_else(std::sync::PoisonError::into_inner));
            Some(lcc_raster::Rgba8 { width: 4, height: 4, data: vec![[200, 100, 50, 255]; 16] })
        }));
        let mut raw = (*photo()).clone();
        raw.kind = lcc_catalog::MediaKind::Raw;
        raw.develop = Arc::new(raw.camera_defaults());
        let (mut app, _, ctx) = fixture_with(s, raw);
        app.renderer.set_offload(Box::new(Blocked));
        let quick = Slot::ThumbQuick(PhotoId(1));
        // the stand-in is running
        for _ in 0..180 {
            request(&mut app);
        }
        assert!(app.renderer.is_pending(quick));
        assert_eq!(app.renderer.thumb_jobs_built, 1);
        drop(held);
        let t0 = std::time::Instant::now();
        while !app.renderer.textures.contains_key(&quick) {
            assert!(t0.elapsed() < std::time::Duration::from_secs(60), "the embedded preview never arrived");
            std::thread::sleep(std::time::Duration::from_millis(5));
            app.renderer.poll(&ctx, &mut app.session);
        }
        // the embedded preview is the thumbnail: no real render is queued behind it, and the grid
        // asking every frame neither rebuilds the job nor re-requests the preview
        for _ in 0..180 {
            request(&mut app);
        }
        assert_eq!(app.renderer.thumb_jobs_built, 1, "an unedited raw's embedded preview is the thumbnail");
        assert!(app.renderer.queue.is_empty(), "nothing is decoded behind it");
        assert!(!app.renderer.is_pending(Slot::Thumb(PhotoId(1))));

        // an edit takes the embedded preview out of the picture: the real thumbnail is rendered then
        let mut edited = (*photo()).clone();
        edited.kind = lcc_catalog::MediaKind::Raw;
        edited.develop = Arc::new({
            let mut d = edited.camera_defaults();
            d.light.exposure = 0.5;
            d
        });
        let (mut app, _, _ctx) = fixture_with(Session::new(), edited);
        app.renderer.set_offload(Box::new(Blocked));
        for _ in 0..180 {
            request(&mut app);
        }
        assert_eq!(app.renderer.thumb_jobs_built, 1);
        assert!(app.renderer.is_pending(Slot::Thumb(PhotoId(1))) || !app.renderer.queue.is_empty(), "an edited raw renders its thumbnail");
    }

    #[test]
    fn closing_import_review_cancels_queued_and_running_identities() {
        let (mut app, work, ctx) = fixture();
        let job = app.session.thumb_job(PhotoId(1), 256).unwrap();
        app.renderer.request(Slot::Import(0), job.clone(), 10);
        let (slot, running) = take_job(&work);
        app.renderer.set_offload(Box::new(Blocked));
        app.renderer.request(Slot::Import(1), job, 10);
        app.renderer.forget_imports();
        assert!(!app.renderer.is_pending(Slot::Import(0)));
        assert!(!app.renderer.is_pending(Slot::Import(1)));
        assert!(app.renderer.queue.is_empty());
        work.borrow_mut().finished.push((slot, running.run(), 0.0));
        app.renderer.set_offload(Box::new(Manual(work.clone())));
        app.renderer.poll(&ctx, &mut app.session);
        assert!(!app.renderer.textures.contains_key(&slot));
        assert_eq!(app.session.media.source_usage().0, 0);
    }

    #[test]
    fn loupe_shows_superseded_drafts_newer_than_its_pixels_during_a_drag() {
        for draft in [true, false] {
            let (mut app, work, ctx) = fixture();
            let mut jobs = Vec::new();
            for exposure in [0.0, 1.0, 2.0] {
                let mut edit = lcc_develop::DevelopSettings::default();
                edit.light.exposure = exposure;
                app.session.catalog.apply(Op::SetDevelop { id: PhotoId(1), settings: Arc::new(edit), label: "drag".into(), edited: None }).unwrap();
                let job = app.session.loupe_job(PhotoId(1), 8, 8, true).unwrap();
                let job = if draft { job.draft() } else { job };
                app.renderer.request(Slot::Main, job, 100);
                jobs.push(take_job(&work).1);
            }
            let keys: Vec<u64> = jobs.iter().map(|j| j.key).collect();
            let mut jobs = jobs.into_iter();
            let (first, second, last) = (jobs.next().unwrap(), jobs.next().unwrap(), jobs.next().unwrap());
            // the first draft lands while the newest is still rendering: the loupe moves on
            finish(&mut app, &work, &ctx, Slot::Main, first.run());
            assert_eq!(app.renderer.textures.get(&Slot::Main).map(|t| t.key), Some(keys[0]));
            assert!(app.renderer.is_pending(Slot::Main));
            finish(&mut app, &work, &ctx, Slot::Main, last.run());
            assert_eq!(app.renderer.textures.get(&Slot::Main).map(|t| t.key), Some(keys[2]));
            // a draft older than the pixels shown never replaces them
            finish(&mut app, &work, &ctx, Slot::Main, second.run());
            assert_eq!(app.renderer.textures.get(&Slot::Main).map(|t| t.key), Some(keys[2]));
            assert!(!app.renderer.is_pending(Slot::Main));
        }
    }

    #[test]
    fn returning_to_displayed_hdr_settings_supersedes_pending_changes() {
        struct Output;
        impl crate::HdrOutput for Output {
            fn headroom(&mut self) -> Option<f32> {
                Some(4.0)
            }
            fn upload(&mut self, _: u8, _: usize, _: usize, _: &[f32]) -> Result<egui::TextureId, String> {
                Ok(egui::TextureId::User(1))
            }
        }
        let (mut app, work, _) = fixture();
        app.hdr_headroom = Some(4.0);
        app.services.hdr_output = Some(Box::new(Output));
        let mut job = app.session.render_job(PhotoId(1), 8, 8, false, true).unwrap();
        Arc::make_mut(&mut job.settings).hdr.enabled = true;
        let view = HDR_LOUPE;
        let key = crate::panels::detail::request_hdr(&mut app, view, job.clone(), true, 100).unwrap();
        take_job(&work);
        app.renderer.hdr_view.insert(view, HdrView { photo: PhotoId(1), key, tex: egui::TextureId::User(1), size: [8, 8] });
        crate::panels::detail::request_hdr(&mut app, view, job.clone().draft(), true, 100);
        take_job(&work);
        crate::panels::detail::request_hdr(&mut app, view, job, true, 100);
        assert_eq!(app.renderer.wanted(Slot::HdrView(view)), Some(key));
    }

    #[test]
    fn a_failed_hdr_render_takes_the_older_frame_down() {
        let (mut app, work, ctx) = fixture();
        app.session.media.file_loader = Some(Arc::new(|_, _, _| Err("simulated decode failure".into())));
        let (view, slot) = (HDR_LOUPE, Slot::HdrView(HDR_LOUPE));
        let job = crate::panels::detail::hdr_view_job(app.session.render_job(PhotoId(1), 8, 8, false, true).unwrap(), Some(4.0));
        app.renderer.prepare_hdr_view(view, &job);
        app.renderer.hdr_view.insert(view, HdrView { photo: PhotoId(1), key: job.key ^ 1, tex: egui::TextureId::User(1), size: [8, 8] });
        app.renderer.request(slot, job.clone(), 100);
        assert!(app.renderer.hdr_frame(view, PhotoId(1), job.key).is_some(), "the older frame stays up while its update renders");
        let (_, pending) = take_job(&work);
        finish(&mut app, &work, &ctx, slot, pending.run());
        assert!(app.renderer.hdr_frame(view, PhotoId(1), job.key).is_none(), "a failed update leaves no stale frame up");
        assert!(app.renderer.hdr_view.is_empty());
    }

    #[test]
    fn a_zoomed_hdr_pane_shows_hdr_until_its_window_comes_and_its_tiles_if_that_fails() {
        use crate::panels::detail::{HdrPane, WindowCtx, WindowView, hdr_window};
        struct Output;
        impl crate::HdrOutput for Output {
            fn headroom(&mut self) -> Option<f32> {
                Some(4.0)
            }
            fn upload(&mut self, _: u8, _: usize, _: usize, _: &[f32]) -> Result<egui::TextureId, String> {
                Ok(egui::TextureId::User(1))
            }
        }
        let (mut app, work, ctx) = fixture();
        app.hdr_headroom = Some(4.0);
        app.services.hdr_output = Some(Box::new(Output));
        let mut edit = (*app.session.catalog.photo(PhotoId(1)).unwrap().develop).clone();
        edit.hdr.enabled = true;
        app.session.catalog.apply(Op::SetDevelop { id: PhotoId(1), settings: Arc::new(edit), label: "hdr".into(), edited: None }).unwrap();
        app.session.media.file_loader = Some(Arc::new(|_, _, _| Err("simulated decode failure".into())));
        let c = WindowCtx {
            id: PhotoId(1),
            frame_edge: Some(8),
            aspect: 1.0,
            texture_side: 4096,
            crop_tool: false,
            interacting: false,
            look: 0,
            holding: false,
            overlay: lcc_pipeline::Overlay::None,
            proof: None,
            full_preview: None,
        };
        let r = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(8.0, 8.0));
        let v = WindowView { pane: PANE_AFTER, before: false, img: r, target: r, visible: r };
        // no whole frame in HDR under it yet: its tiles
        assert!(matches!(hdr_window(&mut app, &c, &v, (8, 8)), HdrPane::Sdr));
        app.renderer.hdr_view.insert(HDR_LOUPE, HdrView { photo: PhotoId(1), key: 1, tex: egui::TextureId::User(2), size: [8, 8] });
        assert!(matches!(hdr_window(&mut app, &c, &v, (8, 8)), HdrPane::Pending), "the whole frame's HDR shows meanwhile");
        let (slot, job) = work.borrow_mut().jobs.drain(..).find(|(s, _)| matches!(s, Slot::HdrView(_))).expect("the HDR window job");
        finish(&mut app, &work, &ctx, slot, job.run());
        assert!(matches!(hdr_window(&mut app, &c, &v, (8, 8)), HdrPane::Sdr), "a failed HDR window: the tiles");
    }

    #[test]
    fn an_older_hdr_frame_is_not_shown_for_a_render_nothing_runs() {
        let (mut app, _work, _) = fixture();
        let job = crate::panels::detail::hdr_view_job(app.session.render_job(PhotoId(1), 8, 8, false, true).unwrap(), Some(4.0));
        let view = HDR_SECOND;
        app.renderer.hdr_view.insert(view, HdrView { photo: PhotoId(1), key: job.key ^ 1, tex: egui::TextureId::User(1), size: [8, 8] });
        assert!(app.renderer.hdr_frame(view, PhotoId(1), job.key).is_none(), "no update on its way: the frame is stale");
        app.renderer.request(Slot::HdrView(view), job.clone(), 100);
        assert!(app.renderer.hdr_frame(view, PhotoId(1), job.key).is_some());
        assert!(app.renderer.hdr_frame(view, PhotoId(2), job.key).is_none(), "another photo's frame never stands in");
        app.renderer.forget_all();
        assert!(app.renderer.hdr_frame(view, PhotoId(1), job.key).is_none());
    }

    #[test]
    fn a_crop_or_rotation_drops_the_held_hdr_frame_and_a_tone_change_keeps_it() {
        let (mut app, _work, _) = fixture();
        let view = HDR_LOUPE;
        let job = crate::panels::detail::hdr_view_job(app.session.render_job(PhotoId(1), 8, 8, false, true).unwrap(), Some(4.0));
        let held = HdrView { photo: PhotoId(1), key: job.key, tex: egui::TextureId::User(1), size: [8, 8] };
        let changes: [(&str, fn(&mut lcc_develop::DevelopSettings), bool); 4] = [
            ("exposure", |s| s.light.exposure += 1.0, true),
            ("crop aspect", |s| s.crop.aspect = Some((1, 1)), false),
            ("crop flip", |s| s.crop.flip_h = !s.crop.flip_h, false),
            ("rotation", |s| s.orientation = lcc_geom::Orientation::Rotate90, false),
        ];
        for (what, change, kept) in changes {
            app.renderer.prepare_hdr_view(view, &job);
            app.renderer.hdr_view.insert(view, held);
            let mut next = job.clone();
            change(Arc::make_mut(&mut next.settings));
            app.renderer.prepare_hdr_view(view, &next);
            assert_eq!(app.renderer.hdr_view.contains_key(&view), kept, "{what}");
        }
    }

    #[test]
    fn renders_with_float_samples_keep_their_texture_pixels_unless_they_are_hdr_views() {
        let (mut app, work, ctx) = fixture();
        let mut job = app.session.render_job(PhotoId(1), 8, 8, false, true).unwrap();
        Arc::make_mut(&mut job.settings).hdr.enabled = true;
        let deep = crate::panels::detail::hdr_view_job(job, Some(4.0));
        assert!(deep.clone().run().rendered.unwrap().deep.is_some(), "the job carries float samples");
        for slot in [Slot::Main, Slot::Before, Slot::Second, Slot::HdrHistogram, Slot::Compare(0)] {
            assert!(Renderer::run_for(slot, || deep.clone().run(), &None).color.is_some(), "{slot:?}");
        }
        assert!(Renderer::run_for(Slot::HdrView(HDR_LOUPE), || deep.clone().run(), &None).color.is_none(), "uploaded from its floats");
        // and through the renderer: a view's texture still arrives
        app.renderer.request(Slot::Second, deep, 100);
        let (slot, job) = take_job(&work);
        finish(&mut app, &work, &ctx, slot, job.run());
        assert!(app.renderer.textures.contains_key(&Slot::Second));
    }

    #[test]
    fn hdr_retention_tracks_region_and_rejects_obsolete_geometry() {
        let (mut app, work, ctx) = fixture();
        let view = HDR_ZOOMED;
        let slot = Slot::HdrView(view);
        let job = app.session.render_job(PhotoId(1), 8, 8, false, true).unwrap();
        let mut job = crate::panels::detail::hdr_view_job(job, Some(4.0));
        job.request.window = Some(lcc_pipeline::PixelWindow { x: 0, y: 0, w: 4, h: 4 });
        app.renderer.prepare_hdr_view(view, &job);
        app.renderer.request(slot, job.clone(), 100);
        let (_, old) = take_job(&work);
        app.renderer.hdr_view.insert(view, HdrView { photo: PhotoId(1), key: job.key, tex: egui::TextureId::User(1), size: [4, 4] });
        app.renderer.prepare_hdr_view(view, &job.clone().draft());
        assert!(app.renderer.hdr_view.contains_key(&view), "tone changes retain the region");
        job.request.window.as_mut().unwrap().x = 4;
        job.key ^= 42;
        app.renderer.prepare_hdr_view(view, &job);
        assert!(!app.renderer.hdr_view.contains_key(&view), "panning invalidates coverage");
        app.renderer.request(slot, job, 100);
        let (_, new) = take_job(&work);
        finish(&mut app, &work, &ctx, slot, old.run());
        assert!(app.renderer.hdr_view_result.is_empty(), "old geometry is rejected even before new pixels arrive");
        finish(&mut app, &work, &ctx, slot, new.run());
        assert!(app.renderer.hdr_view_result.contains_key(&view));
    }

    #[test]
    fn hdr_drafts_progress_and_reject_out_of_order_results() {
        let (mut app, work, ctx) = fixture();
        let slot = Slot::HdrView(HDR_LOUPE);
        let mut jobs = Vec::new();
        for exposure in [3.0, 4.0, 5.0] {
            let mut edit = lcc_develop::DevelopSettings::default();
            edit.hdr.enabled = true;
            edit.light.exposure = exposure;
            app.session.catalog.apply(Op::SetDevelop { id: PhotoId(1), settings: Arc::new(edit), label: "drag".into(), edited: None }).unwrap();
            let job = app.session.render_job(PhotoId(1), 8, 8, false, true).unwrap().draft();
            let job = crate::panels::detail::hdr_view_job(job, Some(4.0));
            app.renderer.prepare_hdr_view(HDR_LOUPE, &job);
            app.renderer.request(slot, job, 100);
            jobs.push(take_job(&work).1);
        }
        let keys: Vec<_> = jobs.iter().map(|j| j.key).collect();
        let mut jobs = jobs.into_iter();
        let (first, second, last) = (jobs.next().unwrap(), jobs.next().unwrap(), jobs.next().unwrap());
        finish(&mut app, &work, &ctx, slot, first.run());
        assert_eq!(app.renderer.hdr_view_result[&HDR_LOUPE].1, keys[0]);
        assert!(app.renderer.is_pending(slot), "updates continue while dragging");
        finish(&mut app, &work, &ctx, slot, last.run());
        finish(&mut app, &work, &ctx, slot, second.run());
        let (_, key, deep) = &app.renderer.hdr_view_result[&HDR_LOUPE];
        assert_eq!(*key, keys[2], "late older frames never replace newer pixels");
        let lcc_pipeline::DeepSamples::F32(rgb) = &deep.samples else { panic!("float HDR required") };
        assert!(rgb.iter().any(|v| *v > 1.0), "highlights survive the draft pipeline");
        let final_job = app.session.render_job(PhotoId(1), 8, 8, false, true).unwrap();
        let final_render = crate::panels::detail::hdr_view_job(final_job, Some(4.0)).run().rendered.unwrap();
        let lcc_pipeline::DeepSamples::F32(final_rgb) = &final_render.deep.unwrap().samples else { panic!("float HDR required") };
        assert_eq!(rgb.len(), final_rgb.len());
        assert!(rgb.iter().zip(final_rgb).all(|(a, b)| (a - b).abs() < 1e-4), "uniform tonal draft matches the final HDR render");
        app.renderer.forget_all();
        assert!(app.renderer.hdr_view_result.is_empty());
    }

    #[test]
    fn a_drag_gets_its_live_hdr_histogram_beside_the_drafts() {
        let (mut app, work, ctx) = fixture();
        let mut jobs = Vec::new();
        for exposure in [1.0, 2.0] {
            let mut edit = lcc_develop::DevelopSettings::default();
            edit.hdr.enabled = true;
            edit.light.exposure = exposure;
            app.session.catalog.apply(Op::SetDevelop { id: PhotoId(1), settings: Arc::new(edit), label: "drag".into(), edited: None }).unwrap();
            let job = app.session.render_job(PhotoId(1), 32, 32, false, true).unwrap();
            app.renderer.request(Slot::HdrHistogram, job, 95);
            jobs.push(take_job(&work).1);
        }
        let keys: Vec<u64> = jobs.iter().map(|j| j.key).collect();
        let mut jobs = jobs.into_iter();
        let (first, last) = (jobs.next().unwrap(), jobs.next().unwrap());
        // superseded while the drag goes on, it still lands: the histogram keeps moving
        finish(&mut app, &work, &ctx, Slot::HdrHistogram, first.run());
        let live = app.renderer.live_hdr_histogram.clone().expect("a live HDR histogram");
        assert_eq!((live.0, live.1), (PhotoId(1), keys[0]));
        assert!(live.2.hdr.is_some(), "HDR bins");
        finish(&mut app, &work, &ctx, Slot::HdrHistogram, last.run());
        assert_eq!(app.renderer.live_hdr_histogram.as_ref().map(|l| l.1), Some(keys[1]));
        // only the histogram is kept: no texture
        assert!(!app.renderer.textures.contains_key(&Slot::HdrHistogram));
    }

    #[test]
    fn library_reset_rejects_old_job_even_when_photo_id_and_key_are_reused() {
        let (mut app, work, ctx) = fixture();
        request(&mut app);
        let (slot, old) = take_job(&work);
        let old_result = old.run();
        app.renderer.forget_all();
        let photo = app.session.catalog.photo(PhotoId(1)).unwrap().as_ref().clone();
        app.session = lcc_engine::Session::new();
        app.session.catalog.apply(Op::AddPhoto { photo: Box::new(photo) }).unwrap();
        app.session.media.file_loader = Some(Arc::new(|_, _, _| {
            let mut image = lcc_raster::Rgb32f::new(8, 8);
            image.data.fill([0.1, 0.2, 0.9]);
            Ok((image, Default::default()))
        }));
        app.renderer.poll(&ctx, &mut app.session);
        request(&mut app);
        let (_, new) = take_job(&work);
        assert_eq!(old_result.key, new.key);
        assert_ne!(old_result.request_id, new.request_id);
        let expected = new.clone().run().rendered.unwrap().image.as_bytes();
        finish(&mut app, &work, &ctx, slot, new.run());
        finish(&mut app, &work, &ctx, slot, old_result);
        assert_eq!(
            app.renderer.thumb(PhotoId(1)).unwrap().pixels.as_ref().unwrap().pixels.iter().flat_map(|c| c.to_array()).collect::<Vec<_>>(),
            expected
        );
    }

    #[test]
    fn late_results_and_errors_cannot_replace_newest_pixels_or_sources() {
        for order in [0, 1, 2] {
            let (mut app, work, ctx) = fixture();
            request(&mut app);
            let (slot, old) = take_job(&work);
            let mut edit = lcc_develop::DevelopSettings::default();
            edit.light.exposure = 1.0;
            app.session.catalog.apply(Op::SetDevelop { id: PhotoId(1), settings: Arc::new(edit), label: "edit".into(), edited: None }).unwrap();
            request(&mut app);
            let (_, new) = take_job(&work);
            let expected = new.clone().run().rendered.unwrap().image.as_bytes();
            if order != 0 {
                finish(&mut app, &work, &ctx, slot, new.run());
                // An obsolete source and error must not poison the current successful request.
                let mut stale = old.run();
                if order == 2 {
                    stale.rendered = Err("old failure".into());
                }
                finish(&mut app, &work, &ctx, slot, stale);
            } else {
                finish(&mut app, &work, &ctx, slot, old.run());
                assert!(app.renderer.thumb(PhotoId(1)).is_none());
                assert_eq!(app.session.media.source_usage().0, 0);
                finish(&mut app, &work, &ctx, slot, new.run());
            }
            assert_eq!(
                app.renderer.thumb(PhotoId(1)).unwrap().pixels.as_ref().unwrap().pixels.iter().flat_map(|c| c.to_array()).collect::<Vec<_>>(),
                expected
            );
            assert!(app.renderer.failure(slot).is_none());
        }
    }

    #[test]
    fn undo_to_existing_texture_supersedes_pending_edit() {
        let (mut app, work, ctx) = fixture();
        request(&mut app);
        let (slot, first) = take_job(&work);
        let key = first.key;
        finish(&mut app, &work, &ctx, slot, first.run());
        let original = app.renderer.thumb(PhotoId(1)).unwrap().pixels.clone().unwrap();
        let mut edit = lcc_develop::DevelopSettings::default();
        edit.light.exposure = 1.0;
        let undo =
            app.session.catalog.apply(Op::SetDevelop { id: PhotoId(1), settings: Arc::new(edit), label: "edit".into(), edited: None }).unwrap();
        request(&mut app);
        let (_, edited) = take_job(&work);
        app.session.catalog.apply(undo).unwrap();
        request(&mut app);
        let (_, restored) = take_job(&work);
        assert_eq!(restored.key, key);
        finish(&mut app, &work, &ctx, slot, restored.run());
        finish(&mut app, &work, &ctx, slot, edited.run());
        assert_eq!(app.renderer.thumb(PhotoId(1)).unwrap().pixels.as_ref().unwrap().pixels, original.pixels);
    }

    #[test]
    fn explicit_clear_repairs_same_key_bad_pixels_and_rejects_old_same_key_job() {
        let (mut app, work, ctx) = fixture();
        let job = app.session.thumb_job(PhotoId(1), 256).unwrap();
        let expected = job.clone().run().rendered.unwrap().image;
        let (cache, key) = job.cache.clone().unwrap();
        let mut bad = expected.clone();
        bad.data.fill([255, 0, 255, 255]);
        cache.put(key, Arc::new(bad.clone()));
        request(&mut app);
        let (slot, corrupt) = take_job(&work);
        finish(&mut app, &work, &ctx, slot, corrupt.run());
        assert_eq!(app.renderer.thumb(PhotoId(1)).unwrap().pixels.as_ref().unwrap().pixels[0].to_array(), [255, 0, 255, 255]);
        // An old job can have the SAME render key and stale decoded pixels.
        let mut old = job.clone();
        old.source =
            lcc_engine::media::SourceRef::Loaded(Box::new(lcc_engine::media::DecodedSource::new(Arc::new(lcc_raster::Rgb32f::new(8, 8)), None)));
        app.renderer.textures.clear();
        app.renderer.request(slot, old, 10);
        let (_, old) = take_job(&work);
        app.session.execute("library.clearPreviews", &serde_json::json!({})).unwrap();
        app.renderer.poll(&ctx, &mut app.session);
        assert!(app.renderer.thumb(PhotoId(1)).is_none());
        request(&mut app);
        let (_, repaired) = take_job(&work);
        assert_eq!(repaired.key, job.key);
        finish(&mut app, &work, &ctx, slot, repaired.run());
        finish(&mut app, &work, &ctx, slot, old.run());
        assert_eq!(cache.get(key).unwrap().as_bytes(), expected.as_bytes());
        assert_eq!(
            app.renderer.thumb(PhotoId(1)).unwrap().pixels.as_ref().unwrap().pixels.iter().flat_map(|c| c.to_array()).collect::<Vec<_>>(),
            expected.as_bytes()
        );
    }

    #[test]
    fn undo_to_embedded_thumb_rejects_late_full_render() {
        let mut s = Session::new();
        s.media.preview_loader = Some(Arc::new(|_, _| Some(lcc_raster::Rgba8 { width: 4, height: 4, data: vec![[200, 100, 50, 255]; 16] })));
        let mut raw = (*photo()).clone();
        raw.kind = lcc_catalog::MediaKind::Raw;
        raw.develop = Arc::new(raw.camera_defaults());
        let (mut app, work, ctx) = fixture_with(s, raw);
        app.renderer.pool = JobPool::new(0);
        let id = PhotoId(1);
        let quick_slot = Slot::ThumbQuick(id);
        let full_slot = Slot::Thumb(id);
        let settings_a = app.session.catalog.photo(id).unwrap().develop.clone();

        crate::panels::grid::request_thumb(&mut app, id, 384, 10);
        assert_eq!(app.renderer.pool.run_inline(1), 1);
        app.renderer.poll(&ctx, &mut app.session);
        let (key_a, pixels_a) = {
            let shown = app.renderer.thumb(id).unwrap();
            assert_eq!(shown.quick, Some(QuickSource::Embedded));
            (shown.key, shown.pixels.as_ref().unwrap().pixels.clone())
        };
        assert!(app.renderer.textures.contains_key(&quick_slot));
        assert!(!app.renderer.textures.contains_key(&full_slot));
        assert!(work.borrow().jobs.is_empty());

        let mut settings_b = (*settings_a).clone();
        settings_b.light.exposure += 0.5;
        app.session.commit("Exposure", Op::SetDevelop { id, settings: Arc::new(settings_b), label: "Exposure".into(), edited: None }).unwrap();
        assert!(!app.session.embedded_thumb_is_final(app.session.catalog.photo(id).unwrap()));
        crate::panels::grid::request_thumb(&mut app, id, 384, 10);
        // Manual owns dispatched jobs: hold B until the real Undo command restores A.
        let (slot, held_b) = take_job(&work);
        assert_eq!(slot, full_slot);
        let key_b = held_b.key;
        assert_ne!(key_b, key_a);
        assert!(app.renderer.is_pending(full_slot));

        app.run("edit.undo", serde_json::json!({})).unwrap();
        assert_eq!(app.session.catalog.photo(id).unwrap().develop.as_ref(), settings_a.as_ref());
        assert!(app.session.embedded_thumb_is_final(app.session.catalog.photo(id).unwrap()));
        crate::panels::grid::request_thumb(&mut app, id, 384, 10);
        {
            let restored = app.renderer.thumb(id).unwrap();
            assert_eq!(restored.key, key_a);
            assert_eq!(restored.pixels.as_ref().unwrap().pixels, pixels_a);
            assert_eq!(restored.quick, Some(QuickSource::Embedded));
        }
        assert!(work.borrow().jobs.is_empty(), "restoring embedded-final A needs no full render");
        assert_eq!(app.renderer.pool.queued(), 0);

        let result_b = held_b.run();
        assert_eq!(result_b.key, key_b);
        let rendered_b = result_b.rendered.as_ref().expect("held edited thumbnail must render before the stale-result oracle");
        assert_ne!(color_image(&rendered_b.image).pixels, pixels_a, "the held result must be distinguishable from A");
        finish(&mut app, &work, &ctx, slot, result_b);

        let shown = app.renderer.thumb(id).unwrap();
        assert_eq!(shown.key, key_a, "late edited Thumb(B) must not replace restored embedded ThumbQuick(A)");
        assert_eq!(shown.pixels.as_ref().unwrap().pixels, pixels_a);
        assert_eq!(shown.quick, Some(QuickSource::Embedded));
        let quick = app.renderer.textures.get(&quick_slot).expect("the correct embedded thumbnail must remain installed");
        assert_eq!(quick.key, key_a);
        assert_eq!(quick.quick, Some(QuickSource::Embedded));
        assert_eq!(quick.pixels.as_ref().unwrap().pixels, pixels_a);
        assert!(!app.renderer.is_pending(full_slot));
    }
}

#[cfg(test)]
mod stage_budget_tests {
    use super::*;

    const MB: usize = 1 << 20;

    // Given stage caches within the budget, nothing is cleared
    #[test]
    fn nothing_is_trimmed_within_the_budget() {
        let sizes = [(Slot::Main, 300 * MB), (Slot::Window(PANE_AFTER), 100 * MB)];
        assert_eq!(stage_trim_order(&sizes, 512 * MB), vec![]);
    }

    // Given HDR views on an HDR display, their caches are reused by a drag like their 8-bit
    // views', but together hold no more than the budget on top of them: past it the loupe's,
    // which every tick reuses, is the one kept
    #[test]
    fn hdr_caches_stay_within_the_budget_and_keep_the_loupes() {
        use lcc_engine::pipeline::{OutputDepth, RenderRequest, SourceInfo, render_cached};
        let src = Arc::new(lcc_raster::Rgb32f::from_fn(64, 48, |x, y| [x as f32 / 16.0, y as f32 / 12.0, 2.0]));
        let mut settings = lcc_develop::DevelopSettings::default();
        settings.hdr.enabled = true;
        settings.effects.clarity = 20.0;
        let mut renderer = Renderer::default();
        let mut fill = |slot: Slot, depth: OutputDepth| {
            let cache = Arc::new(StageCache::default());
            render_cached(&src, &SourceInfo::default(), &settings, &RenderRequest { depth, ..RenderRequest::fit(64, 48) }, &cache);
            renderer.stages.insert(slot, cache);
        };
        fill(Slot::Main, OutputDepth::U8);
        let hdr = [Slot::HdrView(HDR_LOUPE), Slot::HdrView(HDR_ZOOMED + PANE_AFTER), Slot::HdrView(HDR_BEFORE), Slot::HdrView(HDR_COMPARE)];
        for slot in hdr {
            fill(slot, OutputDepth::F32Hdr);
        }
        let bytes = |r: &Renderer, slot: Slot| r.stages.get(&slot).map_or(0, |c| c.bytes());
        let hdr_bytes = |r: &Renderer| hdr.iter().map(|s| bytes(r, *s)).sum::<usize>();
        let (main, loupe, all) = (bytes(&renderer, Slot::Main), bytes(&renderer, Slot::HdrView(HDR_LOUPE)), hdr_bytes(&renderer));
        assert!(main > 0 && loupe > 0);
        // room for all of them: a drag keeps every stage
        renderer.stage_budget_override = Some(main + all);
        renderer.trim_stages();
        assert_eq!(hdr_bytes(&renderer), all);
        // room for the 8-bit views and one HDR view: the HDR caches shrink to the budget
        renderer.stage_budget_override = Some(main.max(loupe));
        for _ in 0..3 {
            renderer.trim_stages();
            assert!(hdr_bytes(&renderer) <= main.max(loupe), "HDR caches are bounded");
            assert_eq!(bytes(&renderer, Slot::HdrView(HDR_LOUPE)), loupe, "the loupe's HDR stages stay for the next tick");
            assert_eq!(bytes(&renderer, Slot::Main), main, "the 8-bit loupe's stages stay");
        }
    }

    // Given too much held, the caches nothing is looking at go first (a hover render's, a compare
    // tile's: rebuilt cheaply), the least useful first, and only as many as needed
    #[test]
    fn the_idle_caches_go_first_and_only_as_many_as_needed() {
        let sizes =
            [(Slot::Main, 300 * MB), (Slot::Hover, 100 * MB), (Slot::Second, 80 * MB), (Slot::Compare(0), 60 * MB), (Slot::Compare(1), 60 * MB)];
        // 600 held, 500 allowed: the hover render's 100 is enough
        assert_eq!(stage_trim_order(&sizes, 500 * MB), vec![Slot::Hover]);
        // 400 allowed: the compare tiles' too (380 left), then, at 350, the second window's
        assert_eq!(stage_trim_order(&sizes, 400 * MB), vec![Slot::Hover, Slot::Compare(0), Slot::Compare(1)]);
        assert_eq!(stage_trim_order(&sizes, 350 * MB), vec![Slot::Hover, Slot::Compare(0), Slot::Compare(1), Slot::Second]);
    }

    // Given the open photo's views over the budget, they are not trimmed: they are what a slider
    // drag reuses on every tick, and clearing them every frame costs far more than the memory
    // (issue #323 review: 3.6x slower drags on a 2800 px canvas). They are counted and reported,
    // and only a runaway (more than HARD_FACTOR times the budget) cuts the window and before
    // caches, never the whole-frame view's.
    #[test]
    fn the_open_photos_views_are_trimmed_only_in_a_runaway_and_never_the_whole_frame() {
        let views = [(Slot::Main, 400 * MB), (Slot::Window(PANE_AFTER), 500 * MB), (Slot::Window(PANE_BEFORE), 500 * MB), (Slot::Before, 100 * MB)];
        assert_eq!(stage_trim_order(&views, 400 * MB), vec![], "1.5 GB against a 400 MB budget is a busy Before/After, not a runaway");
        // against a 100 MB budget (HARD_FACTOR 4 = 400 MB) it is: the before side goes first, then the window
        assert_eq!(stage_trim_order(&views, 100 * MB), vec![Slot::Window(PANE_BEFORE), Slot::Before, Slot::Window(PANE_AFTER)]);
        let mut with_idle = views.to_vec();
        with_idle.push((Slot::Hover, 50 * MB));
        assert_eq!(stage_trim_order(&with_idle, 400 * MB), vec![Slot::Hover]);
        // a whole-frame view of any size stays
        assert_eq!(stage_trim_order(&[(Slot::Main, 9000 * MB)], MB), vec![]);
    }

    // Given caches of slots that hold nothing, they are never "trimmed"
    #[test]
    fn empty_caches_are_left_alone() {
        assert_eq!(stage_trim_order(&[(Slot::Hover, 0), (Slot::Second, 900 * MB)], 100 * MB), vec![Slot::Second]);
    }

    // Given real stage caches over the budget, the renderer clears them (the open photo's last)
    #[test]
    fn the_renderer_trims_real_stage_caches() {
        use lcc_engine::pipeline::{RenderRequest, SourceInfo, render_cached};
        let src = std::sync::Arc::new(lcc_raster::Rgb32f::from_fn(320, 240, |x, y| [x as f32 / 320.0, y as f32 / 240.0, 0.3]));
        let mut r = Renderer::default();
        for slot in [Slot::Main, Slot::Hover] {
            let cache: Arc<StageCache> = Default::default();
            let mut s = lcc_engine::develop::DevelopSettings::default();
            s.effects.clarity = 50.0;
            render_cached(&src, &SourceInfo::default(), &s, &RenderRequest::fit(320, 240), &cache);
            assert!(cache.bytes() > 0);
            r.stages.insert(slot, cache);
        }
        let total: usize = r.stages.values().map(|c| c.bytes()).sum();
        r.stage_budget_override = Some(total * 3 / 4);
        r.trim_stages();
        assert_eq!(r.stages[&Slot::Hover].bytes(), 0, "the hover render's cache went");
        assert!(r.stages[&Slot::Main].bytes() > 0, "the open photo's stayed");
        assert_eq!(r.stages_trimmed, 1);
        r.stage_budget_override = Some(0);
        r.trim_stages();
        assert!(r.stages[&Slot::Main].bytes() > 0, "the open photo's view is never trimmed");
        r.trim_stages();
        assert_eq!(r.stages_trimmed, 1, "and nothing is trimmed again");
    }

    // The renderer counts and trims what it holds, and says so
    #[test]
    fn the_renderer_reports_its_stage_budget() {
        let r = Renderer::default();
        let m = r.memory();
        assert!(m["stageCaches"]["budgetBytes"].as_u64().unwrap() > 0, "{m}");
        assert_eq!(m["stageCaches"]["trimmed"], 0);
        assert!(m["stageCaches"]["sharedSourceBytes"].is_u64(), "the one device copy of a big original is reported: {m}");
    }
}

#[cfg(test)]
mod color_image_tests {
    use super::*;

    // The texture a render becomes is what egui would make of its bytes, whatever the alpha
    #[test]
    fn the_fast_conversion_equals_eguis() {
        let img = lcc_raster::Rgba8::from_fn(37, 29, |x, y| {
            let a = match (x + y) % 5 {
                0 => 0,
                1 => 17,
                2 => 128,
                _ => 255,
            };
            [(x * 7) as u8, (y * 9) as u8, (x * y) as u8, a]
        });
        let want = egui::ColorImage::from_rgba_unmultiplied([img.width, img.height], &img.as_bytes());
        let got = color_image(&img);
        assert_eq!(got.size, want.size);
        assert_eq!(got.pixels, want.pixels);
    }

    #[test]
    fn an_empty_image_is_an_empty_texture() {
        let img = lcc_raster::Rgba8::new(0, 0);
        assert!(color_image(&img).pixels.is_empty());
    }
}

#[cfg(test)]
mod extended_linear_tests {
    use super::*;

    #[test]
    fn job_for_skips_display_profile_for_extended_linear_surface() {
        let mut r = Renderer::default();
        r.set_surface_extended_linear(true);

        let job = RenderJob {
            cache_generation: 0,
            request_id: 1,
            source_key: None,
            photo: lcc_catalog::PhotoId(1),
            level: lcc_engine::media::SourceLevel::Full,
            source: lcc_engine::media::SourceRef::File {
                path: "/tmp/test.jpg".to_string(),
                max_edge: 100,
                loader: None,
                fallback: None,
                denoise: None,
                options: Default::default(),
                reduced: None,
            },
            origin: lcc_catalog::Source::File { path: "/tmp/test.jpg".to_string() },
            info: Default::default(),
            display_fit: None,
            settings: Default::default(),
            request: lcc_pipeline::RenderRequest {
                max_w: 100,
                max_h: 100,
                quality: Default::default(),
                apply_crop: false,
                overlay: Default::default(),
                space: Default::default(),
                depth: Default::default(),
                proof: None,
                window: None,
                display: None,
            },
            key: 0,
            cache: None,
            view_cache: None,
            stages: None,
            display: None,
            slide: None,
            preview_cut: None,
        };

        let result = r.job_for(Slot::Main, job.clone());
        assert!(result.display.is_none(), "display profile should be None for extended linear surface");
    }
}

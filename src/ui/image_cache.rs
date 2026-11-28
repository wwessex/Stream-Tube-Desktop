//! Thumbnail/avatar memory: a bounded image cache.
//!
//! gpui keeps every image it has ever decoded unless an image cache is given, so
//! browsing for a while grows memory without limit. This cache keeps recently
//! shown images and frees the rest whenever the user moves to another page.
use gpui_kit::*;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::rc::Rc;
use futures::channel::oneshot;
use std::sync::{Arc, Mutex, OnceLock, mpsc};

/// Web images are decoded to at most this many pixels (854x480), keeping their
/// shape: cards get 1280x720 thumbnails but draw them about 350 px wide, so
/// decoding them whole took 3.7 MB each (and as much again on the GPU). Wide
/// channel banners still come out about 1570 px wide.
const MAX_PIXELS: u64 = 854 * 480;

type LoadResult = Result<Arc<RenderImage>, ImageCacheError>;

enum Own {
    Loading(HashSet<EntityId>),
    Done(LoadResult),
}

enum Slot {
    /// app assets (logo SVGs and such): gpui's own loader
    Gpui(ImageCacheItem),
    /// web images: fetched and downscaled here
    Web { state: Rc<RefCell<Own>>, _task: Task<()> },
}

impl Slot {
    fn get(&self) -> Option<LoadResult> {
        match self {
            Slot::Gpui(i) => i.get(),
            Slot::Web { state, .. } => match &*state.borrow() {
                Own::Done(r) => Some(r.clone()),
                Own::Loading(_) => None,
            },
        }
    }

    fn use_image(&self, window: &Window) -> Option<LoadResult> {
        match self {
            Slot::Gpui(i) => i.use_image(window),
            Slot::Web { state, .. } => match &mut *state.borrow_mut() {
                Own::Done(r) => Some(r.clone()),
                Own::Loading(views) => {
                    views.insert(window.current_view());
                    None
                }
            },
        }
    }

    fn new(resource: &Resource, cx: &mut App) -> Self {
        let url = match resource {
            Resource::Uri(u) if u.starts_with("http") => u.to_string(),
            _ => return Slot::Gpui(ImageCacheItem::new(resource, cx)),
        };
        let state = Rc::new(RefCell::new(Own::Loading(HashSet::default())));
        let weak = Rc::downgrade(&state);
        let task = cx.spawn(async move |cx| {
            let result = fetch_in_background(url).await.map_err(|e| ImageCacheError::Other(Arc::new(e)));
            let Some(state) = weak.upgrade() else { return };
            let prev = std::mem::replace(&mut *state.borrow_mut(), Own::Done(result));
            if let Own::Loading(views) = prev {
                cx.update(|cx| {
                    for v in views {
                        cx.notify(v);
                    }
                });
            }
        });
        Slot::Web { state, _task: task }
    }
}

type ImageRequest = (String, oneshot::Sender<anyhow::Result<Arc<RenderImage>>>);

/// Keep blocking HTTP off GPUI's workers and bound concurrent downloads.
async fn fetch_in_background(url: String) -> anyhow::Result<Arc<RenderImage>> {
    static QUEUE: OnceLock<mpsc::Sender<ImageRequest>> = OnceLock::new();
    let queue = QUEUE.get_or_init(|| {
        let (sender, receiver) = mpsc::channel::<ImageRequest>();
        let receiver = Arc::new(Mutex::new(receiver));
        for _ in 0..8 {
            let receiver = receiver.clone();
            std::thread::spawn(move || loop {
                let job = receiver.lock().unwrap().recv();
                let Ok((url, result)) = job else { break };
                if !result.is_canceled() {
                    let _ = result.send(fetch_decode(&url));
                }
            });
        }
        sender
    });
    let (send, receive) = oneshot::channel();
    queue.send((url, send)).map_err(|_| anyhow::anyhow!("image workers unavailable"))?;
    receive.await?
}

/// Downloads an image and decodes it (BGRA, like gpui), shrinking oversized ones.
fn fetch_decode(url: &str) -> anyhow::Result<Arc<RenderImage>> {
    let bytes = crate::api::http::get_bytes(url, &[])?;
    if let Some(frames) = animated(&bytes) {
        return Ok(Arc::new(RenderImage::new(frames)));
    }
    let img = image::load_from_memory(&bytes)?;
    let (w, h) = (img.width(), img.height());
    let mut rgba = if w as u64 * h as u64 > MAX_PIXELS {
        let k = (MAX_PIXELS as f64 / (w as f64 * h as f64)).sqrt() as f32;
        let (nw, nh) = (((w as f32 * k).round() as u32).max(1), ((h as f32 * k).round() as u32).max(1));
        image::imageops::resize(&img.into_rgba8(), nw, nh, image::imageops::FilterType::Triangle)
    } else {
        img.into_rgba8()
    };
    for px in rgba.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    Ok(Arc::new(RenderImage::new(smallvec::smallvec![image::Frame::new(rgba)])))
}

/// Animated images are kept this small (they're drawn at most 240px wide), cut
/// off after this many frames, and shrunk further to fit a memory budget, so one
/// GIF can't take much memory.
const ANIM_MAX: u32 = 240;
const ANIM_FRAMES: usize = 150;
const ANIM_BUDGET: f32 = 6.0 * 1024.0 * 1024.0;

/// Every frame of an animated GIF or WebP (BGRA, scaled down), or None for a still image.
fn animated(bytes: &[u8]) -> Option<smallvec::SmallVec<[image::Frame; 1]>> {
    use image::AnimationDecoder;
    use std::io::Cursor;
    let frames: Vec<image::Frame> = match image::guess_format(bytes).ok()? {
        image::ImageFormat::Gif => image::codecs::gif::GifDecoder::new(Cursor::new(bytes)).ok()?.into_frames().take(ANIM_FRAMES).collect::<Result<_, _>>().ok()?,
        image::ImageFormat::WebP => {
            let d = image::codecs::webp::WebPDecoder::new(Cursor::new(bytes)).ok()?;
            if !d.has_animation() {
                return None;
            }
            d.into_frames().take(ANIM_FRAMES).collect::<Result<_, _>>().ok()?
        }
        _ => return None,
    };
    if frames.len() < 2 {
        return None;
    }
    // long animations keep every other frame, each shown twice as long
    let step = if frames.len() > 40 { 2 } else { 1 };
    let count = frames.len().div_ceil(step);
    let (fw, fh) = frames[0].buffer().dimensions();
    let fit = (ANIM_BUDGET / (count as f32 * 4.0 * fw.max(1) as f32 * fh.max(1) as f32)).sqrt();
    let k = (ANIM_MAX as f32 / fw.max(1) as f32).min(ANIM_MAX as f32 / fh.max(1) as f32).min(fit).min(1.0);
    Some(
        frames
            .into_iter()
            .step_by(step)
            .map(|f| {
                // a frame time of 0-10 ms means "default" in GIFs; browsers show those for 100 ms
                let (n, d) = f.delay().numer_denom_ms();
                let (n, d) = if d == 0 || n as f32 / (d as f32) <= 10.0 { (100, 1) } else { (n, d) };
                let delay = image::Delay::from_numer_denom_ms(n * step as u32, d);
                let buf = f.into_buffer();
                let (w, h) = buf.dimensions();
                let mut buf = if k < 1.0 {
                    image::imageops::resize(&buf, ((w as f32 * k) as u32).max(1), ((h as f32 * k) as u32).max(1), image::imageops::FilterType::Triangle)
                } else {
                    buf
                };
                for px in buf.chunks_exact_mut(4) {
                    px.swap(0, 2);
                }
                image::Frame::from_parts(buf, 0, 0, delay)
            })
            .collect(),
    )
}

/// Decoded images above this are freed, least recently shown first.
const BUDGET_BYTES: usize = 48 * 1024 * 1024;

struct Entry {
    item: Slot,
    /// navigation generation in which it was last drawn
    used_gen: u64,
    /// render counter when it was last drawn (orders entries of the same generation)
    used_tick: u64,
    bytes: usize,
}

pub struct LruImageCache {
    items: HashMap<u64, Entry>,
    gen_: u64,
    tick: u64,
}

#[derive(Clone)]
pub struct Images(pub Entity<LruImageCache>);
impl Global for Images {}

pub fn init(cx: &mut App) -> Entity<LruImageCache> {
    let e = cx.new(|_| LruImageCache { items: HashMap::new(), gen_: 0, tick: 0 });
    cx.set_global(Images(e.clone()));
    // STREAMTUBE_IMAGE_STATS=<file>: write what the cache holds every few seconds (diagnostics)
    if let Some(path) = std::env::var_os("STREAMTUBE_IMAGE_STATS") {
        let e2 = e.clone();
        cx.spawn(async move |cx| loop {
            cx.background_executor().timer(std::time::Duration::from_secs(4)).await;
            let line = cx.update(|cx| {
                let c = e2.read(cx);
                let mut sizes: Vec<(usize, String)> = c
                    .items
                    .values()
                    .filter_map(|e| e.item.get().and_then(|r| r.ok()).map(|i| (size_of(&i), format!("{:?}", i.size(0)))))
                    .collect();
                sizes.sort_by(|a, b| b.0.cmp(&a.0));
                let total: usize = sizes.iter().map(|s| s.0).sum();
                format!("images={} decoded={}MB largest={:?}
", sizes.len(), total / 1048576, sizes.iter().take(5).map(|s| &s.1).collect::<Vec<_>>())
            });
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
                let _ = f.write_all(line.as_bytes());
            }
        })
        .detach();
    }
    e
}

fn key(r: &Resource) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    r.hash(&mut h);
    h.finish()
}

fn size_of(img: &Arc<RenderImage>) -> usize {
    let s = img.size(0);
    (s.width.0.max(0) as usize) * (s.height.0.max(0) as usize) * 4 * img.frame_count().max(1)
}

impl ImageCache for LruImageCache {
    fn load(&mut self, resource: &Resource, window: &mut Window, cx: &mut App) -> Option<Result<Arc<RenderImage>, ImageCacheError>> {
        self.tick += 1;
        let (g, t) = (self.gen_, self.tick);
        let e = self.items.entry(key(resource)).or_insert_with(|| Entry { item: Slot::new(resource, cx), used_gen: g, used_tick: t, bytes: 0 });
        e.used_gen = g;
        e.used_tick = t;
        let r = e.item.use_image(window);
        if e.bytes == 0 {
            if let Some(Ok(img)) = &r {
                e.bytes = size_of(img);
            }
        }
        r
    }
}

impl LruImageCache {
    fn total(&self) -> usize {
        self.items.values().map(|e| e.bytes).sum()
    }

    /// Frees images not drawn on the current page until the cache is within budget.
    fn trim(&mut self, cx: &mut App) -> bool {
        let mut total = self.total();
        if total <= BUDGET_BYTES {
            return false;
        }
        let gen_ = self.gen_;
        let mut old: Vec<(u64, u64, u64)> = self.items.iter().filter(|(_, e)| e.used_gen < gen_).map(|(k, e)| (e.used_gen, e.used_tick, *k)).collect();
        old.sort_unstable();
        let mut freed = false;
        for (_, _, k) in old {
            if total <= BUDGET_BYTES * 3 / 4 {
                break;
            }
            if let Some(e) = self.items.remove(&k) {
                total = total.saturating_sub(e.bytes);
                if let Some(Ok(img)) = e.item.get() {
                    cx.drop_image(img, None);
                }
                freed = true;
            }
        }
        freed
    }
}

/// Called when the user navigates: images drawn from now on belong to the new page.
/// A moment later (once the new page has drawn), images over budget are freed and
/// the pages kept for Back/Forward are told to redraw, so they reload what they need.
pub fn on_navigate(cx: &mut App) {
    let Some(images) = cx.try_global::<Images>().map(|i| i.0.clone()) else { return };
    images.update(cx, |c, _| c.gen_ += 1);
    cx.spawn(async move |cx| {
        cx.background_executor().timer(std::time::Duration::from_millis(1500)).await;
        let _ = cx.update(|cx| {
            let freed = images.update(cx, |c, cx| c.trim(cx));
            if freed {
                crate::ui::app::refresh_all_pages(cx);
            }
        });
    })
    .detach();
}

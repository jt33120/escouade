//! Screen context while dictating: frames of the main display, and the dev server's URL.

use super::logic::select_frames;
use parking_lot::Mutex;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const INTERVAL: Duration = Duration::from_millis(700);
const MAX_FRAMES_KEPT: usize = 150;
const MAX_SENT: usize = 6;
const CHANGE_THRESHOLD: f32 = 8.0;
const MAX_WIDTH: u32 = 1280;

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
}

/// Whether the app may record the screen, without asking.
pub fn has_access() -> bool {
    // SAFETY: plain CoreGraphics query without arguments.
    unsafe { CGPreflightScreenCaptureAccess() }
}

/// Whether the app may record the screen; asks macOS (once per call) when it may not.
pub fn ensure_access() -> bool {
    // SAFETY: plain CoreGraphics queries without arguments.
    unsafe { CGPreflightScreenCaptureAccess() || CGRequestScreenCaptureAccess() }
}

pub struct Frame {
    pub t_ms: u64,
    thumb: Vec<u8>,
    pub jpeg: Vec<u8>,
}

fn capture_once(n: u64) -> Option<(Vec<u8>, Vec<u8>)> {
    let path = std::env::temp_dir().join(format!("escouade-voice-{}-{n}.jpg", std::process::id()));
    let ok = Command::new("/usr/sbin/screencapture")
        .args(["-x", "-C", "-m", "-t", "jpg"])
        .arg(&path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    let bytes = std::fs::read(&path).ok();
    let _ = std::fs::remove_file(&path);
    if !ok {
        return None;
    }
    process(&bytes?)
}

/// (thumbnail 32x32 gray, JPEG at most 1280 px wide)
fn process(jpeg: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    use image::imageops::FilterType;
    let img = image::load_from_memory_with_format(jpeg, image::ImageFormat::Jpeg).ok()?;
    let thumb = img.resize_exact(32, 32, FilterType::Triangle).to_luma8().into_raw();
    let small = if img.width() > MAX_WIDTH { img.resize(MAX_WIDTH, u32::MAX, FilterType::Triangle) } else { img };
    let mut out = Vec::new();
    let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 75);
    let rgb = image::DynamicImage::ImageRgb8(small.to_rgb8());
    rgb.write_with_encoder(enc).ok()?;
    Some((thumb, out))
}

/// The URL of the front browser's active tab when it is a local dev server.
pub fn local_url() -> Option<String> {
    const SCRIPT: &str = r#"
tell application "System Events" to set appName to name of first application process whose frontmost is true
if appName is "Google Chrome" then
  tell application "Google Chrome" to return URL of active tab of front window
else if appName is "Arc" then
  tell application "Arc" to return URL of active tab of front window
else if appName is "Safari" then
  tell application "Safari" to return URL of current tab of front window
end if
return """#;
    let mut child = Command::new("/usr/bin/osascript")
        .args(["-e", SCRIPT])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if start.elapsed() < Duration::from_secs(3) => std::thread::sleep(Duration::from_millis(30)),
            _ => {
                let _ = child.kill();
                return None;
            }
        }
    }
    let mut out = String::new();
    std::io::Read::read_to_string(&mut child.stdout.take()?, &mut out).ok()?;
    let url = out.trim();
    (url.starts_with("http://localhost") || url.starts_with("http://127.0.0.1")).then(|| url.to_string())
}

/// Records frames in the background until `finish`.
pub struct Recorder {
    frames: Arc<Mutex<Vec<Frame>>>,
    stop: Arc<AtomicBool>,
    started: Instant,
    thread: Option<std::thread::JoinHandle<()>>,
    active: bool,
}

pub struct Context {
    /// JPEG bytes of the frames kept, in order.
    pub images: Vec<Vec<u8>>,
    pub url: Option<String>,
}

impl Recorder {
    /// `on_denied` is called once if the screen cannot be recorded; dictation goes on without frames.
    pub fn start(on_denied: impl FnOnce() + Send + 'static) -> Self {
        let frames = Arc::new(Mutex::new(Vec::<Frame>::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let started = Instant::now();
        let (f, s) = (frames.clone(), stop.clone());
        let thread = std::thread::Builder::new()
            .name("voice-frames".into())
            .spawn(move || {
                if !ensure_access() {
                    on_denied();
                    return;
                }
                let mut n = 0;
                while !s.load(Ordering::Acquire) {
                    let tick = Instant::now();
                    let t_ms = started.elapsed().as_millis() as u64;
                    if let Some((thumb, jpeg)) = capture_once(n) {
                        let mut g = f.lock();
                        if g.len() >= MAX_FRAMES_KEPT {
                            g.remove(0);
                        }
                        g.push(Frame { t_ms, thumb, jpeg });
                    }
                    n += 1;
                    while tick.elapsed() < INTERVAL && !s.load(Ordering::Acquire) {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                }
            })
            .ok();
        Self { frames, stop, started, thread, active: true }
    }

    pub fn started(&self) -> Instant {
        self.started
    }

    /// Stops recording and picks the frames to send. `deictic`: ms (since start) of pointing words.
    pub fn finish(mut self, deictic: &[u64]) -> Context {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        self.active = false;
        let mut frames = std::mem::take(&mut *self.frames.lock());
        // The last frame is always sent: the state of the screen when the user finished.
        if !frames.is_empty() {
            let t_ms = self.started.elapsed().as_millis() as u64;
            if let Some((thumb, jpeg)) = capture_once(u64::MAX) {
                frames.push(Frame { t_ms, thumb, jpeg });
            }
        }
        let url = local_url();
        let thumbs: Vec<Vec<u8>> = frames.iter().map(|f| f.thumb.clone()).collect();
        let times: Vec<u64> = frames.iter().map(|f| f.t_ms).collect();
        let picked = select_frames(&thumbs, &times, deictic, MAX_SENT, CHANGE_THRESHOLD);
        let images = picked.into_iter().map(|i| std::mem::take(&mut frames[i].jpeg)).collect();
        Context { images, url }
    }

    /// Stops without keeping anything.
    pub fn cancel(mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        self.active = false;
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        if self.active {
            self.stop.store(true, Ordering::Release);
        }
    }
}

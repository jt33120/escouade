//! Voice mode (macOS): push-to-talk and hands-free dictation with local Whisper, screen
//! context, and spoken feedback. See docs/VOICE-SPEC.md.

pub mod audio;
pub mod logic;
pub mod screen;
pub mod stt;

use crate::model::Settings;
use audio::Capture;
use base64::Engine;
use logic::{is_deictic, is_hallucination, speakable, Action, HandsFree, Phase, Vad, VadEvent};
use parking_lot::Mutex;
use screen::Recorder;
use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};
use stt::{Segment, Stt};
use tauri::{AppHandle, Emitter, Wry};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};

#[derive(Serialize, Clone)]
pub struct VoiceFrame {
    pub name: String,
    #[serde(rename = "mediaType")]
    pub media_type: String,
    pub data: String,
}

/// What the UI hears from the voice backend (event "voice").
#[derive(Serialize, Clone)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum VoiceEvent {
    State { state: &'static str },
    Append { text: String },
    Clear,
    Finish { text: String, context: Option<String>, frames: Vec<VoiceFrame>, send: bool },
    Model { status: &'static str, progress: f64 },
    Notice { message: String },
    Error { message: String },
}

#[derive(Clone, Default)]
struct VoiceSettings {
    enabled: bool,
    shortcut: String,
    language: String,
    hands_free: bool,
    speak: bool,
    proxy: String,
}

struct Ptt {
    _capture: Capture,
    buf: Arc<Mutex<Vec<f32>>>,
    recorder: Option<Recorder>,
}

struct HandsFreeSession {
    _capture: Capture,
    stop: Arc<AtomicBool>,
}

pub struct Voice {
    app: AppHandle<Wry>,
    stt: Stt,
    settings: Mutex<VoiceSettings>,
    ptt: Mutex<Option<Ptt>>,
    hf: Mutex<Option<HandsFreeSession>>,
    speaking: AtomicBool,
    muted_until: Mutex<Instant>,
    downloading: AtomicBool,
    denied_notified: AtomicBool,
    state: Mutex<&'static str>,
}

/// The line that tells the agent what the attached images are.
pub fn context_line(frames: usize, url: Option<&str>) -> Option<String> {
    let mut parts = Vec::new();
    match frames {
        0 => {}
        1 => parts.push(
            "[Contexte : 1 capture de l'écran prise pendant que je parlais ; le curseur indique où je pointais.]".to_string(),
        ),
        n => parts.push(format!(
            "[Contexte : {n} captures de l'écran prises pendant que je parlais, dans l'ordre ; le curseur indique où je pointais.]"
        )),
    }
    if let Some(u) = url {
        parts.push(format!("URL : {u}"));
    }
    (!parts.is_empty()).then(|| parts.join("\n"))
}

fn play_sound(name: &str) {
    let _ = std::process::Command::new("/usr/bin/afplay")
        .arg(format!("/System/Library/Sounds/{name}.aiff"))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

fn join_text(segs: &[Segment]) -> String {
    segs.iter().map(|s| s.text.as_str()).collect::<Vec<_>>().join(" ").trim().to_string()
}

impl Voice {
    pub fn new(app: AppHandle<Wry>) -> Arc<Self> {
        Arc::new(Self {
            app,
            stt: Stt::default(),
            settings: Mutex::new(VoiceSettings::default()),
            ptt: Mutex::new(None),
            hf: Mutex::new(None),
            speaking: AtomicBool::new(false),
            muted_until: Mutex::new(Instant::now()),
            downloading: AtomicBool::new(false),
            denied_notified: AtomicBool::new(false),
            state: Mutex::new("off"),
        })
    }

    fn emit(&self, ev: VoiceEvent) {
        let _ = self.app.emit("voice", ev);
    }

    fn error(&self, message: impl Into<String>) {
        let message = message.into();
        log::warn!("voice: {message}");
        self.emit(VoiceEvent::Error { message });
    }

    fn set_state(&self, s: &'static str) {
        *self.state.lock() = s;
        self.emit(VoiceEvent::State { state: s });
    }

    pub fn state(&self) -> &'static str {
        *self.state.lock()
    }

    fn rest_state(&self) -> &'static str {
        if !self.settings.lock().enabled {
            "off"
        } else if self.hf.lock().is_some() {
            "listening"
        } else {
            "idle"
        }
    }

    /// The microphone is ignored while the app speaks (it would transcribe itself).
    pub fn is_muted(&self) -> bool {
        self.speaking.load(Ordering::Acquire) || Instant::now() < *self.muted_until.lock()
    }

    pub fn model_ready(&self) -> bool {
        stt::model_ready()
    }

    pub fn downloading(&self) -> bool {
        self.downloading.load(Ordering::Acquire)
    }

    /// Applies the voice settings: shortcut, hands-free listening.
    pub fn apply_settings(self: &Arc<Self>, s: &Settings) {
        let new = VoiceSettings {
            enabled: s.voice_enabled,
            shortcut: s.voice_shortcut.trim().to_string(),
            language: s.voice_language.trim().to_string(),
            hands_free: s.voice_hands_free,
            speak: s.voice_speak,
            proxy: s.proxy_url.clone(),
        };
        let old = std::mem::replace(&mut *self.settings.lock(), new.clone());
        if old.enabled != new.enabled || old.shortcut != new.shortcut {
            self.register_shortcut();
        }
        let want_hf = new.enabled && new.hands_free;
        let running = self.hf.lock().is_some();
        if running && !want_hf {
            self.stop_hands_free();
        } else if !running && want_hf {
            self.start_hands_free();
        }
        if new.enabled && stt::model_ready() {
            self.preload();
        }
        if self.hf.lock().is_none() && self.ptt.lock().is_none() {
            self.set_state(self.rest_state());
        }
    }

    /// Loads the model in the background so the first dictation is not slow.
    fn preload(self: &Arc<Self>) {
        let v = self.clone();
        std::thread::spawn(move || {
            if let Err(e) = v.stt.context() {
                v.error(format!("{e:#}"));
            }
        });
    }

    fn register_shortcut(self: &Arc<Self>) {
        let gs = self.app.global_shortcut();
        let _ = gs.unregister_all();
        let (enabled, shortcut) = {
            let s = self.settings.lock();
            (s.enabled, s.shortcut.clone())
        };
        if !enabled || shortcut.is_empty() {
            return;
        }
        let v = self.clone();
        let res = gs.on_shortcut(shortcut.as_str(), move |_app, _sc, ev| match ev.state() {
            ShortcutState::Pressed => v.press(),
            ShortcutState::Released => v.release(),
        });
        if let Err(e) = res {
            self.error(format!("Raccourci vocal « {shortcut} » indisponible : {e}"));
        }
    }

    // ----- Push-to-talk -----

    fn press(self: &Arc<Self>) {
        if !self.settings.lock().enabled || self.hf.lock().is_some() {
            return;
        }
        let mut slot = self.ptt.lock();
        if slot.is_some() {
            return;
        }
        if !stt::model_ready() {
            self.error("Modèle vocal absent : télécharge-le dans Réglages → Voix.");
            return;
        }
        let buf = Arc::new(Mutex::new(Vec::<f32>::new()));
        let (b, v) = (buf.clone(), self.clone());
        let capture = Capture::start(move |chunk| {
            let mut g = b.lock();
            // One minute at most.
            if !v.is_muted() && g.len() < 16_000 * 60 {
                g.extend_from_slice(chunk);
            }
        });
        let capture = match capture {
            Ok(c) => c,
            Err(e) => {
                drop(slot);
                self.error(format!("{e:#}"));
                return;
            }
        };
        let v = self.clone();
        let recorder = Recorder::start(move || v.screen_denied());
        *slot = Some(Ptt { _capture: capture, buf, recorder: Some(recorder) });
        drop(slot);
        self.set_state("recording");
        self.preload();
    }

    fn screen_denied(&self) {
        if !self.denied_notified.swap(true, Ordering::AcqRel) {
            self.error(
                "Enregistrement de l'écran non autorisé : autorise Escouade dans Réglages Système → Confidentialité et sécurité → Enregistrement de l'écran (puis relance l'app).",
            );
        }
    }

    fn release(self: &Arc<Self>) {
        let Some(mut session) = self.ptt.lock().take() else { return };
        let samples = std::mem::take(&mut *session.buf.lock());
        let recorder = session.recorder.take();
        drop(session); // stops the capture
        if samples.len() < 16_000 * 3 / 10 {
            if let Some(r) = recorder {
                r.cancel();
            }
            self.set_state(self.rest_state());
            return;
        }
        self.set_state("transcribing");
        let v = self.clone();
        std::thread::spawn(move || {
            let t = Instant::now();
            let lang = v.settings.lock().language.clone();
            let result = v.stt.transcribe(&samples, &lang);
            let segs: Vec<Segment> = match result {
                Ok(s) => s.into_iter().filter(|s| !is_hallucination(&s.text)).collect(),
                Err(e) => {
                    if let Some(r) = recorder {
                        r.cancel();
                    }
                    v.error(format!("{e:#}"));
                    v.set_state(v.rest_state());
                    return;
                }
            };
            log::info!("voice: transcribed {} s in {} ms", samples.len() / 16_000, t.elapsed().as_millis());
            let text = join_text(&segs);
            if text.is_empty() {
                if let Some(r) = recorder {
                    r.cancel();
                }
                v.emit(VoiceEvent::Notice { message: "Aucune parole détectée.".into() });
            } else {
                let points: Vec<u64> =
                    segs.iter().filter(|s| is_deictic(&s.text)).map(|s| (s.t0_ms + s.t1_ms) / 2).collect();
                v.finish(text, recorder, &points, false);
            }
            v.set_state(v.rest_state());
        });
    }

    /// Hands the text, the chosen frames and their context line to the UI.
    fn finish(&self, text: String, recorder: Option<Recorder>, points: &[u64], send: bool) {
        let ctx = recorder.map(|r| r.finish(points));
        let (frames, url) = match ctx {
            Some(c) => (c.images, c.url),
            None => (Vec::new(), None),
        };
        let context = context_line(frames.len(), url.as_deref());
        let frames = frames
            .into_iter()
            .enumerate()
            .map(|(i, jpeg)| VoiceFrame {
                name: format!("capture-{}.jpg", i + 1),
                media_type: "image/jpeg".into(),
                data: base64::engine::general_purpose::STANDARD.encode(jpeg),
            })
            .collect();
        self.emit(VoiceEvent::Finish { text, context, frames, send });
    }

    // ----- Hands-free -----

    fn start_hands_free(self: &Arc<Self>) {
        if !stt::model_ready() {
            self.error("Mains libres : le modèle vocal n'est pas téléchargé (Réglages → Voix).");
            return;
        }
        let (tx, rx) = mpsc::channel::<Vec<f32>>();
        let capture = match Capture::start(move |chunk| {
            let _ = tx.send(chunk.to_vec());
        }) {
            Ok(c) => c,
            Err(e) => {
                self.error(format!("{e:#}"));
                return;
            }
        };
        let stop = Arc::new(AtomicBool::new(false));
        *self.hf.lock() = Some(HandsFreeSession { _capture: capture, stop: stop.clone() });
        let v = self.clone();
        std::thread::Builder::new()
            .name("voice-handsfree".into())
            .spawn(move || {
                let mut w = Worker::new(v.clone());
                v.set_state("listening");
                while !stop.load(Ordering::Acquire) {
                    match rx.recv_timeout(Duration::from_millis(100)) {
                        Ok(chunk) => w.audio(&chunk),
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                    w.tick();
                }
            })
            .ok();
        self.preload();
    }

    fn stop_hands_free(self: &Arc<Self>) {
        if let Some(s) = self.hf.lock().take() {
            s.stop.store(true, Ordering::Release);
        }
        self.set_state(self.rest_state());
    }

    // ----- Model -----

    pub fn download_model(self: &Arc<Self>) {
        if stt::model_ready() || self.downloading.swap(true, Ordering::AcqRel) {
            return;
        }
        let v = self.clone();
        let proxy = self.settings.lock().proxy.clone();
        tauri::async_runtime::spawn(async move {
            v.emit(VoiceEvent::Model { status: "downloading", progress: 0.0 });
            let mut last = Instant::now();
            let vv = v.clone();
            let res = stt::download(&proxy, move |done, total| {
                if last.elapsed() > Duration::from_millis(250) {
                    last = Instant::now();
                    let progress = total.map_or(0.0, |t| done as f64 / t as f64);
                    vv.emit(VoiceEvent::Model { status: "downloading", progress });
                }
            })
            .await;
            v.downloading.store(false, Ordering::Release);
            match res {
                Ok(()) => {
                    v.emit(VoiceEvent::Model { status: "ready", progress: 1.0 });
                    let s = v.settings.lock().clone();
                    if s.enabled {
                        v.preload();
                        if s.hands_free && v.hf.lock().is_none() {
                            v.start_hands_free();
                        }
                    }
                }
                Err(e) => {
                    v.emit(VoiceEvent::Model { status: "error", progress: 0.0 });
                    v.error(format!("Téléchargement du modèle : {e:#}"));
                }
            }
        });
    }

    // ----- Spoken feedback -----

    /// Reads the first sentence of the agent's answer aloud, when enabled.
    pub fn speak_summary(self: &Arc<Self>, markdown: &str) {
        let (enabled, speak) = {
            let s = self.settings.lock();
            (s.enabled, s.speak)
        };
        if !enabled || !speak {
            return;
        }
        let Some(text) = speakable(markdown, 200) else { return };
        let v = self.clone();
        self.speaking.store(true, Ordering::Release);
        std::thread::spawn(move || {
            let run = |voice: Option<&str>| {
                let mut c = std::process::Command::new("/usr/bin/say");
                if let Some(vn) = voice {
                    c.args(["-v", vn]);
                }
                c.arg("--").arg(&text);
                c.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
                c.status().is_ok_and(|s| s.success())
            };
            if !run(Some("Thomas")) {
                run(None);
            }
            *v.muted_until.lock() = Instant::now() + Duration::from_millis(500);
            v.speaking.store(false, Ordering::Release);
        });
    }
}

/// The hands-free loop's state: VAD, wake-word machine, screen recorder.
struct Worker {
    v: Arc<Voice>,
    vad: Vad,
    hf: HandsFree,
    recorder: Option<Recorder>,
    speech_start: Option<Instant>,
    points: Vec<u64>,
    epoch: Instant,
}

impl Worker {
    fn new(v: Arc<Voice>) -> Self {
        Self {
            v,
            vad: Vad::new(),
            hf: HandsFree::new(),
            recorder: None,
            speech_start: None,
            points: Vec::new(),
            epoch: Instant::now(),
        }
    }

    fn now(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }

    fn audio(&mut self, chunk: &[f32]) {
        if self.v.is_muted() {
            self.vad.reset();
            return;
        }
        for ev in self.vad.push(chunk) {
            match ev {
                VadEvent::SpeechStart => self.speech_started(),
                VadEvent::Segment { samples, pre_ms } => self.segment(&samples, pre_ms),
            }
        }
    }

    fn tick(&mut self) {
        let now = self.now();
        for a in self.hf.on_tick(now) {
            self.apply(a);
        }
    }

    fn speech_started(&mut self) {
        self.hf.touch(self.now());
        self.speech_start = Some(Instant::now());
        // Armed on any speech so the frames before the wake word are not lost; dropped when it
        // turns out not to be addressed to us. Never asks for permission here.
        if self.recorder.is_none() && screen::has_access() {
            let v = self.v.clone();
            self.recorder = Some(Recorder::start(move || v.screen_denied()));
        }
    }

    fn segment(&mut self, samples: &[f32], pre_ms: u64) {
        let lang = self.v.settings.lock().language.clone();
        let segs: Vec<Segment> = match self.v.stt.transcribe(samples, &lang) {
            Ok(s) => s.into_iter().filter(|s| !is_hallucination(&s.text)).collect(),
            Err(e) => {
                self.v.error(format!("{e:#}"));
                return;
            }
        };
        let text = join_text(&segs);
        let phase = self.hf.phase;
        if text.is_empty() {
            if phase == Phase::Idle {
                self.recorder = None;
            }
            return;
        }
        log::info!("voice: heard {text:?}");
        let rel = match (self.speech_start, &self.recorder) {
            (Some(ss), Some(r)) => ss.saturating_duration_since(r.started()).as_millis() as u64,
            _ => 0,
        };
        let offset = rel.saturating_sub(pre_ms + 60);
        let pts: Vec<u64> =
            segs.iter().filter(|s| is_deictic(&s.text)).map(|s| offset + (s.t0_ms + s.t1_ms) / 2).collect();
        let now = self.now();
        let actions = self.hf.on_segment(&text, now);
        if actions.is_empty() {
            if self.hf.phase == Phase::Idle {
                self.recorder = None; // not for us
            }
            return;
        }
        self.points.extend(pts);
        for a in actions {
            self.apply(a);
        }
    }

    fn apply(&mut self, a: Action) {
        match a {
            Action::Wake => {
                play_sound("Tink");
                self.v.set_state("dictating");
                if self.recorder.is_none() {
                    let v = self.v.clone();
                    self.recorder = Some(Recorder::start(move || v.screen_denied()));
                }
            }
            Action::Append(text) => self.v.emit(VoiceEvent::Append { text }),
            Action::Send(text) => {
                self.v.set_state("sending");
                let points = std::mem::take(&mut self.points);
                self.v.finish(text, self.recorder.take(), &points, true);
                self.v.set_state("listening");
            }
            Action::Cancel => {
                play_sound("Basso");
                self.recorder = None;
                self.points.clear();
                self.v.emit(VoiceEvent::Clear);
                self.v.set_state("listening");
            }
            Action::Draft => {
                let points = std::mem::take(&mut self.points);
                self.v.finish(String::new(), self.recorder.take(), &points, false);
                self.v.set_state("listening");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_lines() {
        assert_eq!(context_line(0, None), None);
        assert!(context_line(3, None).unwrap().starts_with("[Contexte : 3 captures de l'écran prises pendant que je parlais, dans l'ordre"));
        assert!(context_line(1, None).unwrap().contains("1 capture de l'écran prise"));
        assert_eq!(context_line(0, Some("http://localhost:5173/")).unwrap(), "URL : http://localhost:5173/");
        assert!(context_line(2, Some("http://localhost:1")).unwrap().ends_with("\nURL : http://localhost:1"));
    }
}

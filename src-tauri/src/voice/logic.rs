//! Pure voice-mode logic: phrase detection, hands-free state machine, screen frame selection,
//! voice activity detection, text for speech. No I/O, so it is all unit-tested.

use std::collections::BTreeSet;

// ---------- Text helpers ----------

fn strip_accent(c: char) -> char {
    match c {
        'à' | 'â' | 'ä' => 'a',
        'é' | 'è' | 'ê' | 'ë' => 'e',
        'î' | 'ï' => 'i',
        'ô' | 'ö' => 'o',
        'ù' | 'û' | 'ü' => 'u',
        'ç' => 'c',
        c => c,
    }
}

/// Lowercase, no accents, punctuation turned into single spaces.
pub fn normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.to_lowercase().chars() {
        let c = strip_accent(c);
        if c.is_alphanumeric() {
            out.push(c);
        } else if !out.ends_with(' ') && !out.is_empty() {
            out.push(' ');
        }
    }
    out.trim_end().to_string()
}

/// A word of the original text with its byte span.
struct Word {
    start: usize,
    end: usize,
    norm: String,
}

fn words(s: &str) -> Vec<Word> {
    let mut out = Vec::new();
    let mut start = None;
    for (i, c) in s.char_indices() {
        if c.is_alphanumeric() {
            if start.is_none() {
                start = Some(i);
            }
        } else if let Some(st) = start.take() {
            out.push(Word { start: st, end: i, norm: normalize(&s[st..i]) });
        }
    }
    if let Some(st) = start {
        out.push(Word { start: st, end: s.len(), norm: normalize(&s[st..]) });
    }
    out
}

fn trim_punct(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_whitespace() || ",.;:!?-–—…".contains(c))
}

/// "Escouade, ajoute un bouton" -> Some("ajoute un bouton"). The wake word must open the text.
pub fn strip_wake(text: &str) -> Option<String> {
    let w = words(text);
    let first = w.first()?;
    if first.norm != "escouade" && first.norm != "escouades" {
        return None;
    }
    Some(trim_punct(&text[first.end..]).to_string())
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum End {
    None,
    /// The text to keep, without the trigger word.
    Send(String),
    Cancel,
}

/// "annule" anywhere cancels; "envoie" / "envoie le" / "go" at the end sends.
pub fn detect_end(text: &str) -> End {
    let w = words(text);
    if w.iter().any(|x| x.norm == "annule") {
        return End::Cancel;
    }
    let n = w.len();
    let cut = if n >= 2 && w[n - 2].norm == "envoie" && w[n - 1].norm == "le" {
        Some(w[n - 2].start)
    } else if n >= 1 && (w[n - 1].norm == "envoie" || w[n - 1].norm == "go") {
        Some(w[n - 1].start)
    } else {
        None
    };
    match cut {
        Some(c) => End::Send(trim_punct(&text[..c]).to_string()),
        None => End::None,
    }
}

/// Whether the text points at something on screen. Lowercase but accent-aware: the article
/// "la" is not the adverb "là".
pub fn is_deictic(text: &str) -> bool {
    let mut s = String::new();
    for c in text.to_lowercase().chars() {
        if c.is_alphanumeric() {
            s.push(c);
        } else if !s.ends_with(' ') {
            s.push(' ');
        }
    }
    let padded = format!(" {} ", s.trim());
    [
        "ça", "ici", "là", "regarde", "ce bouton", "cet élément", "cet element", "celui ci",
        "celle ci", "cette page", "ce truc",
    ]
    .iter()
    .any(|k| padded.contains(&format!(" {k} ")))
}

/// Phrases Whisper invents on near-silence.
pub fn is_hallucination(text: &str) -> bool {
    let n = normalize(text);
    n.is_empty()
        || n.starts_with("sous titr")
        || n.contains("merci d avoir regarde")
        || n.contains("blank audio")
        || n == "musique"
}

/// First sentence of a message, markdown stripped, at most `max` chars; for `say`.
pub fn speakable(markdown: &str, max: usize) -> Option<String> {
    let mut in_code = false;
    let mut lines = Vec::new();
    for l in markdown.lines() {
        let t = l.trim();
        if t.starts_with("```") {
            in_code = !in_code;
            continue;
        }
        if in_code || t.is_empty() || t.starts_with('|') || t.starts_with("---") {
            continue;
        }
        lines.push(t.to_string());
    }
    let line = lines.into_iter().next()?;
    let line = line.trim_start_matches(|c: char| "#>-*+ ".contains(c));
    // "1. item" -> "item"
    let digits = line.chars().take_while(|c| c.is_ascii_digit()).count();
    let line = match line[digits..].strip_prefix(". ") {
        Some(rest) if digits > 0 => rest,
        _ => line,
    }
    .to_string();
    // [text](url) -> text
    let mut plain = String::new();
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '[' => {}
            ']' => {
                if chars.peek() == Some(&'(') {
                    for d in chars.by_ref() {
                        if d == ')' {
                            break;
                        }
                    }
                }
            }
            '`' | '*' | '_' | '#' => {}
            c => plain.push(c),
        }
    }
    let plain = plain.trim();
    let mut end = plain.len();
    let b: Vec<(usize, char)> = plain.char_indices().collect();
    for (k, &(i, c)) in b.iter().enumerate() {
        if matches!(c, '.' | '!' | '?') && b.get(k + 1).is_none_or(|&(_, n)| n.is_whitespace()) {
            end = i + c.len_utf8();
            break;
        }
    }
    let mut s = plain[..end].trim().to_string();
    if s.chars().count() > max {
        s = s.chars().take(max).collect();
        if let Some(p) = s.rfind(' ') {
            s.truncate(p);
        }
    }
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

// ---------- Frame selection ----------

/// Mean absolute difference of two grayscale thumbnails, in 0..255.
pub fn thumb_diff(a: &[u8], b: &[u8]) -> f32 {
    let n = a.len().min(b.len());
    if n == 0 {
        return 0.0;
    }
    a.iter().zip(b).map(|(x, y)| (*x as i32 - *y as i32).unsigned_abs() as f32).sum::<f32>() / n as f32
}

/// Frames to send: those nearest each deictic moment, those where the screen changes, and the
/// last one; at most `max`, in order. `times` are in ms, ascending.
pub fn select_frames(thumbs: &[Vec<u8>], times: &[u64], deictic: &[u64], max: usize, threshold: f32) -> Vec<usize> {
    let n = thumbs.len().min(times.len());
    if n == 0 || max == 0 {
        return Vec::new();
    }
    let nearest = |t: u64| (0..n).min_by_key(|&i| times[i].abs_diff(t)).unwrap_or(0);
    let deictic_idx: BTreeSet<usize> = deictic.iter().map(|&t| nearest(t)).collect();
    let mut changed = Vec::new();
    let mut last = &thumbs[0];
    for i in 0..n {
        if deictic_idx.contains(&i) {
            last = &thumbs[i];
        } else if i > 0 && thumb_diff(&thumbs[i], last) > threshold {
            changed.push(i);
            last = &thumbs[i];
        }
    }
    // Priority when over the cap: the last frame, then the pointed ones, then the changes.
    let mut picked: Vec<usize> = vec![n - 1];
    for i in deictic_idx.iter().rev().chain(changed.iter().rev()) {
        if !picked.contains(i) {
            picked.push(*i);
        }
    }
    picked.truncate(max);
    picked.sort_unstable();
    picked
}

// ---------- Hands-free state machine ----------

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Phase {
    Idle,
    Dictating,
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum Action {
    /// The wake word was heard: confirmation sound, start dictation.
    Wake,
    /// Text to add to the input field.
    Append(String),
    /// Send what was dictated (the text is the last bit, trigger word removed).
    Send(String),
    Cancel,
    /// Silence: leave the text as a draft.
    Draft,
}

pub const DICTATION_TIMEOUT_MS: u64 = 8_000;

pub struct HandsFree {
    pub phase: Phase,
    last_activity: u64,
}

impl Default for HandsFree {
    fn default() -> Self {
        Self::new()
    }
}

impl HandsFree {
    pub fn new() -> Self {
        Self { phase: Phase::Idle, last_activity: 0 }
    }

    /// Speech heard (even before it is transcribed) keeps the dictation alive.
    pub fn touch(&mut self, now_ms: u64) {
        self.last_activity = now_ms;
    }

    /// A transcribed speech segment.
    pub fn on_segment(&mut self, text: &str, now_ms: u64) -> Vec<Action> {
        self.last_activity = now_ms;
        let mut out = Vec::new();
        let rest = match self.phase {
            Phase::Idle => match strip_wake(text) {
                Some(rest) => {
                    self.phase = Phase::Dictating;
                    out.push(Action::Wake);
                    rest
                }
                None => return out,
            },
            Phase::Dictating => text.trim().to_string(),
        };
        match detect_end(&rest) {
            End::Cancel => {
                self.phase = Phase::Idle;
                out.push(Action::Cancel);
            }
            End::Send(t) => {
                self.phase = Phase::Idle;
                out.push(Action::Send(t));
            }
            End::None => {
                if !rest.is_empty() {
                    out.push(Action::Append(rest));
                }
            }
        }
        out
    }

    /// Called regularly: 8 s without speech ends the dictation as a draft.
    pub fn on_tick(&mut self, now_ms: u64) -> Vec<Action> {
        if self.phase == Phase::Dictating && now_ms.saturating_sub(self.last_activity) > DICTATION_TIMEOUT_MS {
            self.phase = Phase::Idle;
            return vec![Action::Draft];
        }
        Vec::new()
    }
}

// ---------- Voice activity detection ----------

pub const FRAME: usize = 320; // 20 ms at 16 kHz

#[derive(Debug)]
pub enum VadEvent {
    SpeechStart,
    /// A finished utterance (16 kHz mono) with its pre-roll; `pre_ms` is the length of the
    /// pre-roll at its start.
    Segment { samples: Vec<f32>, pre_ms: u64 },
}

pub struct Vad {
    floor: f32,
    in_speech: bool,
    loud_run: usize,
    quiet_run: usize,
    pending: Vec<f32>,
    preroll: std::collections::VecDeque<Vec<f32>>,
    buf: Vec<f32>,
    pre_frames: usize,
}

const PREROLL_FRAMES: usize = 10; // 200 ms
const END_SILENCE_FRAMES: usize = 30; // 600 ms
const MIN_SPEECH_FRAMES: usize = 15; // 300 ms
const MAX_FRAMES: usize = 1250; // 25 s

fn rms(f: &[f32]) -> f32 {
    (f.iter().map(|x| x * x).sum::<f32>() / f.len().max(1) as f32).sqrt()
}

impl Default for Vad {
    fn default() -> Self {
        Self::new()
    }
}

impl Vad {
    pub fn new() -> Self {
        Self {
            floor: 0.003,
            in_speech: false,
            loud_run: 0,
            quiet_run: 0,
            pending: Vec::new(),
            preroll: Default::default(),
            buf: Vec::new(),
            pre_frames: 0,
        }
    }

    pub fn reset(&mut self) {
        *self = Self::new();
    }

    pub fn push(&mut self, samples: &[f32]) -> Vec<VadEvent> {
        self.pending.extend_from_slice(samples);
        let mut events = Vec::new();
        while self.pending.len() >= FRAME {
            let frame: Vec<f32> = self.pending.drain(..FRAME).collect();
            self.frame(frame, &mut events);
        }
        events
    }

    fn frame(&mut self, frame: Vec<f32>, events: &mut Vec<VadEvent>) {
        let level = rms(&frame);
        let start_thr = (self.floor * 3.0).max(0.012);
        let keep_thr = (self.floor * 2.0).max(0.008);
        if !self.in_speech {
            // The noise floor follows the quiet frames, slowly.
            if level < start_thr {
                self.floor = (self.floor * 0.95 + level * 0.05).clamp(0.0005, 0.05);
            }
            self.loud_run = if level >= start_thr { self.loud_run + 1 } else { 0 };
            self.preroll.push_back(frame);
            if self.preroll.len() > PREROLL_FRAMES + 3 {
                self.preroll.pop_front();
            }
            if self.loud_run >= 3 {
                self.in_speech = true;
                self.quiet_run = 0;
                // Pre-roll: the frames before the run started, plus the run itself.
                let keep = (PREROLL_FRAMES + 3).min(self.preroll.len());
                let skip = self.preroll.len() - keep;
                self.pre_frames = keep - 3.min(keep);
                self.buf = self.preroll.drain(..).skip(skip).flatten().collect();
                events.push(VadEvent::SpeechStart);
            }
        } else {
            self.buf.extend_from_slice(&frame);
            self.quiet_run = if level < keep_thr { self.quiet_run + 1 } else { 0 };
            let total = self.buf.len() / FRAME;
            if self.quiet_run >= END_SILENCE_FRAMES || total >= MAX_FRAMES {
                let speech = total.saturating_sub(self.quiet_run + self.pre_frames);
                let samples = std::mem::take(&mut self.buf);
                if speech >= MIN_SPEECH_FRAMES {
                    events.push(VadEvent::Segment { samples, pre_ms: self.pre_frames as u64 * 20 });
                }
                self.in_speech = false;
                self.loud_run = 0;
                self.quiet_run = 0;
                self.preroll.clear();
            }
        }
    }
}

// ---------- Resampling ----------

/// Streaming mono 16 kHz converter (linear interpolation) for any input rate / channel count.
pub struct Resampler {
    step: f64,
    pos: f64,
    prev: f32,
    channels: usize,
}

impl Resampler {
    pub fn new(rate: u32, channels: u16) -> Self {
        Self { step: rate as f64 / 16_000.0, pos: 0.0, prev: 0.0, channels: channels.max(1) as usize }
    }

    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        let mono: Vec<f32> = input.chunks(self.channels).map(|c| c.iter().sum::<f32>() / c.len() as f32).collect();
        // `pos` indexes into [prev, mono...]: index 0 is the previous chunk's last sample.
        let len = mono.len() as f64;
        while self.pos < len {
            let i = self.pos.floor();
            let frac = (self.pos - i) as f32;
            let a = if i < 0.0 { self.prev } else { mono[i as usize] };
            let b = mono.get(i as usize + 1).copied().unwrap_or(a);
            out.push(a + (b - a) * frac);
            self.pos += self.step;
        }
        self.pos -= len;
        if let Some(&l) = mono.last() {
            self.prev = l;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes() {
        assert_eq!(normalize("  Escouade, ÇA va ?! "), "escouade ca va");
    }

    #[test]
    fn wake_word() {
        assert_eq!(strip_wake("Escouade, ajoute un bouton"), Some("ajoute un bouton".into()));
        assert_eq!(strip_wake("escouade"), Some("".into()));
        assert_eq!(strip_wake("Escouade. Regarde ça !"), Some("Regarde ça".into()));
        assert_eq!(strip_wake("on parle de l'escouade"), None);
        assert_eq!(strip_wake("bonjour tout le monde"), None);
        assert_eq!(strip_wake(""), None);
    }

    #[test]
    fn end_phrases() {
        assert_eq!(detect_end("Ajoute un bouton, envoie."), End::Send("Ajoute un bouton".into()));
        assert_eq!(detect_end("Ajoute un bouton envoie le"), End::Send("Ajoute un bouton".into()));
        assert_eq!(detect_end("c'est bon, Go !"), End::Send("c'est bon".into()));
        assert_eq!(detect_end("envoie"), End::Send("".into()));
        assert_eq!(detect_end("Annule tout"), End::Cancel);
        assert_eq!(detect_end("Annule, envoie"), End::Cancel);
        assert_eq!(detect_end("envoie un mail à Paul"), End::None);
        assert_eq!(detect_end("on va gogo plus loin"), End::None);
    }

    #[test]
    fn deictic_words() {
        assert!(is_deictic("ce bouton est mal aligné"));
        assert!(is_deictic("Mets ça en rouge"));
        assert!(is_deictic("regarde"));
        assert!(is_deictic("celui-ci"));
        assert!(!is_deictic("la page d'accueil"));
        assert!(!is_deictic("ajoute un bouton de connexion"));
    }

    #[test]
    fn hallucinations() {
        assert!(is_hallucination("Sous-titrage ST' 501"));
        assert!(is_hallucination(" "));
        assert!(!is_hallucination("ajoute un bouton"));
    }

    #[test]
    fn speech_text() {
        let md = "## Titre\n\nJ'ai corrigé le **bouton** dans `Header.svelte`. Il reste un test.\n\n```rs\nfn x(){}\n```";
        assert_eq!(speakable(md, 200).unwrap(), "Titre");
        let md = "J'ai corrigé le **bouton** dans `Header.svelte`. Il reste un test.";
        assert_eq!(speakable(md, 200).unwrap(), "J'ai corrigé le bouton dans Header.svelte.");
        assert_eq!(speakable("Voir [la doc](http://x.y/z) pour plus", 200).unwrap(), "Voir la doc pour plus");
        assert_eq!(speakable("```\ncode\n```", 200), None);
        let long = "mot ".repeat(100);
        assert!(speakable(&long, 50).unwrap().chars().count() <= 50);
        assert_eq!(speakable("Version 1.2 prête", 200).unwrap(), "Version 1.2 prête");
    }

    fn flat(v: u8) -> Vec<u8> {
        vec![v; 1024]
    }

    #[test]
    fn frames_static_screen_keeps_last_only() {
        let thumbs = vec![flat(100); 10];
        let times: Vec<u64> = (0..10).map(|i| i * 700).collect();
        assert_eq!(select_frames(&thumbs, &times, &[], 6, 8.0), vec![9]);
    }

    #[test]
    fn frames_deictic_and_changes() {
        let mut thumbs = vec![flat(100); 10];
        for t in thumbs.iter_mut().skip(5) {
            *t = flat(160);
        }
        let times: Vec<u64> = (0..10).map(|i| i * 700).collect();
        // Pointing at 2 s -> frame 3 (2100 ms); the screen changes at frame 5; last is 9.
        assert_eq!(select_frames(&thumbs, &times, &[2000], 6, 8.0), vec![3, 5, 9]);
    }

    #[test]
    fn frames_are_capped_with_priority() {
        let thumbs: Vec<Vec<u8>> = (0..20).map(|i| flat(if i % 2 == 0 { 0 } else { 200 })).collect();
        let times: Vec<u64> = (0..20).map(|i| i * 700).collect();
        let sel = select_frames(&thumbs, &times, &[700], 6, 8.0);
        assert_eq!(sel.len(), 6);
        assert!(sel.contains(&19) && sel.contains(&1));
        assert!(sel.windows(2).all(|w| w[0] < w[1]));
        assert!(select_frames(&[], &[], &[], 6, 8.0).is_empty());
    }

    #[test]
    fn hands_free_ignores_conversation() {
        let mut m = HandsFree::new();
        assert!(m.on_segment("alors je te disais que demain", 0).is_empty());
        assert!(m.on_segment("envoie", 1000).is_empty());
        assert_eq!(m.phase, Phase::Idle);
    }

    #[test]
    fn hands_free_dictation_and_send() {
        let mut m = HandsFree::new();
        assert_eq!(
            m.on_segment("Escouade, ajoute un bouton", 0),
            vec![Action::Wake, Action::Append("ajoute un bouton".into())]
        );
        assert_eq!(m.on_segment("en haut à droite", 2000), vec![Action::Append("en haut à droite".into())]);
        assert_eq!(m.on_segment("envoie", 4000), vec![Action::Send("".into())]);
        assert_eq!(m.phase, Phase::Idle);
    }

    #[test]
    fn hands_free_one_breath_and_cancel() {
        let mut m = HandsFree::new();
        assert_eq!(
            m.on_segment("Escouade ajoute un test envoie le", 0),
            vec![Action::Wake, Action::Send("ajoute un test".into())]
        );
        assert_eq!(m.on_segment("Escouade fais ceci", 10), vec![Action::Wake, Action::Append("fais ceci".into())]);
        assert_eq!(m.on_segment("non annule", 20), vec![Action::Cancel]);
        assert_eq!(m.phase, Phase::Idle);
    }

    #[test]
    fn hands_free_timeout_leaves_draft() {
        let mut m = HandsFree::new();
        m.on_segment("Escouade ajoute", 1000);
        assert!(m.on_tick(5000).is_empty());
        m.touch(6000);
        assert!(m.on_tick(13000).is_empty());
        assert_eq!(m.on_tick(14500), vec![Action::Draft]);
        assert_eq!(m.phase, Phase::Idle);
        assert!(m.on_tick(99999).is_empty());
    }

    fn tone(n: usize, amp: f32) -> Vec<f32> {
        (0..n).map(|i| (i as f32 * 0.3).sin() * amp).collect()
    }

    fn count(v: &mut Vad, s: &[f32]) -> (usize, usize) {
        let (mut starts, mut segs) = (0, 0);
        for e in v.push(s) {
            match e {
                VadEvent::SpeechStart => starts += 1,
                VadEvent::Segment { .. } => segs += 1,
            }
        }
        (starts, segs)
    }

    #[test]
    fn vad_segments_speech_and_ignores_blips() {
        let mut v = Vad::new();
        let mut total = (0, 0);
        for chunk in [tone(16000, 0.001), tone(16000, 0.1), tone(16000, 0.001)] {
            let c = count(&mut v, &chunk); // 1 s silence, 1 s speech, 1 s silence
            total = (total.0 + c.0, total.1 + c.1);
        }
        assert_eq!(total, (1, 1));
        let mut blip = (0, 0);
        for chunk in [tone(2400, 0.1), tone(16000, 0.001)] {
            let c = count(&mut v, &chunk); // a 150 ms blip
            blip = (blip.0 + c.0, blip.1 + c.1);
        }
        assert_eq!(blip.1, 0, "a blip under 300 ms is dropped");
    }

    #[test]
    fn resamples_48k_stereo() {
        let mut r = Resampler::new(48_000, 2);
        let input: Vec<f32> = vec![0.5; 48_000 * 2];
        let mut out = Vec::new();
        r.process(&input, &mut out);
        assert!((out.len() as i32 - 16_000).abs() <= 2);
        assert!(out.iter().skip(1).all(|x| (x - 0.5).abs() < 1e-4));
    }
}

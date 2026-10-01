//! Speech to text: whisper.cpp (Metal) with a model downloaded on first use.

use anyhow::{anyhow, bail, Context, Result};
use parking_lot::Mutex;
use std::path::PathBuf;
use std::sync::Arc;
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

pub const MODEL_FILE: &str = "ggml-large-v3-turbo-q5_0.bin";
const MODEL_URL: &str = "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3-turbo-q5_0.bin";
/// The real file is about 574 MB: anything much smaller is a broken download.
const MODEL_MIN_BYTES: u64 = 500_000_000;
/// Bias towards the words of a dev's vocabulary.
const PROMPT: &str = "Escouade, agent, bouton, composant, interface, Svelte, TypeScript, Claude, commit, branche.";

pub fn model_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_default().join(".escouade").join("models")
}

pub fn model_path() -> PathBuf {
    model_dir().join(MODEL_FILE)
}

pub fn model_ready() -> bool {
    std::fs::metadata(model_path()).is_ok_and(|m| m.len() >= MODEL_MIN_BYTES)
}

#[derive(Debug, Clone)]
pub struct Segment {
    pub t0_ms: u64,
    pub t1_ms: u64,
    pub text: String,
}

/// Downloads the model to a `.part` file, then renames it. `progress(done, total)` is called
/// as data arrives.
pub async fn download(proxy: &str, mut progress: impl FnMut(u64, Option<u64>)) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    std::fs::create_dir_all(model_dir())?;
    let part = model_path().with_extension("bin.part");
    let mut client = reqwest::Client::builder();
    if !proxy.trim().is_empty() {
        client = client.proxy(reqwest::Proxy::all(proxy.trim())?);
    }
    let mut resp = client.build()?.get(MODEL_URL).send().await?.error_for_status()?;
    let total = resp.content_length();
    let mut file = tokio::fs::File::create(&part).await?;
    let mut done = 0u64;
    while let Some(chunk) = resp.chunk().await? {
        file.write_all(&chunk).await?;
        done += chunk.len() as u64;
        progress(done, total);
    }
    file.flush().await?;
    drop(file);
    if done < MODEL_MIN_BYTES {
        let _ = std::fs::remove_file(&part);
        bail!("téléchargement incomplet ({done} octets)");
    }
    std::fs::rename(&part, model_path())?;
    Ok(())
}

#[derive(Default)]
pub struct Stt {
    ctx: Mutex<Option<Arc<WhisperContext>>>,
}

impl Stt {
    /// The model context, loaded once.
    pub fn context(&self) -> Result<Arc<WhisperContext>> {
        let mut g = self.ctx.lock();
        if let Some(c) = g.as_ref() {
            return Ok(c.clone());
        }
        if !model_ready() {
            bail!("le modèle vocal n'est pas téléchargé");
        }
        let t = std::time::Instant::now();
        let path = model_path();
        let ctx = WhisperContext::new_with_params(&path, WhisperContextParameters::default())
            .map_err(|e| anyhow!("chargement du modèle : {e}"))?;
        log::info!("voice: model loaded in {} ms", t.elapsed().as_millis());
        let ctx = Arc::new(ctx);
        *g = Some(ctx.clone());
        Ok(ctx)
    }

    /// `samples`: 16 kHz mono. Segments carry their time within the audio.
    pub fn transcribe(&self, samples: &[f32], lang: &str) -> Result<Vec<Segment>> {
        let ctx = self.context()?;
        let mut state = ctx.create_state().map_err(|e| anyhow!("whisper : {e}"))?;
        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        let auto = lang.is_empty() || lang == "auto";
        if auto {
            params.set_detect_language(true);
        } else {
            params.set_language(Some(lang));
        }
        params.set_translate(false);
        params.set_no_context(true);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_special(false);
        params.set_print_timestamps(false);
        params.set_suppress_blank(true);
        params.set_initial_prompt(PROMPT);
        params.set_n_threads(std::thread::available_parallelism().map_or(4, |n| n.get().min(8)) as i32);
        // whisper.cpp refuses audio shorter than a second.
        let mut audio = samples.to_vec();
        if audio.len() < 16_800 {
            audio.resize(16_800, 0.0);
        }
        state.full(params, &audio).map_err(|e| anyhow!("transcription : {e}"))?;
        let mut out = Vec::new();
        for seg in state.as_iter() {
            let text = seg.to_str_lossy().context("segment illisible")?.trim().to_string();
            if text.is_empty() {
                continue;
            }
            out.push(Segment {
                t0_ms: seg.start_timestamp().max(0) as u64 * 10,
                t1_ms: seg.end_timestamp().max(0) as u64 * 10,
                text,
            });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wav_samples(bytes: &[u8]) -> Vec<f32> {
        let pos = bytes.windows(4).position(|w| w == b"data").expect("data chunk") + 8;
        bytes[pos..].chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0).collect()
    }

    /// Real speech through the real model: `say` voices a French sentence, whisper reads it.
    /// Run with: cargo test --release transcribes_french_speech -- --ignored --nocapture
    #[test]
    #[ignore]
    fn transcribes_french_speech() {
        if !model_ready() {
            let rt = tokio::runtime::Runtime::new().unwrap();
            let mut last = 0;
            rt.block_on(download("", |d, t| {
                let pct = t.map_or(0, |t| d * 100 / t);
                if pct != last {
                    last = pct;
                    println!("download {pct}%");
                }
            }))
            .expect("download");
        }
        let sentence = "Ajoute un bouton de connexion en haut à droite de la page";
        let wav = "/tmp/t.wav";
        let st = std::process::Command::new("/usr/bin/say")
            .args(["-v", "Thomas", "-o", wav, "--data-format=LEI16@16000", sentence])
            .status()
            .expect("say");
        assert!(st.success());
        let samples = wav_samples(&std::fs::read(wav).unwrap());
        let t = std::time::Instant::now();
        let segs = Stt::default().transcribe(&samples, "fr").expect("transcribe");
        println!("transcription time (incl. model load): {} ms", t.elapsed().as_millis());
        let text = segs.iter().map(|s| s.text.as_str()).collect::<Vec<_>>().join(" ");
        println!("TRANSCRIPT: {text}");
        // Once the model is loaded: what a dictation costs.
        let stt = Stt::default();
        stt.transcribe(&samples, "fr").expect("warm-up");
        let t = std::time::Instant::now();
        stt.transcribe(&samples, "fr").expect("second run");
        println!("transcription time (model loaded): {} ms", t.elapsed().as_millis());
        let n = super::super::logic::normalize(&text);
        assert!(n.contains("bouton") && n.contains("connexion"), "got: {text}");
    }
}

//! Microphone capture: the default input, converted to 16 kHz mono f32.

use super::logic::Resampler;
use anyhow::{anyhow, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;

/// A running capture. The stream lives in its own thread (cpal streams are not `Send`);
/// dropping the capture stops it.
pub struct Capture {
    stop: Arc<AtomicBool>,
}

impl Capture {
    /// `sink` receives chunks of 16 kHz mono samples on the audio thread: keep it quick.
    pub fn start(mut sink: impl FnMut(&[f32]) + Send + 'static) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel::<Result<()>>();
        let flag = stop.clone();
        std::thread::Builder::new().name("voice-capture".into()).spawn(move || {
            let built = (|| -> Result<cpal::Stream> {
                let device = cpal::default_host()
                    .default_input_device()
                    .ok_or_else(|| anyhow!("aucun micro disponible"))?;
                let config = device.default_input_config().map_err(|e| anyhow!("micro : {e}"))?;
                let mut rs = Resampler::new(config.sample_rate().0, config.channels());
                let mut out = Vec::new();
                let err = |e| log::warn!("voice: audio stream error: {e}");
                let stream_config: cpal::StreamConfig = config.clone().into();
                let stream = match config.sample_format() {
                    cpal::SampleFormat::F32 => device.build_input_stream(
                        &stream_config,
                        move |data: &[f32], _: &_| {
                            out.clear();
                            rs.process(data, &mut out);
                            sink(&out);
                        },
                        err,
                        None,
                    ),
                    cpal::SampleFormat::I16 => device.build_input_stream(
                        &stream_config,
                        move |data: &[i16], _: &_| {
                            let f: Vec<f32> = data.iter().map(|s| *s as f32 / 32768.0).collect();
                            out.clear();
                            rs.process(&f, &mut out);
                            sink(&out);
                        },
                        err,
                        None,
                    ),
                    f => return Err(anyhow!("format audio non géré : {f:?}")),
                }
                .map_err(|e| anyhow!("micro : {e}"))?;
                stream.play().map_err(|e| anyhow!("micro : {e}"))?;
                Ok(stream)
            })();
            match built {
                Ok(stream) => {
                    let _ = tx.send(Ok(()));
                    while !flag.load(Ordering::Acquire) {
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                    drop(stream);
                }
                Err(e) => {
                    let _ = tx.send(Err(e));
                }
            }
        })?;
        rx.recv().map_err(|_| anyhow!("le thread audio s'est arrêté"))??;
        Ok(Self { stop })
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

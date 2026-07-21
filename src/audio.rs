//! cpal glue: opens the default output device and drives the [`Synth`] from a
//! channel of note events sent by the UI thread.

use std::sync::mpsc::Receiver;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SizedSample};

use crate::synth::{Patch, Synth, VoiceMonitor};

/// Commands sent from the UI thread to the real-time audio thread. A tone is
/// identified by its MIDI note `id` (for on/off matching); `freq` carries the
/// actual, possibly just-intoned, pitch.
pub enum SynthEvent {
    NoteOn { id: u8, freq: f32, pan: f32 },
    NoteOff { id: u8 },
    SetPatch(Patch),
}

/// Details about the running output stream, shown in the UI.
pub struct AudioInfo {
    pub device: String,
    pub sample_rate: u32,
}

/// Open the default output device and start streaming.
///
/// The returned [`cpal::Stream`] must be kept alive for as long as you want
/// sound — dropping it stops audio.
pub fn start(
    rx: Receiver<SynthEvent>,
    monitor: Arc<VoiceMonitor>,
) -> Result<(cpal::Stream, AudioInfo)> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or_else(|| anyhow!("no default output audio device"))?;
    let supported = device.default_output_config()?;

    let info = AudioInfo {
        device: device.name().unwrap_or_else(|_| "unknown".to_string()),
        sample_rate: supported.sample_rate().0,
    };

    let format = supported.sample_format();
    let config = supported.into();
    let stream = match format {
        cpal::SampleFormat::F32 => build::<f32>(&device, &config, rx, monitor)?,
        cpal::SampleFormat::I16 => build::<i16>(&device, &config, rx, monitor)?,
        cpal::SampleFormat::U16 => build::<u16>(&device, &config, rx, monitor)?,
        other => return Err(anyhow!("unsupported sample format: {other:?}")),
    };
    stream.play()?;

    Ok((stream, info))
}

/// Build an output stream for a concrete sample type `T`.
fn build<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    rx: Receiver<SynthEvent>,
    monitor: Arc<VoiceMonitor>,
) -> Result<cpal::Stream>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = config.channels as usize;
    let mut synth = Synth::new(config.sample_rate.0 as f32, monitor);

    let stream = device.build_output_stream(
        config,
        move |output: &mut [T], _: &cpal::OutputCallbackInfo| {
            // Apply every note event that arrived since the last callback.
            // `try_recv` never blocks, so this is real-time safe.
            while let Ok(event) = rx.try_recv() {
                match event {
                    SynthEvent::NoteOn { id, freq, pan } => synth.note_on(id, freq, pan),
                    SynthEvent::NoteOff { id } => synth.note_off(id),
                    SynthEvent::SetPatch(patch) => synth.set_patch(patch),
                }
            }

            // Stereo: L to even channels, R to odd (mono devices get L+R mix).
            for frame in output.chunks_mut(channels) {
                let (l, r) = synth.next_frame();
                if channels == 1 {
                    frame[0] = T::from_sample((l + r) * 0.5);
                } else {
                    for (i, slot) in frame.iter_mut().enumerate() {
                        *slot = T::from_sample(if i % 2 == 0 { l } else { r });
                    }
                }
            }
        },
        |err| eprintln!("audio stream error: {err}"),
        None,
    )?;

    Ok(stream)
}

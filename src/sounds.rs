//! Brief recording cues, played off the input/capture threads.
use std::sync::{mpsc, OnceLock};

use anyhow::{Context, Result};
use libpulse_binding::{
    def::BufferAttr,
    sample::{Format, Spec},
    stream::Direction,
};
use libpulse_simple_binding::Simple;

#[derive(Clone, Copy)]
pub enum Cue {
    Start,
    Stop,
}

pub fn play(cue: Cue) {
    if std::env::var("WHISP_SOUNDS").as_deref() == Ok("0") {
        return;
    }
    static QUEUE: OnceLock<mpsc::SyncSender<Cue>> = OnceLock::new();
    let queue = QUEUE.get_or_init(|| {
        let (tx, rx) = mpsc::sync_channel(8);
        std::thread::spawn(move || {
            for cue in rx {
                if let Err(e) = play_now(cue) {
                    eprintln!("[whisp] recording sound failed: {e:#}");
                }
            }
        });
        tx
    });
    // An unavailable audio server must never stall dictation.
    let _ = queue.try_send(cue);
}

fn play_now(cue: Cue) -> Result<()> {
    let spec = Spec {
        format: Format::S16le,
        channels: 1,
        rate: 48_000,
    };
    let sink = std::env::var("WHISP_SOUND_SINK")
        .ok()
        .filter(|s| !s.is_empty());
    let buffer = BufferAttr {
        maxlength: u32::MAX,
        tlength: spec.rate * 2 * 40 / 1000,
        prebuf: 0,
        minreq: u32::MAX,
        fragsize: u32::MAX,
    };
    let stream = Simple::new(
        None,
        "whisp recording cues",
        Direction::Playback,
        sink.as_deref(),
        "Recording cue",
        &spec,
        None,
        Some(&buffer),
    )
    .context("opening playback")?;
    let notes = match cue {
        Cue::Start => [660.0, 880.0],
        Cue::Stop => [880.0, 660.0],
    };
    let mut pcm = Vec::new();
    for hz in notes {
        // Two 65 ms notes with a short gap; fade both edges to avoid clicks.
        let frames = spec.rate as usize * 65 / 1000;
        let fade = spec.rate as usize * 8 / 1000;
        for i in 0..frames {
            let envelope = (i.min(frames - 1 - i) as f32 / fade as f32).min(1.0);
            let phase = std::f32::consts::TAU * hz * i as f32 / spec.rate as f32;
            let sample = (phase.sin() * envelope * 0.18 * i16::MAX as f32) as i16;
            pcm.extend_from_slice(&sample.to_le_bytes());
        }
        pcm.resize(pcm.len() + spec.rate as usize * 10 / 1000 * 2, 0);
    }
    stream.write(&pcm).context("writing cue")?;
    stream.drain().context("finishing cue")?;
    Ok(())
}

pub fn preview() -> Result<()> {
    play_now(Cue::Start)?;
    std::thread::sleep(std::time::Duration::from_millis(500));
    play_now(Cue::Stop)
}

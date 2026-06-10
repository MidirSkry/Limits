//! All sound, synthesized at startup — zero asset files.
//!
//! Every clip is generated as PCM samples, wrapped in an in-memory WAV (the
//! `wav` bevy feature provides the decoder), and stored as a normal
//! `Handle<AudioSource>`. Gameplay systems never touch audio APIs directly:
//! they push `SfxEvent`s into the `SfxQueue` resource and this module drains
//! it. Loops (laser, jetpack, ambient drone) are driven from small state
//! resources published by the player module.

use bevy::audio::{AudioPlayer, AudioSink, AudioSource, PlaybackSettings, Volume};
use bevy::prelude::*;
use std::sync::Arc;

use crate::player::{JetState, LaserState};

const RATE: u32 = 44_100;

// ---------------------------------------------------------------------------
// Public interface: queue an event, this module makes the noise.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
pub enum SfxEvent {
    /// Block destroyed. Crystal breaks ring; rock breaks crunch. `pitch`
    /// scales playback speed (deeper/harder blocks sound heavier at <1.0).
    Break { crystal: bool, pitch: f32 },
    Pickup { pitch: f32 },
    Sell,
    Buy,
    Deny,
    Explosion,
    Plant,
    Land,
    WarpUp,
    WarpDown,
    Overheat,
    Vent,
}

#[derive(Resource, Default)]
pub struct SfxQueue(pub Vec<SfxEvent>);

impl SfxQueue {
    pub fn push(&mut self, e: SfxEvent) {
        self.0.push(e);
    }
}

// ---------------------------------------------------------------------------
// Plugin
// ---------------------------------------------------------------------------

pub struct SoundPlugin;

impl Plugin for SoundPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<SfxQueue>()
            .add_systems(Startup, setup_sfx)
            .add_systems(Update, (play_queued, laser_loop_ctl, jet_loop_ctl));
    }
}

#[derive(Resource)]
struct Sfx {
    laser_loop: Handle<AudioSource>,
    jet_loop: Handle<AudioSource>,
    rock_break: Handle<AudioSource>,
    crystal_break: Handle<AudioSource>,
    pickup: Handle<AudioSource>,
    sell: Handle<AudioSource>,
    buy: Handle<AudioSource>,
    deny: Handle<AudioSource>,
    explosion: Handle<AudioSource>,
    plant: Handle<AudioSource>,
    land: Handle<AudioSource>,
    warp_up: Handle<AudioSource>,
    warp_down: Handle<AudioSource>,
    overheat: Handle<AudioSource>,
    vent: Handle<AudioSource>,
}

// ---------------------------------------------------------------------------
// Synthesis primitives
// ---------------------------------------------------------------------------

const TAU: f32 = std::f32::consts::TAU;

/// Deterministic white noise in [-1, 1] — no RNG state, loop-safe.
fn noise(i: usize) -> f32 {
    let mut x = i as u64;
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    ((x >> 40) as f32) / ((1u64 << 24) as f32) * 2.0 - 1.0
}

/// Normalize peak to `peak`, encode 16-bit mono WAV, hand back an AudioSource.
fn wav(mut samples: Vec<f32>, peak: f32) -> AudioSource {
    let max = samples.iter().fold(1e-6f32, |m, s| m.max(s.abs()));
    let k = peak / max;
    for s in &mut samples {
        *s *= k;
    }
    let n = samples.len() as u32;
    let data_len = n * 2;
    let mut b: Vec<u8> = Vec::with_capacity(44 + data_len as usize);
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data_len).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes()); // PCM chunk size
    b.extend_from_slice(&1u16.to_le_bytes()); // PCM format
    b.extend_from_slice(&1u16.to_le_bytes()); // mono
    b.extend_from_slice(&RATE.to_le_bytes());
    b.extend_from_slice(&(RATE * 2).to_le_bytes()); // byte rate
    b.extend_from_slice(&2u16.to_le_bytes()); // block align
    b.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data_len.to_le_bytes());
    for s in &samples {
        b.extend_from_slice(&((s.clamp(-1.0, 1.0) * 32_767.0) as i16).to_le_bytes());
    }
    AudioSource {
        bytes: Arc::from(b.into_boxed_slice()),
    }
}

/// Render `secs` of audio through a per-sample closure of (t, i).
fn render(secs: f32, mut f: impl FnMut(f32, usize) -> f32) -> Vec<f32> {
    let n = (secs * RATE as f32) as usize;
    (0..n).map(|i| f(i as f32 / RATE as f32, i)).collect()
}

/// Crossfade the last `n` samples into the first `n` so a loop has no seam.
fn loopify(mut s: Vec<f32>, n: usize) -> Vec<f32> {
    let len = s.len();
    for k in 0..n.min(len / 2) {
        let w = k as f32 / n as f32;
        s[len - n + k] = s[len - n + k] * (1.0 - w) + s[k] * w;
    }
    s
}

fn expd(t: f32, tau: f32) -> f32 {
    (-t / tau).exp()
}

// ---------------------------------------------------------------------------
// Clip recipes
// ---------------------------------------------------------------------------

fn setup_sfx(mut commands: Commands, mut audio: ResMut<Assets<AudioSource>>) {
    // Mining laser: detuned saw + harmonics with a 7Hz phase wobble. Both the
    // carrier (98Hz) and the wobble are integer cycles over the 1s buffer, so
    // it loops seamlessly by construction.
    let laser_loop = render(1.0, |t, i| {
        let phase = TAU * 98.0 * t + 0.5 * (TAU * 7.0 * t).sin();
        let saw = (phase / TAU).fract() * 2.0 - 1.0;
        let x = 0.45 * saw + 0.30 * (2.0 * phase).sin() + 0.12 * (3.0 * phase).sin()
            + 0.06 * noise(i);
        x / (1.0 + x.abs()) // soft clip
    });

    // Jetpack: lowpassed noise roar + a 56Hz rumble (integer cycles over 0.75s).
    let mut lp = 0.0f32;
    let jet_loop = loopify(
        render(0.75, |t, i| {
            lp += 0.18 * (noise(i) - lp);
            lp * 1.4 + 0.35 * (TAU * 56.0 * t).sin()
        }),
        2048,
    );

    // Ambient drone: detuned low sines, amplitudes breathing on integer-cycle
    // LFOs over the 6s buffer — eerie, seamless, very quiet.
    let drone_loop = render(6.0, |t, _| {
        let lfo = |k: f32| 0.5 + 0.5 * (TAU * k * t / 6.0).sin();
        0.50 * (TAU * 55.0 * t).sin() * (0.6 + 0.4 * lfo(1.0))
            + 0.25 * (TAU * 82.5 * t).sin() * (0.5 + 0.5 * lfo(2.0))
            + 0.18 * (TAU * 110.0 * t).sin() * (0.4 + 0.6 * lfo(3.0))
            + 0.10 * (TAU * 165.0 * t).sin() * lfo(5.0)
    });

    // Rock break: downward zap + initial crunch.
    let rock_break = render(0.18, |t, i| {
        let f = 700.0 * (-t * 9.0).exp() + 110.0;
        (TAU * f * t).sin() * expd(t, 0.05)
            + noise(i) * expd(t, 0.012) * 0.8
    });

    // Crystal break: a bright ringing triad with shimmer.
    let crystal_break = render(0.45, |t, i| {
        0.5 * (TAU * 1244.5 * t).sin() * expd(t, 0.10)
            + 0.4 * (TAU * 1865.0 * t).sin() * expd(t, 0.14)
            + 0.3 * (TAU * 2489.0 * t).sin() * expd(t, 0.18)
            + 0.08 * noise(i) * expd(t, 0.02)
    });

    // Pickup: tiny rising blip.
    let pickup = render(0.12, |t, _| {
        let f = 1200.0 + 900.0 * (t * 14.0).min(1.0);
        ((TAU * f * t).sin() + 0.4 * (TAU * f * 2.0 * t).sin()) * expd(t, 0.035)
    });

    // Sell: three-chime arpeggio with sparkle.
    let sell = render(0.7, |t, i| {
        let note = |f: f32, at: f32| {
            if t < at {
                0.0
            } else {
                (TAU * f * (t - at)).sin() * expd(t - at, 0.16)
            }
        };
        note(880.0, 0.0) + note(1108.7, 0.09) + note(1318.5, 0.18)
            + 0.05 * noise(i) * expd(t, 0.30)
    });

    // Buy: affirmative click-blip.
    let buy = render(0.20, |t, i| {
        (TAU * 660.0 * t).sin() * expd(t, 0.06)
            + 0.5 * (TAU * 990.0 * t).sin() * expd(t, 0.04)
            + noise(i) * expd(t, 0.004)
    });

    // Deny: low double buzz.
    let deny = render(0.30, |t, _| {
        let gate = if t < 0.10 || (0.15..0.25).contains(&t) { 1.0 } else { 0.0 };
        ((TAU * 120.0 * t).sin().signum() * 0.6 + 0.4 * (TAU * 60.0 * t).sin()) * gate
    });

    // Explosion: brown-noise boom + sub thump + first-instant crack.
    let mut brown = 0.0f32;
    let explosion = render(1.5, |t, i| {
        brown = (brown + 0.12 * noise(i)) * 0.996;
        brown * 6.0 * (-t * 2.6).exp()
            + 0.8 * (TAU * 42.0 * t).sin() * (-t * 2.2).exp()
            + noise(i) * expd(t, 0.015)
    });

    // Plant: charge sticks to the ground with a thock.
    let plant = render(0.15, |t, i| {
        (TAU * 200.0 * t).sin() * expd(t, 0.04) + 0.5 * noise(i) * expd(t, 0.006)
    });

    // Land: soft suit thud.
    let mut lp2 = 0.0f32;
    let land = render(0.16, |t, i| {
        lp2 += 0.25 * (noise(i) - lp2);
        (TAU * 80.0 * t).sin() * expd(t, 0.05) + lp2 * expd(t, 0.03)
    });

    // Teleport sweeps.
    let warp_up = render(0.55, |t, _| {
        let f = 250.0 * (1500.0f32 / 250.0).powf(t / 0.55);
        ((TAU * f * t).sin() + 0.3 * (TAU * f * 1.5 * t).sin())
            * (1.0 - (t / 0.55)).max(0.0).powf(0.4)
    });
    let warp_down = render(0.55, |t, _| {
        let f = 1500.0 * (250.0f32 / 1500.0).powf(t / 0.55);
        ((TAU * f * t).sin() + 0.3 * (TAU * f * 1.5 * t).sin())
            * (1.0 - (t / 0.55)).max(0.0).powf(0.4)
    });

    // Overheat: two hard beeps.
    let overheat = render(0.50, |t, _| {
        let gate = if t < 0.16 || (0.26..0.42).contains(&t) { 1.0 } else { 0.0 };
        let x = (TAU * 1450.0 * t).sin() * 1.8;
        (x / (1.0 + x.abs())) * gate
    });

    // Vent: steam hiss bleeding off.
    let mut lp3 = 0.0f32;
    let vent = render(0.85, |t, i| {
        lp3 += 0.45 * (noise(i) - lp3);
        lp3 * (1.0 - expd(t, 0.02)) * expd(t, 0.35)
    });

    commands.insert_resource(Sfx {
        laser_loop: audio.add(wav(laser_loop, 0.8)),
        jet_loop: audio.add(wav(jet_loop, 0.8)),
        rock_break: audio.add(wav(rock_break, 0.85)),
        crystal_break: audio.add(wav(crystal_break, 0.85)),
        pickup: audio.add(wav(pickup, 0.8)),
        sell: audio.add(wav(sell, 0.85)),
        buy: audio.add(wav(buy, 0.8)),
        deny: audio.add(wav(deny, 0.7)),
        explosion: audio.add(wav(explosion, 0.95)),
        plant: audio.add(wav(plant, 0.8)),
        land: audio.add(wav(land, 0.8)),
        warp_up: audio.add(wav(warp_up, 0.8)),
        warp_down: audio.add(wav(warp_down, 0.8)),
        overheat: audio.add(wav(overheat, 0.8)),
        vent: audio.add(wav(vent, 0.8)),
    });

    // The void hum starts immediately and never stops.
    let drone = audio.add(wav(drone_loop, 0.7));
    commands.spawn((
        AudioPlayer::new(drone),
        PlaybackSettings::LOOP.with_volume(Volume::Linear(0.16)),
    ));
}

// ---------------------------------------------------------------------------
// Playback
// ---------------------------------------------------------------------------

fn play_queued(mut queue: ResMut<SfxQueue>, sfx: Res<Sfx>, mut commands: Commands) {
    for e in queue.0.drain(..) {
        let (handle, vol, speed) = match e {
            SfxEvent::Break { crystal, pitch } => (
                if crystal { &sfx.crystal_break } else { &sfx.rock_break },
                if crystal { 0.5 } else { 0.42 },
                pitch,
            ),
            SfxEvent::Pickup { pitch } => (&sfx.pickup, 0.32, pitch),
            SfxEvent::Sell => (&sfx.sell, 0.6, 1.0),
            SfxEvent::Buy => (&sfx.buy, 0.5, 1.0),
            SfxEvent::Deny => (&sfx.deny, 0.45, 1.0),
            SfxEvent::Explosion => (&sfx.explosion, 0.9, 1.0),
            SfxEvent::Plant => (&sfx.plant, 0.5, 1.0),
            SfxEvent::Land => (&sfx.land, 0.35, 1.0),
            SfxEvent::WarpUp => (&sfx.warp_up, 0.5, 1.0),
            SfxEvent::WarpDown => (&sfx.warp_down, 0.5, 1.0),
            SfxEvent::Overheat => (&sfx.overheat, 0.5, 1.0),
            SfxEvent::Vent => (&sfx.vent, 0.5, 1.0),
        };
        commands.spawn((
            AudioPlayer::new(handle.clone()),
            PlaybackSettings::DESPAWN
                .with_volume(Volume::Linear(vol))
                .with_speed(speed),
        ));
    }
}

/// Spin the laser hum up/down with the trigger, pitch rising with heat.
fn laser_loop_ctl(
    laser: Res<LaserState>,
    sfx: Res<Sfx>,
    mut handle: Local<Option<Entity>>,
    mut sinks: Query<&mut AudioSink>,
    mut commands: Commands,
) {
    match (*handle, laser.firing) {
        (None, true) => {
            *handle = Some(
                commands
                    .spawn((
                        AudioPlayer::new(sfx.laser_loop.clone()),
                        PlaybackSettings::LOOP.with_volume(Volume::Linear(0.35)),
                    ))
                    .id(),
            );
        }
        (Some(e), true) => {
            // Sink appears a frame or two after spawn; ignore until then.
            if let Ok(sink) = sinks.get_mut(e) {
                sink.set_speed(0.85 + laser.heat * 0.6);
            }
        }
        (Some(e), false) => {
            commands.entity(e).despawn();
            *handle = None;
        }
        (None, false) => {}
    }
}

/// Same idea for the jetpack roar.
fn jet_loop_ctl(
    jet: Res<JetState>,
    sfx: Res<Sfx>,
    mut handle: Local<Option<Entity>>,
    mut sinks: Query<&mut AudioSink>,
    mut commands: Commands,
) {
    match (*handle, jet.0) {
        (None, true) => {
            *handle = Some(
                commands
                    .spawn((
                        AudioPlayer::new(sfx.jet_loop.clone()),
                        PlaybackSettings::LOOP.with_volume(Volume::Linear(0.3)),
                    ))
                    .id(),
            );
        }
        (Some(_), true) => {}
        (Some(e), false) => {
            commands.entity(e).despawn();
            *handle = None;
        }
        (None, false) => {}
    }
    // Quiet the borrow checker about the unused query on the no-op arms.
    let _ = &mut sinks;
}

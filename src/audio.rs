//! All sound, synthesized at startup — zero asset files.
//!
//! Every clip is generated as raw PCM samples and played through rodio
//! DIRECTLY (not bevy_audio, whose one output stream is created at boot,
//! private, and unrecoverable — a Bluetooth headset going to sleep used to
//! kill all audio until restart). We own the `OutputStream`, and a watchdog
//! polls the OS default output device: when it changes (headset sleeps,
//! speakers unplugged, device comes back), the stream is rebuilt and the
//! persistent loops respawn on the new device within ~a second.
//!
//! Gameplay systems never touch audio APIs directly: they push `SfxEvent`s
//! into the `SfxQueue` resource and this module drains it. Loops (laser hum,
//! jetpack, ambient drone, black-hole dread rumble) are driven from small
//! state resources published by the player/blackhole modules.

use bevy::prelude::*;
use rodio::buffer::SamplesBuffer;
use rodio::{OutputStream, OutputStreamHandle, Sink, Source};
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
    /// The dive begins — a long dread riser.
    Collapse,
    /// Crossing the horizon — sub-bass annihilation.
    Consumed,
    /// A new day — quiet, warm, alive.
    Dawn,
    /// The star detonates — the day's opening thunderclap.
    Supernova,
}

#[derive(Resource, Default)]
pub struct SfxQueue(pub Vec<SfxEvent>);

impl SfxQueue {
    pub fn push(&mut self, e: SfxEvent) {
        self.0.push(e);
    }
}

// ---------------------------------------------------------------------------
// Output device + clips
// ---------------------------------------------------------------------------

/// A synthesized mono clip, shared cheaply between plays.
#[derive(Clone)]
struct Clip(Arc<Vec<f32>>);

impl Clip {
    fn source(&self) -> SamplesBuffer<f32> {
        SamplesBuffer::new(1, RATE, self.0.as_slice().to_vec())
    }
}

/// Our own rodio output. The `OutputStream` is leaked (same trick bevy_audio
/// uses: the stream is !Send, the handle isn't) — one small leak per device
/// change, which is rare. `device_name` is what the watchdog diffs against.
#[derive(Resource)]
struct AudioOut {
    handle: Option<OutputStreamHandle>,
    device_name: Option<String>,
}

fn default_device_name() -> Option<String> {
    use rodio::cpal::traits::{DeviceTrait, HostTrait};
    rodio::cpal::default_host()
        .default_output_device()
        .and_then(|d| d.name().ok())
}

impl AudioOut {
    fn open() -> Self {
        match OutputStream::try_default() {
            Ok((stream, handle)) => {
                core::mem::forget(stream);
                Self {
                    handle: Some(handle),
                    device_name: default_device_name(),
                }
            }
            Err(_) => Self {
                handle: None,
                device_name: default_device_name(),
            },
        }
    }

    /// Fire-and-forget one-shot.
    fn play(&self, clip: &Clip, volume: f32, speed: f32) {
        let Some(handle) = &self.handle else { return };
        if let Ok(sink) = Sink::try_new(handle) {
            sink.set_volume(volume);
            sink.set_speed(speed);
            sink.append(clip.source());
            sink.detach();
        }
    }

    /// A persistent looping sink (caller keeps it to retune volume/speed).
    fn start_loop(&self, clip: &Clip, volume: f32) -> Option<Sink> {
        let handle = self.handle.as_ref()?;
        let sink = Sink::try_new(handle).ok()?;
        sink.set_volume(volume);
        sink.append(clip.source().repeat_infinite());
        Some(sink)
    }
}

/// The always-running loops. Laser/jet are started on demand by their ctl
/// systems; drone + dread live for the whole session. All of them are dropped
/// and rebuilt by the watchdog when the output device changes.
#[derive(Resource, Default)]
struct Loops {
    drone: Option<Sink>,
    dread: Option<Sink>,
    laser: Option<Sink>,
    jet: Option<Sink>,
}

impl Loops {
    fn stop_all(&mut self) {
        // Dropping a Sink stops it (the old ones are on a dead stream anyway).
        self.drone = None;
        self.dread = None;
        self.laser = None;
        self.jet = None;
    }
}

// ---------------------------------------------------------------------------
// Plugin
// ---------------------------------------------------------------------------

pub struct SoundPlugin;

impl Plugin for SoundPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<SfxQueue>()
            .init_resource::<Loops>()
            .add_systems(Startup, setup_sfx)
            .add_systems(
                Update,
                (
                    audio_watchdog,
                    play_queued,
                    laser_loop_ctl,
                    jet_loop_ctl,
                    dread_loop_ctl,
                ),
            );
    }
}

#[derive(Resource)]
struct Sfx {
    laser_loop: Clip,
    jet_loop: Clip,
    drone_loop: Clip,
    dread_loop: Clip,
    rock_break: Clip,
    crystal_break: Clip,
    pickup: Clip,
    sell: Clip,
    buy: Clip,
    deny: Clip,
    explosion: Clip,
    plant: Clip,
    land: Clip,
    warp_up: Clip,
    warp_down: Clip,
    overheat: Clip,
    vent: Clip,
    collapse: Clip,
    consumed: Clip,
    dawn: Clip,
    supernova: Clip,
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

/// Normalize peak to `peak` and wrap as a shareable clip.
fn clip(mut samples: Vec<f32>, peak: f32) -> Clip {
    let max = samples.iter().fold(1e-6f32, |m, s| m.max(s.abs()));
    let k = peak / max;
    for s in &mut samples {
        *s = (*s * k).clamp(-1.0, 1.0);
    }
    Clip(Arc::new(samples))
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

fn setup_sfx(mut commands: Commands) {
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

    // Black-hole rumble: lowpassed brown noise + a 26Hz throb breathing on a
    // slow LFO. Loops over 8s (all components integer-cycle); volume rides
    // the dread level so it creeps in as the day runs out.
    let mut brown2 = 0.0f32;
    let mut lp4 = 0.0f32;
    let dread_loop = loopify(
        render(8.0, |t, i| {
            brown2 = (brown2 + 0.10 * noise(i)) * 0.997;
            lp4 += 0.06 * (brown2 - lp4);
            let breathe = 0.65 + 0.35 * (TAU * t / 8.0).sin();
            lp4 * 5.0 * breathe
                + 0.45 * (TAU * 26.0 * t).sin() * breathe
                + 0.18 * (TAU * 39.0 * t).sin() * (0.5 + 0.5 * (TAU * 2.0 * t / 8.0).sin())
        }),
        4096,
    );

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

    // Collapse: a 3s dread riser — pitch and density climbing into the boom.
    let mut lp5 = 0.0f32;
    let collapse = render(3.0, |t, i| {
        let k = t / 3.0;
        let f = 55.0 * (9.0f32).powf(k);
        lp5 += (0.05 + 0.4 * k) * (noise(i) - lp5);
        (TAU * f * t).sin() * (0.25 + 0.75 * k)
            + 0.5 * (TAU * f * 1.5 * t).sin() * k * k
            + lp5 * (0.8 + 2.0 * k)
    });

    // Consumed: crossing the horizon. Sub annihilation + a long white wash.
    let mut brown3 = 0.0f32;
    let consumed = render(2.4, |t, i| {
        brown3 = (brown3 + 0.14 * noise(i)) * 0.997;
        0.9 * (TAU * 30.0 * (1.0 - t * 0.18) * t).sin() * (-t * 1.3).exp()
            + brown3 * 7.0 * (-t * 1.8).exp()
            + noise(i) * expd(t, 0.05) * 0.6
    });

    // Supernova: the day's opening thunderclap — first-instant crack, deep
    // descending body, long radiant noise tail.
    let mut brown4 = 0.0f32;
    let mut lp6 = 0.0f32;
    let supernova = render(4.5, |t, i| {
        brown4 = (brown4 + 0.12 * noise(i)) * 0.9975;
        lp6 += 0.10 * (noise(i) - lp6);
        noise(i) * expd(t, 0.03)
            + 0.9 * (TAU * (34.0 - 6.0 * t.min(2.0)) * t).sin() * (-t * 0.9).exp()
            + brown4 * 6.5 * (-t * 0.7).exp()
            + lp6 * 1.4 * (-t * 0.5).exp()
    });

    // Dawn: three soft warm tones blooming out of silence.
    let dawn = render(1.6, |t, _| {
        let note = |f: f32, at: f32| {
            if t < at {
                0.0
            } else {
                (TAU * f * (t - at)).sin() * (1.0 - expd(t - at, 0.04)) * expd(t - at, 0.45)
            }
        };
        note(392.0, 0.0) + 0.8 * note(523.25, 0.25) + 0.7 * note(659.25, 0.5)
    });

    let sfx = Sfx {
        laser_loop: clip(laser_loop, 0.8),
        jet_loop: clip(jet_loop, 0.8),
        drone_loop: clip(drone_loop, 0.7),
        dread_loop: clip(dread_loop, 0.85),
        rock_break: clip(rock_break, 0.85),
        crystal_break: clip(crystal_break, 0.85),
        pickup: clip(pickup, 0.8),
        sell: clip(sell, 0.85),
        buy: clip(buy, 0.8),
        deny: clip(deny, 0.7),
        explosion: clip(explosion, 0.95),
        plant: clip(plant, 0.8),
        land: clip(land, 0.8),
        warp_up: clip(warp_up, 0.8),
        warp_down: clip(warp_down, 0.8),
        overheat: clip(overheat, 0.8),
        vent: clip(vent, 0.8),
        collapse: clip(collapse, 0.9),
        consumed: clip(consumed, 0.95),
        dawn: clip(dawn, 0.8),
        supernova: clip(supernova, 0.95),
    };

    let out = AudioOut::open();
    let mut loops = Loops::default();
    start_persistent_loops(&out, &sfx, &mut loops);

    commands.insert_resource(sfx);
    commands.insert_resource(out);
    commands.insert_resource(loops);
}

/// The void hum and the singularity's rumble start immediately and never
/// stop (the rumble just starts inaudible). Re-run on every device rebuild.
fn start_persistent_loops(out: &AudioOut, sfx: &Sfx, loops: &mut Loops) {
    loops.drone = out.start_loop(&sfx.drone_loop, 0.16);
    loops.dread = out.start_loop(&sfx.dread_loop, 0.0);
}

// ---------------------------------------------------------------------------
// Device watchdog — the reason this module owns its stream
// ---------------------------------------------------------------------------

/// Poll the OS default output device every ~1.5s. When it changes (headset
/// sleeps → fallback to speakers; device returns → follow it back; device
/// gone entirely → silence until one reappears), rebuild the stream and the
/// persistent loops. Laser/jet loops respawn on demand from their ctl
/// systems; in-flight one-shots are simply lost, which nobody notices.
#[cfg(not(target_arch = "wasm32"))]
fn audio_watchdog(
    time: Res<Time>,
    sfx: Option<Res<Sfx>>,
    mut out: Option<ResMut<AudioOut>>,
    mut loops: ResMut<Loops>,
    mut next_poll: Local<f32>,
) {
    let (Some(sfx), Some(out)) = (sfx, out.as_deref_mut()) else {
        return;
    };
    *next_poll -= time.delta_secs();
    if *next_poll > 0.0 {
        return;
    }
    *next_poll = 1.5;
    let name = default_device_name();
    if name == out.device_name {
        return;
    }
    // Default output changed under us: move there.
    *out = AudioOut::open();
    loops.stop_all();
    start_persistent_loops(out, &sfx, &mut loops);
}

/// Browsers route audio through WebAudio, which survives device changes on
/// its own — no watchdog needed (and cpal device enumeration is a stub there).
#[cfg(target_arch = "wasm32")]
fn audio_watchdog() {}

// ---------------------------------------------------------------------------
// Playback
// ---------------------------------------------------------------------------

fn play_queued(mut queue: ResMut<SfxQueue>, sfx: Res<Sfx>, out: Res<AudioOut>) {
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
            SfxEvent::Collapse => (&sfx.collapse, 0.85, 1.0),
            SfxEvent::Consumed => (&sfx.consumed, 0.95, 1.0),
            SfxEvent::Dawn => (&sfx.dawn, 0.55, 1.0),
            SfxEvent::Supernova => (&sfx.supernova, 0.9, 1.0),
        };
        out.play(handle, vol, speed);
    }
}

/// Spin the laser hum up/down with the trigger, pitch rising with heat.
fn laser_loop_ctl(
    laser: Res<LaserState>,
    sfx: Res<Sfx>,
    out: Res<AudioOut>,
    mut loops: ResMut<Loops>,
) {
    match (&loops.laser, laser.firing) {
        (None, true) => {
            loops.laser = out.start_loop(&sfx.laser_loop, 0.35);
        }
        (Some(sink), true) => {
            sink.set_speed(0.85 + laser.heat * 0.6);
        }
        (Some(_), false) => {
            loops.laser = None;
        }
        (None, false) => {}
    }
}

/// Same idea for the jetpack roar.
fn jet_loop_ctl(jet: Res<JetState>, sfx: Res<Sfx>, out: Res<AudioOut>, mut loops: ResMut<Loops>) {
    match (&loops.jet, jet.0) {
        (None, true) => {
            loops.jet = out.start_loop(&sfx.jet_loop, 0.3);
        }
        (Some(_), true) => {}
        (Some(_), false) => {
            loops.jet = None;
        }
        (None, false) => {}
    }
}

/// The rumble loop's volume and pitch ride the published dread level —
/// silence at dawn, chest-cavity throb at the horizon.
fn dread_loop_ctl(bh: Res<crate::blackhole::BlackHole>, loops: Res<Loops>) {
    if let Some(sink) = &loops.dread {
        let d = bh.dread.clamp(0.0, 1.0);
        sink.set_volume(0.65 * d * d);
        sink.set_speed(0.9 + 0.35 * d);
    }
}

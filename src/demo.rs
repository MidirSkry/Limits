//! Scripted demo mode for hands-off verification (native only).
//!
//! `LIMITS_DEMO=1` drives the player through a fixed tour of the open world —
//! admire the sky, lase the home rock to overheat, lift off and FLY to the
//! nearest neighboring asteroid, torpedo it with a plasma charge, then fly
//! home to the depot and sell — saving screenshots into `shots/` at key
//! beats. Combine with `LIMITS_BENCH_EXIT_AFTER=50` for a self-terminating
//! visual smoke test.
//!
//! Input is injected by pressing the real `ButtonInput` resources before the
//! gameplay systems run (this plugin's system is ordered before
//! `GameplaySet`), so the demo exercises the exact code paths a player does.

use bevy::prelude::*;

#[cfg(not(target_arch = "wasm32"))]
use bevy::render::view::window::screenshot::{save_to_disk, Screenshot};

use crate::game::shop_pos;
use crate::player::{Focused, PlayerState};

pub struct DemoPlugin;

impl Plugin for DemoPlugin {
    fn build(&self, app: &mut App) {
        #[cfg(not(target_arch = "wasm32"))]
        if std::env::var("LIMITS_DEMO").is_ok_and(|v| v == "1") {
            let _ = std::fs::create_dir_all("shots");
            app.add_systems(Update, run_demo.before(crate::GameplaySet));
        }
        #[cfg(target_arch = "wasm32")]
        let _ = app;
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Default)]
struct DemoState {
    target: Option<(IVec3, crate::world::Asteroid)>,
    arrived_at: Option<f32>,
    bombed_at: Option<f32>,
    heading_home_at: Option<f32>,
    sold_at: Option<f32>,
    q_fired: bool,
    e_fired: bool,
    shots_done: u32,
}

/// Ease an angle toward a target along the shortest arc. The wrap matters:
/// when the target sits near ±π it flips sign frame-to-frame, and a naive
/// lerp averages the flips to 0 — 180° wrong.
#[cfg(not(target_arch = "wasm32"))]
fn ease_angle(current: f32, target: f32, dt: f32) -> f32 {
    use std::f32::consts::{PI, TAU};
    let mut delta = (target - current) % TAU;
    if delta > PI {
        delta -= TAU;
    } else if delta < -PI {
        delta += TAU;
    }
    current + delta * (dt * 4.0).min(1.0)
}

/// Yaw/pitch that point the camera along `dir`.
#[cfg(not(target_arch = "wasm32"))]
fn aim(dir: Vec3) -> (f32, f32) {
    let yaw = f32::atan2(-dir.x, -dir.z);
    let pitch = (dir.y / dir.length().max(1e-5)).clamp(-1.0, 1.0).asin();
    (yaw, pitch)
}

#[cfg(not(target_arch = "wasm32"))]
#[allow(clippy::too_many_arguments)]
fn run_demo(
    time: Res<Time>,
    mut focused: ResMut<Focused>,
    mut player: ResMut<PlayerState>,
    bh: Res<crate::blackhole::BlackHole>,
    world_res: Res<crate::world::VoxelWorld>,
    mut keys: ResMut<ButtonInput<KeyCode>>,
    mut mouse: ResMut<ButtonInput<MouseButton>>,
    mut state: Local<DemoState>,
    mut commands: Commands,
) {
    let t = time.elapsed_secs();
    let dt = time.delta_secs();
    focused.0 = true;

    // Pick the flight target once: the nearest non-home asteroid.
    if state.target.is_none() {
        if let Some((cell, a)) = crate::world::nearest_asteroid_keyed(player.pos, true) {
            state.target = Some((cell, a));
            eprintln!(
                "[demo] target asteroid at ({:.0},{:.0},{:.0}) r~{:.0} dist {:.0}m",
                a.center.x,
                a.center.y,
                a.center.z,
                a.reach(),
                (a.center - player.pos).length()
            );
        }
    }
    let (target_c, target_dist) = match &state.target {
        Some((cell, a)) => {
            // Bodies MOVE now — chase the displaced position, and measure to
            // the true surface along our approach (reach() overshoots the
            // lumpy ellipsoids by 10m+ and would strand the tour hovering).
            let current = a.center + world_res.offset_of(*cell);
            let to = current - player.eye();
            let surf = a.surface_toward(player.eye() - world_res.offset_of(*cell));
            (current, to.length() - surf)
        }
        None => {
            let c = Vec3::new(60.0, 0.0, 60.0);
            (c, (c - player.eye()).length() - 8.0)
        }
    };
    let to_target = target_c - player.eye();
    let to_shop = shop_pos() + Vec3::Y * 1.0 - player.pos;
    let shop_dist = Vec2::new(to_shop.x, to_shop.z).length();

    // --- Look script -----------------------------------------------------------
    let (yaw_t, pitch_t) = if t < 3.0 {
        (t * 0.55, 0.10) // sky pan
    } else if t < 5.0 {
        // Stare into the black hole — the money shot.
        aim((bh.center - player.eye()).normalize_or_zero())
    } else if t < 11.0 {
        (0.64, -0.95) // mine the ground (long enough to overheat)
    } else if state.bombed_at.is_none() || state.heading_home_at.is_none() {
        // Aim at the neighbor rock: for the flight, the charge throw, and
        // watching the boom.
        aim(to_target.normalize_or_zero())
    } else {
        // Homeward: look at the depot.
        aim((shop_pos() + Vec3::Y * 1.0 - player.eye()).normalize_or_zero())
    };
    player.yaw = ease_angle(player.yaw, yaw_t, dt);
    player.pitch = ease_angle(player.pitch, pitch_t, dt);

    // --- Inputs ------------------------------------------------------------------
    mouse.release(MouseButton::Left);
    keys.release(KeyCode::KeyW);
    keys.release(KeyCode::KeyS);
    keys.release(KeyCode::Space);
    keys.release(KeyCode::ShiftLeft);

    if (5.0..10.5).contains(&t) {
        // Mine the home rock at our feet.
        mouse.press(MouseButton::Left);
    } else if (11.0..50.0).contains(&t) && state.arrived_at.is_none() {
        // FLY: lift off, then thrust along the look ray toward the target.
        if t < 12.0 {
            keys.press(KeyCode::Space);
        }
        if target_dist > 3.0 {
            keys.press(KeyCode::KeyW);
            // Don't slam into it: bleed speed on final approach.
            if target_dist < 12.0 && player.vel.length() > 8.0 {
                keys.press(KeyCode::ShiftLeft);
            }
        } else {
            keys.press(KeyCode::ShiftLeft); // brake at the rock
            if player.vel.length() < 1.0 {
                state.arrived_at = Some(t);
                eprintln!("[demo] arrived at neighbor t={t:.1}");
            }
        }
    } else if let Some(arrived) = state.arrived_at {
        if state.bombed_at.is_none() {
            // Hover-mine the face for a moment, then torpedo it.
            if t < arrived + 2.0 {
                mouse.press(MouseButton::Left);
                keys.press(KeyCode::ShiftLeft);
            } else if !state.q_fired {
                state.q_fired = true;
                keys.release(KeyCode::KeyQ);
                keys.press(KeyCode::KeyQ);
                state.bombed_at = Some(t);
                eprintln!("[demo] charge away t={t:.1}");
            }
        } else if let Some(bombed) = state.bombed_at {
            if t < bombed + 1.6 {
                keys.press(KeyCode::KeyS); // back off and watch
            } else if t < bombed + 3.4 {
                keys.press(KeyCode::ShiftLeft); // hold position for the boom
            } else if state.heading_home_at.is_none() {
                state.heading_home_at = Some(t);
                eprintln!("[demo] heading home t={t:.1}");
            } else if shop_dist > 1.2 {
                keys.press(KeyCode::KeyW);
                // Bleed speed close-in so we don't faceplant the pad.
                if to_shop.length() < 12.0 && player.vel.length() > 6.0 {
                    keys.press(KeyCode::ShiftLeft);
                }
            } else if !state.e_fired {
                state.e_fired = true;
                state.sold_at = Some(t);
                keys.release(KeyCode::KeyE);
                keys.press(KeyCode::KeyE);
                eprintln!("[demo] sold t={t:.1}");
            }
        }
    }
    if !state.q_fired || state.bombed_at.is_some_and(|b| t > b + 0.1) {
        keys.release(KeyCode::KeyQ);
    }
    if state.e_fired && state.sold_at.is_some_and(|s| t > s + 0.1) {
        keys.release(KeyCode::KeyE);
    }

    // Once-per-second breadcrumb so a failed tour can be reconstructed.
    if (t * 10.0) as u32 % 10 == 0 && (t * 10.0).fract() < dt * 10.0 {
        eprintln!(
            "[demo] t={t:.0} pos=({:.1},{:.1},{:.1}) yaw={:.2} v={:.1} grounded={}",
            player.pos.x,
            player.pos.y,
            player.pos.z,
            player.yaw,
            player.vel.length(),
            player.grounded
        );
    }

    // --- Screenshots ---------------------------------------------------------
    // Fixed beats early; event-relative beats once flight timing is real.
    // Shot in declared order: the next one fires when its condition is true.
    let conds: [(bool, &'static str); 8] = [
        (t >= 2.2, "shots/01-sky.png"),
        (t >= 4.5, "shots/02-blackhole.png"),
        (t >= 6.5, "shots/03-laser.png"),
        (t >= 10.4, "shots/04-overheat.png"),
        (t >= 14.5, "shots/05-flight.png"),
        (
            state.arrived_at.is_some_and(|a| t >= a + 1.0),
            "shots/06-neighbor.png",
        ),
        (
            state.bombed_at.is_some_and(|b| t >= b + 2.4),
            "shots/07-boom.png",
        ),
        (
            state.sold_at.is_some_and(|s| t >= s + 0.7)
                || state.heading_home_at.is_some_and(|h| t >= h + 14.0),
            "shots/08-depot.png",
        ),
    ];
    if let Some(&(cond, name)) = conds.get(state.shots_done as usize) {
        if cond {
            state.shots_done += 1;
            commands
                .spawn(Screenshot::primary_window())
                .observe(save_to_disk(name));
        }
    }
}

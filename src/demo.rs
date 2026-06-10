//! Scripted demo mode for hands-off verification (native only).
//!
//! `LIMITS_DEMO=1` drives the player through a fixed tour — admire the sky,
//! lase the ground, set off a plasma charge, jetpack out, visit the depot —
//! and saves screenshots into `shots/` at key beats. Combine with
//! `LIMITS_BENCH_EXIT_AFTER=26` for a self-terminating visual smoke test.
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
    shots_taken: u32,
    q_fired: bool,
    e_fired: bool,
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

#[cfg(not(target_arch = "wasm32"))]
#[allow(clippy::too_many_arguments)]
fn run_demo(
    time: Res<Time>,
    mut focused: ResMut<Focused>,
    mut player: ResMut<PlayerState>,
    mut keys: ResMut<ButtonInput<KeyCode>>,
    mut mouse: ResMut<ButtonInput<MouseButton>>,
    mut state: Local<DemoState>,
    mut commands: Commands,
) {
    let t = time.elapsed_secs();
    let dt = time.delta_secs();
    focused.0 = true;

    // Where the depot is, for the final walk. Horizontal distance only —
    // a falling player passing over the pad still counts as arriving.
    let to_shop = shop_pos() - player.pos;
    let shop_dist = Vec2::new(to_shop.x, to_shop.z).length();
    let shop_yaw = f32::atan2(-to_shop.x, -to_shop.z);

    // --- The tour script, keyed on elapsed seconds --------------------------
    let (yaw_t, pitch_t) = if t < 3.0 {
        // Slow pan across the sky.
        (t * 0.55, 0.10)
    } else if t < 5.0 {
        // Face the gas giant (it sits along -X,-Z at modest elevation).
        (0.64, 0.18)
    } else if t < 12.0 {
        // Look at the ground in front and mine (long enough to overheat).
        (0.64, -0.95)
    } else if t < 16.2 {
        // Aim straight down: the plasma charge goes under our own feet.
        (0.64, -1.40)
    } else if t < 19.5 {
        // We fell into the crater — pan around the glowing walls.
        ((t - 16.2) * 0.8 + 0.64, -0.15)
    } else if t < 23.0 {
        // Jetpack out, drifting toward the depot so we land shop-side.
        (shop_yaw, -0.05)
    } else {
        // Walk to the depot.
        (shop_yaw, -0.25)
    };
    player.yaw = ease_angle(player.yaw, yaw_t, dt);
    player.pitch = ease_angle(player.pitch, pitch_t, dt);

    // Trigger + movement per phase.
    mouse.release(MouseButton::Left);
    keys.release(KeyCode::KeyW);
    keys.release(KeyCode::KeyS);
    keys.release(KeyCode::Space);

    match t {
        t if (5.0..13.5).contains(&t) => {
            mouse.press(MouseButton::Left);
            // Shuffle forward onto fresh rock now and then.
            if (8.0..8.6).contains(&t) || (11.0..11.5).contains(&t) {
                keys.press(KeyCode::KeyW);
            }
        }
        t if (14.0..14.1).contains(&t) => {
            if !state.q_fired {
                state.q_fired = true;
                keys.release(KeyCode::KeyQ);
                keys.press(KeyCode::KeyQ);
            }
        }
        t if (19.5..23.0).contains(&t) => {
            keys.press(KeyCode::Space); // jetpack out of the crater...
            if t >= 21.0 {
                keys.press(KeyCode::KeyW); // ...drifting toward the rim
            }
        }
        t if t >= 23.0 => {
            if shop_dist > 1.2 {
                keys.press(KeyCode::KeyW);
                // Blocked, or fell into a crater? Hop/jet until clear.
                // Only below surface level, so this can't ride the rim wall.
                let slow = Vec2::new(player.vel.x, player.vel.z).length() < 0.6;
                if slow && (player.grounded || player.pos.y < -0.3) {
                    keys.press(KeyCode::Space);
                }
            } else if t >= 26.2 && !state.e_fired {
                // Arrived: sell the hold once.
                state.e_fired = true;
                keys.release(KeyCode::KeyE);
                keys.press(KeyCode::KeyE);
            }
        }
        _ => {}
    }
    if state.e_fired && !(26.2..26.3).contains(&t) {
        keys.release(KeyCode::KeyE);
    }

    // Once-per-second breadcrumb so a failed tour can be reconstructed from
    // the log. Demo-only diagnostic; the game itself never prints.
    if (t * 10.0) as u32 % 10 == 0 && (t * 10.0).fract() < dt * 10.0 {
        eprintln!(
            "[demo] t={t:.0} pos=({:.1},{:.1},{:.1}) yaw={:.2} grounded={}",
            player.pos.x, player.pos.y, player.pos.z, player.yaw, player.grounded
        );
    }
    if !(14.0..14.1).contains(&t) {
        keys.release(KeyCode::KeyQ);
    }

    // --- Screenshots at fixed beats ------------------------------------------
    const SHOTS: [(f32, &str); 8] = [
        (2.2, "shots/01-surface-sky.png"),
        (4.5, "shots/02-planet.png"),
        (6.5, "shots/03-laser.png"),
        (10.4, "shots/04-overheat.png"),
        (16.45, "shots/05-explosion.png"),
        (18.5, "shots/06-crater.png"),
        (22.0, "shots/07-jetpack.png"),
        (27.3, "shots/08-depot-sell.png"),
    ];
    if (state.shots_taken as usize) < SHOTS.len() {
        let (at, path) = SHOTS[state.shots_taken as usize];
        if t >= at {
            state.shots_taken += 1;
            commands
                .spawn(Screenshot::primary_window())
                .observe(save_to_disk(path));
        }
    }
}

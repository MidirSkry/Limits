//! Incremental economy: credits, cargo hold, upgrade ladder, the surface
//! depot, and the recall/dive teleports that close the dig→sell→upgrade loop.

use bevy::prelude::*;
use std::collections::BTreeMap;

use crate::audio::{SfxEvent, SfxQueue};
use crate::player::PlayerState;
use crate::world::loot_value;
use std::sync::OnceLock;

// ---------------------------------------------------------------------------
// Tunables — the entire progression curve lives in these constants.
// ---------------------------------------------------------------------------

/// Laser damage per second. Continuous beam, so this is the whole story —
/// roughly: base regolith (3 HP) melts in a third of a second.
const BASE_DPS: f32 = 9.0;
const DPS_GROWTH: f32 = 1.5;
/// Heat added per second of continuous fire / removed per second idle.
const BASE_HEAT_RATE: f32 = 0.20;
const HEAT_RATE_SHRINK: f32 = 0.85;
const BASE_COOL_RATE: f32 = 0.45;
const COOL_RATE_GROWTH: f32 = 1.12;
/// Tractor field: how far drops magnet toward the player.
const BASE_MAGNET_RANGE: f32 = 2.4;
const MAGNET_RANGE_STEP: f32 = 0.8;

const POWER_COST: f64 = 12.0;
const POWER_COST_GROWTH: f64 = 1.65;
const COOLANT_COST: f64 = 20.0;
const COOLANT_COST_GROWTH: f64 = 1.7;
const TRACTOR_COST: f64 = 35.0;
const TRACTOR_COST_GROWTH: f64 = 1.9;
pub const RECALL_COST: u64 = 250;
const PLASMA_PACK_COST: f64 = 60.0;
const PLASMA_PACK_COST_GROWTH: f64 = 1.25;
/// Charges per plasma pack purchase.
pub const PLASMA_PACK_SIZE: u32 = 3;

/// How close (m) the player must be to the depot pad to trade.
const SHOP_RADIUS: f32 = 2.8;
const STATUS_TTL: f32 = 2.5;

// ---------------------------------------------------------------------------
// Resources
// ---------------------------------------------------------------------------

#[derive(Resource, Default)]
pub struct Wallet {
    pub credits: u64,
}

#[derive(Resource)]
pub struct Upgrades {
    pub power_lvl: u32,
    pub coolant_lvl: u32,
    pub tractor_lvl: u32,
    pub recall: bool,
    /// Plasma charges in the hold (consumable).
    pub charges: u32,
    pub packs_bought: u32,
}

impl Default for Upgrades {
    fn default() -> Self {
        Self {
            power_lvl: 0,
            coolant_lvl: 0,
            tractor_lvl: 0,
            recall: false,
            // Start with a couple of charges so the player discovers the boom.
            charges: 2,
            packs_bought: 0,
        }
    }
}

impl Upgrades {
    pub fn dps(&self) -> f32 {
        BASE_DPS * DPS_GROWTH.powi(self.power_lvl as i32)
    }
    pub fn heat_rate(&self) -> f32 {
        BASE_HEAT_RATE * HEAT_RATE_SHRINK.powi(self.coolant_lvl as i32)
    }
    pub fn cool_rate(&self) -> f32 {
        BASE_COOL_RATE * COOL_RATE_GROWTH.powi(self.coolant_lvl as i32)
    }
    pub fn magnet_range(&self) -> f32 {
        BASE_MAGNET_RANGE + MAGNET_RANGE_STEP * self.tractor_lvl as f32
    }
    pub fn magnet_accel(&self) -> f32 {
        40.0 * (1.0 + 0.3 * self.tractor_lvl as f32)
    }

    pub fn power_cost(&self) -> u64 {
        (POWER_COST * POWER_COST_GROWTH.powi(self.power_lvl as i32)).round() as u64
    }
    pub fn coolant_cost(&self) -> u64 {
        (COOLANT_COST * COOLANT_COST_GROWTH.powi(self.coolant_lvl as i32)).round() as u64
    }
    pub fn tractor_cost(&self) -> u64 {
        (TRACTOR_COST * TRACTOR_COST_GROWTH.powi(self.tractor_lvl as i32)).round() as u64
    }
    pub fn plasma_cost(&self) -> u64 {
        (PLASMA_PACK_COST * PLASMA_PACK_COST_GROWTH.powi(self.packs_bought as i32)).round()
            as u64
    }
}

/// Loot hold, keyed by (block id, band) — which together determine the
/// display name and unit value. Everything mined ends up here, spoil included.
#[derive(Resource, Default)]
pub struct Inventory {
    pub stacks: BTreeMap<(u8, i32), u32>,
    pub units: u32,
}

impl Inventory {
    pub fn add_n(&mut self, id: u8, band: i32, n: u32) {
        *self.stacks.entry((id, band)).or_insert(0) += n;
        self.units += n;
    }
    pub fn total_value(&self) -> u64 {
        self.stacks
            .iter()
            .map(|(&(id, band), &count)| loot_value(id, band) * count as u64)
            .sum()
    }
    pub fn clear(&mut self) {
        self.stacks.clear();
        self.units = 0;
    }
}

/// Transient HUD status line ("Sold 14 crystals for 260 cr", ...).
#[derive(Resource, Default)]
pub struct StatusMsg {
    pub text: String,
    pub ttl: f32,
}

impl StatusMsg {
    pub fn set(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.ttl = STATUS_TTL;
    }
}

/// True while the player stands close enough to the depot pad to trade.
#[derive(Resource, Default)]
pub struct NearShop(pub bool);

/// The depot pad sits on the home rock's surface a short walk from spawn.
/// Cached: the surface scan runs worldgen noise and callers hit this per
/// frame.
pub fn shop_pos() -> Vec3 {
    static POS: OnceLock<Vec3> = OnceLock::new();
    *POS.get_or_init(|| Vec3::new(4.5, crate::world::surface_y_at(4.5, 0.0), 0.0))
}

// ---------------------------------------------------------------------------
// Plugin
// ---------------------------------------------------------------------------

pub struct GamePlugin;

impl Plugin for GamePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Wallet>()
            .init_resource::<Upgrades>()
            .init_resource::<Inventory>()
            .init_resource::<StatusMsg>()
            .init_resource::<NearShop>()
            .add_systems(Startup, setup_shop)
            .add_systems(
                Update,
                (
                    (shop_system, teleport_system).in_set(crate::GameplaySet),
                    status_decay,
                    spin_holo,
                ),
            );
    }
}

#[derive(Component)]
struct HoloSpin {
    rate: f32,
    base_y: f32,
}

fn setup_shop(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    let pos = shop_pos();

    // Landing-pad base: dark metal slab.
    commands.spawn((
        Mesh3d(meshes.add(Cuboid::new(2.2, 0.25, 2.2))),
        MeshMaterial3d(materials.add(StandardMaterial {
            base_color: Color::srgb(0.16, 0.18, 0.22),
            perceptual_roughness: 0.5,
            metallic: 0.8,
            ..default()
        })),
        Transform::from_translation(pos + Vec3::Y * 0.125),
    ));

    // Holographic trade beacon: a glowing core...
    commands.spawn((
        Mesh3d(meshes.add(Sphere::new(0.22))),
        MeshMaterial3d(materials.add(StandardMaterial {
            base_color: Color::linear_rgb(1.0, 2.0, 2.4),
            unlit: true,
            ..default()
        })),
        Transform::from_translation(pos + Vec3::Y * 2.4),
        HoloSpin {
            rate: 0.0,
            base_y: 2.4,
        },
    ));
    // ...two slow counter-rotating holo rings...
    let ring_mat = materials.add(StandardMaterial {
        base_color: Color::linear_rgba(0.4, 2.0, 2.6, 0.55),
        unlit: true,
        alpha_mode: AlphaMode::Add,
        cull_mode: None,
        ..default()
    });
    let ring = meshes.add(crate::sky::ring_mesh(0.42, 0.55, 48, |_| {
        [1.0, 1.0, 1.0, 1.0]
    }));
    for (rate, y) in [(0.9f32, 2.4f32), (-0.6, 2.65)] {
        commands.spawn((
            Mesh3d(ring.clone()),
            MeshMaterial3d(ring_mat.clone()),
            Transform::from_translation(pos + Vec3::Y * y),
            HoloSpin { rate, base_y: y },
        ));
    }
    // ...a faint light pillar marking the pad from across the claim...
    commands.spawn((
        Mesh3d(meshes.add(Cuboid::new(0.5, 14.0, 0.5))),
        MeshMaterial3d(materials.add(StandardMaterial {
            base_color: Color::linear_rgba(0.10, 0.55, 0.70, 0.10),
            unlit: true,
            alpha_mode: AlphaMode::Add,
            cull_mode: None,
            ..default()
        })),
        Transform::from_translation(pos + Vec3::Y * 7.0),
    ));
    // ...and the actual light it casts on the ground.
    commands.spawn((
        PointLight {
            color: Color::srgb(0.45, 0.9, 1.0),
            intensity: 400_000.0,
            range: 10.0,
            shadows_enabled: false,
            ..default()
        },
        Transform::from_translation(pos + Vec3::Y * 2.0),
    ));
}

/// Idle animation for the depot hologram: rings spin, core bobs.
fn spin_holo(time: Res<Time>, mut q: Query<(&HoloSpin, &mut Transform)>) {
    let t = time.elapsed_secs();
    for (spin, mut tf) in &mut q {
        if spin.rate != 0.0 {
            tf.rotation = Quat::from_rotation_y(t * spin.rate)
                * Quat::from_rotation_x((t * spin.rate * 0.5).sin() * 0.25);
        }
        tf.translation.y = shop_pos().y + spin.base_y + (t * 1.3).sin() * 0.05;
    }
}

fn shop_system(
    keys: Res<ButtonInput<KeyCode>>,
    player: Res<PlayerState>,
    mut near: ResMut<NearShop>,
    mut wallet: ResMut<Wallet>,
    mut inventory: ResMut<Inventory>,
    mut upgrades: ResMut<Upgrades>,
    mut status: ResMut<StatusMsg>,
    mut sfx: ResMut<SfxQueue>,
    mut commands: Commands,
) {
    near.0 = player.pos.distance(shop_pos()) <= SHOP_RADIUS;
    if !near.0 {
        return;
    }

    if keys.just_pressed(KeyCode::KeyE) {
        let value = inventory.total_value();
        let units = inventory.units;
        if units == 0 {
            status.set("Hold's empty — go carve some rock!");
            sfx.push(SfxEvent::Deny);
        } else {
            wallet.credits += value;
            inventory.clear();
            status.set(format!("Sold {units} units for {value} cr"));
            sfx.push(SfxEvent::Sell);
            crate::hud::spawn_float_text(
                &mut commands,
                shop_pos() + Vec3::new(0.0, 1.9, 0.0),
                format!("+{value} cr"),
                Color::srgb(0.5, 1.0, 0.6),
                30.0,
                1.5,
            );
        }
    }

    let try_buy = |cost: u64,
                       wallet: &mut Wallet,
                       status: &mut StatusMsg,
                       sfx: &mut SfxQueue,
                       what: &str|
     -> bool {
        if wallet.credits >= cost {
            wallet.credits -= cost;
            sfx.push(SfxEvent::Buy);
            true
        } else {
            status.set(format!("Need {cost} cr for {what}"));
            sfx.push(SfxEvent::Deny);
            false
        }
    };

    if keys.just_pressed(KeyCode::Digit1) {
        let cost = upgrades.power_cost();
        if try_buy(cost, &mut wallet, &mut status, &mut sfx, "the laser upgrade") {
            upgrades.power_lvl += 1;
            status.set(format!("Laser output → {:.0} DPS", upgrades.dps()));
        }
    }
    if keys.just_pressed(KeyCode::Digit2) {
        let cost = upgrades.coolant_cost();
        if try_buy(cost, &mut wallet, &mut status, &mut sfx, "the coolant loop") {
            upgrades.coolant_lvl += 1;
            status.set(format!(
                "Coolant loop {} — beam runs {:.0}% longer",
                upgrades.coolant_lvl,
                (1.0 / upgrades.heat_rate() / (1.0 / BASE_HEAT_RATE) - 1.0) * 100.0
            ));
        }
    }
    if keys.just_pressed(KeyCode::Digit3) {
        let cost = upgrades.tractor_cost();
        if try_buy(cost, &mut wallet, &mut status, &mut sfx, "the tractor field") {
            upgrades.tractor_lvl += 1;
            status.set(format!(
                "Tractor field → {:.1}m reach",
                upgrades.magnet_range()
            ));
        }
    }
    if keys.just_pressed(KeyCode::Digit4) && !upgrades.recall {
        if try_buy(RECALL_COST, &mut wallet, &mut status, &mut sfx, "the recall rig") {
            upgrades.recall = true;
            status.set("Recall rig online — [T] surface, [G] dive to depth");
        }
    }
    if keys.just_pressed(KeyCode::Digit5) {
        let cost = upgrades.plasma_cost();
        if try_buy(cost, &mut wallet, &mut status, &mut sfx, "plasma charges") {
            upgrades.charges += PLASMA_PACK_SIZE;
            upgrades.packs_bought += 1;
            status.set(format!(
                "+{PLASMA_PACK_SIZE} plasma charges ({} in hold)",
                upgrades.charges
            ));
        }
    }
}

fn teleport_system(
    keys: Res<ButtonInput<KeyCode>>,
    upgrades: Res<Upgrades>,
    mut player: ResMut<PlayerState>,
    mut status: ResMut<StatusMsg>,
    mut sfx: ResMut<SfxQueue>,
) {
    if !upgrades.recall {
        return;
    }
    if keys.just_pressed(KeyCode::KeyT) {
        player.pos = PlayerState::spawn_point();
        player.vel = Vec3::ZERO;
        status.set("Recalled to the home rock");
        sfx.push(SfxEvent::WarpUp);
    }
    if keys.just_pressed(KeyCode::KeyG) && player.max_range > 25.0 {
        player.pos = player.far_pos;
        player.vel = Vec3::ZERO;
        status.set(format!("Jumped to furthest site — {:.0}m out", player.max_range));
        sfx.push(SfxEvent::WarpDown);
    }
}

fn status_decay(time: Res<Time>, mut status: ResMut<StatusMsg>) {
    if status.ttl > 0.0 {
        status.ttl -= time.delta_secs();
    }
}

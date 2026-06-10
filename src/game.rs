//! Incremental economy: wallet, limited inventory, upgrade ladder, the surface
//! shop, and the recall/dive teleports that close the dig→sell→upgrade loop.

use bevy::prelude::*;
use std::collections::BTreeMap;

use crate::player::PlayerState;
use crate::world::{loot_value, VOXEL, WORLD_VOXELS_XZ};

// ---------------------------------------------------------------------------
// Tunables — the entire progression curve lives in these constants.
// ---------------------------------------------------------------------------

const BASE_DAMAGE: f32 = 4.0;
const DAMAGE_GROWTH: f32 = 1.5;
const BASE_SWINGS: f32 = 2.0;
const SWINGS_GROWTH: f32 = 1.12;
const BASE_CAPACITY: u32 = 16;
const CAPACITY_PER_LEVEL: u32 = 8;

const DAMAGE_COST: f64 = 12.0;
const DAMAGE_COST_GROWTH: f64 = 1.65;
const SWINGS_COST: f64 = 20.0;
const SWINGS_COST_GROWTH: f64 = 1.7;
const CAPACITY_COST: f64 = 30.0;
const CAPACITY_COST_GROWTH: f64 = 1.8;
pub const RECALL_COST: u64 = 200;

/// How close (m) the player must be to the shop pad to trade.
const SHOP_RADIUS: f32 = 2.8;
const STATUS_TTL: f32 = 2.5;

// ---------------------------------------------------------------------------
// Resources
// ---------------------------------------------------------------------------

#[derive(Resource, Default)]
pub struct Wallet {
    pub money: u64,
}

#[derive(Resource, Default)]
pub struct Upgrades {
    pub damage_lvl: u32,
    pub swings_lvl: u32,
    pub capacity_lvl: u32,
    pub recall: bool,
}

impl Upgrades {
    pub fn damage(&self) -> f32 {
        BASE_DAMAGE * DAMAGE_GROWTH.powi(self.damage_lvl as i32)
    }
    pub fn swings_per_sec(&self) -> f32 {
        BASE_SWINGS * SWINGS_GROWTH.powi(self.swings_lvl as i32)
    }
    pub fn capacity(&self) -> u32 {
        BASE_CAPACITY + CAPACITY_PER_LEVEL * self.capacity_lvl
    }
    pub fn damage_cost(&self) -> u64 {
        (DAMAGE_COST * DAMAGE_COST_GROWTH.powi(self.damage_lvl as i32)).round() as u64
    }
    pub fn swings_cost(&self) -> u64 {
        (SWINGS_COST * SWINGS_COST_GROWTH.powi(self.swings_lvl as i32)).round() as u64
    }
    pub fn capacity_cost(&self) -> u64 {
        (CAPACITY_COST * CAPACITY_COST_GROWTH.powi(self.capacity_lvl as i32)).round() as u64
    }
}

/// Loot inventory, keyed by (block id, band) — which together determine the
/// display name and unit value. Everything mined ends up here, dirt included.
#[derive(Resource, Default)]
pub struct Inventory {
    pub stacks: BTreeMap<(u8, i32), u32>,
    pub units: u32,
}

impl Inventory {
    pub fn add(&mut self, id: u8, band: i32) {
        *self.stacks.entry((id, band)).or_insert(0) += 1;
        self.units += 1;
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

/// Transient HUD status line ("Pack full!", "Sold 14 ore for $260", ...).
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

/// True while the player stands close enough to the shop pad to trade.
#[derive(Resource, Default)]
pub struct NearShop(pub bool);

pub fn shop_pos() -> Vec3 {
    let c = WORLD_VOXELS_XZ as f32 * VOXEL * 0.5;
    Vec3::new(c + 3.0, 0.0, c)
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
            .add_systems(Update, (shop_system, teleport_system, status_decay));
    }
}

fn setup_shop(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    let pos = shop_pos();
    // The shop pad: a glowing gold block you can spot from across the claim.
    commands.spawn((
        Mesh3d(meshes.add(Cuboid::new(1.2, 0.8, 1.2))),
        MeshMaterial3d(materials.add(StandardMaterial {
            base_color: Color::srgb(0.9, 0.75, 0.2),
            emissive: LinearRgba::rgb(1.8, 1.2, 0.2),
            ..default()
        })),
        Transform::from_translation(pos + Vec3::Y * 0.4),
    ));
    commands.spawn((
        PointLight {
            color: Color::srgb(1.0, 0.85, 0.4),
            intensity: 400_000.0,
            range: 10.0,
            shadows_enabled: false,
            ..default()
        },
        Transform::from_translation(pos + Vec3::Y * 2.0),
    ));
}

fn shop_system(
    keys: Res<ButtonInput<KeyCode>>,
    player: Res<PlayerState>,
    mut near: ResMut<NearShop>,
    mut wallet: ResMut<Wallet>,
    mut inventory: ResMut<Inventory>,
    mut upgrades: ResMut<Upgrades>,
    mut status: ResMut<StatusMsg>,
) {
    near.0 = player.pos.distance(shop_pos()) <= SHOP_RADIUS;
    if !near.0 {
        return;
    }

    if keys.just_pressed(KeyCode::KeyE) {
        let value = inventory.total_value();
        let units = inventory.units;
        if units == 0 {
            status.set("Nothing to sell — go dig!");
        } else {
            wallet.money += value;
            inventory.clear();
            status.set(format!("Sold {units} items for ${value}"));
        }
    }

    let try_buy = |cost: u64, wallet: &mut Wallet| -> bool {
        if wallet.money >= cost {
            wallet.money -= cost;
            true
        } else {
            false
        }
    };

    if keys.just_pressed(KeyCode::Digit1) {
        let cost = upgrades.damage_cost();
        if try_buy(cost, &mut wallet) {
            upgrades.damage_lvl += 1;
            status.set(format!("Pickaxe damage → {:.0}", upgrades.damage()));
        } else {
            status.set(format!("Need ${cost} for damage upgrade"));
        }
    }
    if keys.just_pressed(KeyCode::Digit2) {
        let cost = upgrades.swings_cost();
        if try_buy(cost, &mut wallet) {
            upgrades.swings_lvl += 1;
            status.set(format!("Swing speed → {:.2}/s", upgrades.swings_per_sec()));
        } else {
            status.set(format!("Need ${cost} for speed upgrade"));
        }
    }
    if keys.just_pressed(KeyCode::Digit3) {
        let cost = upgrades.capacity_cost();
        if try_buy(cost, &mut wallet) {
            upgrades.capacity_lvl += 1;
            status.set(format!("Pack capacity → {}", upgrades.capacity()));
        } else {
            status.set(format!("Need ${cost} for pack upgrade"));
        }
    }
    if keys.just_pressed(KeyCode::Digit4) && !upgrades.recall {
        if try_buy(RECALL_COST, &mut wallet) {
            upgrades.recall = true;
            status.set("Recall device online — [T] surface, [G] dive to depth");
        } else {
            status.set(format!("Need ${RECALL_COST} for the recall device"));
        }
    }
}

fn teleport_system(
    keys: Res<ButtonInput<KeyCode>>,
    upgrades: Res<Upgrades>,
    mut player: ResMut<PlayerState>,
    mut status: ResMut<StatusMsg>,
) {
    if !upgrades.recall {
        return;
    }
    if keys.just_pressed(KeyCode::KeyT) {
        player.pos = PlayerState::spawn_point();
        player.vel = Vec3::ZERO;
        status.set("Recalled to the surface");
    }
    if keys.just_pressed(KeyCode::KeyG) && player.max_depth > 1.0 {
        player.pos = player.deepest_pos;
        player.vel = Vec3::ZERO;
        status.set(format!("Dove back to {:.1}m", player.max_depth));
    }
}

fn status_decay(time: Res<Time>, mut status: ResMut<StatusMsg>) {
    if status.ttl > 0.0 {
        status.ttl -= time.delta_secs();
    }
}

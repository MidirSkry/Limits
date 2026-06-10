//! HUD: stats panel, crosshair + target block readout, shop panel, status line.
//! All text — a handful of UI entities, so per-frame `format!` here is fine.

use bevy::prelude::*;

use crate::game::{Inventory, NearShop, StatusMsg, Upgrades, Wallet, RECALL_COST};
use crate::player::{Focused, PlayerState, TargetInfo};
use crate::world::loot_name;

#[derive(Component)]
struct StatsText;
#[derive(Component)]
struct TargetText;
#[derive(Component)]
struct ShopText;
#[derive(Component)]
struct StatusText;

pub struct HudPlugin;

impl Plugin for HudPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, setup_hud).add_systems(
            Update,
            (hud_stats, hud_target, hud_shop, hud_status),
        );
    }
}

fn setup_hud(mut commands: Commands) {
    let font = |size: f32| TextFont {
        font_size: size,
        ..default()
    };

    // Top-left: money / depth / pack / tool stats.
    commands.spawn((
        Text::new(""),
        font(15.0),
        TextColor(Color::WHITE),
        Node {
            position_type: PositionType::Absolute,
            top: Val::Px(10.0),
            left: Val::Px(10.0),
            ..default()
        },
        StatsText,
    ));

    // Center: crosshair, with the target-block readout just below it.
    commands
        .spawn(Node {
            position_type: PositionType::Absolute,
            width: Val::Percent(100.0),
            height: Val::Percent(100.0),
            flex_direction: FlexDirection::Column,
            align_items: AlignItems::Center,
            justify_content: JustifyContent::Center,
            ..default()
        })
        .with_children(|parent| {
            parent.spawn((Text::new("+"), font(20.0), TextColor(Color::WHITE)));
            parent.spawn((
                Text::new(""),
                font(14.0),
                TextColor(Color::srgb(0.95, 0.9, 0.7)),
                TargetText,
            ));
        });

    // Top-right: shop panel (only visible near the pad).
    commands.spawn((
        Text::new(""),
        font(15.0),
        TextColor(Color::srgb(1.0, 0.9, 0.55)),
        Node {
            position_type: PositionType::Absolute,
            top: Val::Px(10.0),
            right: Val::Px(10.0),
            ..default()
        },
        Visibility::Hidden,
        ShopText,
    ));

    // Bottom-center: transient status line.
    commands
        .spawn(Node {
            position_type: PositionType::Absolute,
            bottom: Val::Px(48.0),
            width: Val::Percent(100.0),
            justify_content: JustifyContent::Center,
            ..default()
        })
        .with_children(|parent| {
            parent.spawn((
                Text::new(""),
                font(17.0),
                TextColor(Color::srgb(1.0, 0.8, 0.4)),
                StatusText,
            ));
        });
}

fn hud_stats(
    wallet: Res<Wallet>,
    inventory: Res<Inventory>,
    upgrades: Res<Upgrades>,
    player: Res<PlayerState>,
    focused: Res<Focused>,
    mut q: Query<&mut Text, With<StatsText>>,
) {
    let Ok(mut text) = q.single_mut() else { return };

    let mut s = format!(
        "${money}\n\
         Depth: {depth:.1}m   (best {best:.1}m)\n\
         Pack:  {units}/{cap}\n\
         Pick:  {dmg:.0} dmg @ {spd:.2}/s\n",
        money = wallet.money,
        depth = player.depth_m(),
        best = player.max_depth,
        units = inventory.units,
        cap = upgrades.capacity(),
        dmg = upgrades.damage(),
        spd = upgrades.swings_per_sec(),
    );
    // Show the most valuable few stacks so the pack readout stays compact.
    for (&(id, band), &count) in inventory.stacks.iter().rev().take(4) {
        s.push_str(&format!("  {} x{}\n", loot_name(id, band), count));
    }
    s.push('\n');
    if focused.0 {
        s.push_str("WASD move  Space jump  LMB mine  Q drop TNT  Esc release mouse");
        if upgrades.recall {
            s.push_str("\n[T] surface   [G] dive to best depth");
        }
    } else {
        s.push_str("CLICK TO PLAY");
    }
    text.0 = s;
}

fn hud_target(target: Res<TargetInfo>, mut q: Query<&mut Text, With<TargetText>>) {
    let Ok(mut text) = q.single_mut() else { return };
    match &target.0 {
        Some(t) if t.indestructible => {
            text.0 = format!("\n{} — indestructible", t.name);
        }
        Some(t) => {
            // Ten-segment HP bar, drawn with text so we need zero art assets.
            let filled = (t.hp_frac * 10.0).ceil() as usize;
            let mut bar = String::with_capacity(32);
            for i in 0..10 {
                bar.push(if i < filled { '█' } else { '░' });
            }
            if t.value > 0 {
                text.0 = format!("\n{} (${})\n{}", t.name, t.value, bar);
            } else {
                text.0 = format!("\n{}\n{}", t.name, bar);
            }
        }
        None => text.0.clear(),
    }
}

fn hud_shop(
    near: Res<NearShop>,
    wallet: Res<Wallet>,
    inventory: Res<Inventory>,
    upgrades: Res<Upgrades>,
    mut q: Query<(&mut Text, &mut Visibility), With<ShopText>>,
) {
    let Ok((mut text, mut vis)) = q.single_mut() else {
        return;
    };
    if !near.0 {
        *vis = Visibility::Hidden;
        return;
    }
    *vis = Visibility::Visible;

    let tag = |cost: u64| -> &'static str {
        if wallet.money >= cost {
            ""
        } else {
            "  (can't afford)"
        }
    };
    let recall_line = if upgrades.recall {
        "[4] Recall device — OWNED".to_string()
    } else {
        format!("[4] Recall device  ${}{}", RECALL_COST, tag(RECALL_COST))
    };
    text.0 = format!(
        "SHOP\n\
         [E] Sell pack  (+${sell})\n\
         [1] Damage {dmg:.0} → {dmg_next:.0}   ${dmg_cost}{t1}\n\
         [2] Speed {spd:.2} → {spd_next:.2}   ${spd_cost}{t2}\n\
         [3] Pack {cap} → {cap_next}   ${cap_cost}{t3}\n\
         {recall_line}",
        sell = inventory.total_value(),
        dmg = upgrades.damage(),
        dmg_next = upgrades.damage() * 1.5,
        dmg_cost = upgrades.damage_cost(),
        t1 = tag(upgrades.damage_cost()),
        spd = upgrades.swings_per_sec(),
        spd_next = upgrades.swings_per_sec() * 1.12,
        spd_cost = upgrades.swings_cost(),
        t2 = tag(upgrades.swings_cost()),
        cap = upgrades.capacity(),
        cap_next = upgrades.capacity() + 8,
        cap_cost = upgrades.capacity_cost(),
        t3 = tag(upgrades.capacity_cost()),
    );
}

fn hud_status(status: Res<StatusMsg>, mut q: Query<&mut Text, With<StatusText>>) {
    let Ok(mut text) = q.single_mut() else { return };
    if status.ttl > 0.0 {
        text.0.clone_from(&status.text);
    } else {
        text.0.clear();
    }
}

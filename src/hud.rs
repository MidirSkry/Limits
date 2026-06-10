//! HUD + floating combat text.
//!
//! Graphical, not textual: procedurally-baked pixel icons (no asset files),
//! stat chips, a real heat bar, plasma pips, loot-color swatches for the
//! hold, a depot panel built from key-chips + level dots + cost pills, and a
//! screen-edge nav icon pointing home. Numbers stay as text — numbers are
//! what text is for. The controls hint only exists while unfocused.

use bevy::image::{Image, ImageSampler};
use bevy::prelude::*;
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat};

use crate::game::{shop_pos, Inventory, NearShop, StatusMsg, Upgrades, Wallet, RECALL_COST};
use crate::player::{Focused, LaserState, PlayerState, TargetInfo};
use crate::world::{loot_color, TIER_M};

// ---------------------------------------------------------------------------
// Palette
// ---------------------------------------------------------------------------

const PANEL_BG: Color = Color::srgba(0.03, 0.05, 0.08, 0.80);
const PANEL_BORDER: Color = Color::srgba(0.25, 0.65, 0.80, 0.45);
const CREDITS: Color = Color::srgb(0.55, 1.0, 0.85);
const CYAN: Color = Color::srgb(0.45, 0.9, 1.0);
const MAGENTA: Color = Color::srgb(0.9, 0.4, 1.0);
const GOLD: Color = Color::srgb(1.0, 0.84, 0.30);
const TEXT_DIM: Color = Color::srgb(0.75, 0.84, 0.90);
const TEXT_FAINT: Color = Color::srgb(0.50, 0.60, 0.68);
const BAR_BG: Color = Color::srgba(0.0, 0.0, 0.0, 0.55);
const AFFORD: Color = Color::srgb(0.55, 1.0, 0.75);
const TOO_RICH: Color = Color::srgb(0.55, 0.58, 0.64);
const PILL_OK_BG: Color = Color::srgba(0.10, 0.35, 0.22, 0.9);
const PILL_NO_BG: Color = Color::srgba(0.12, 0.14, 0.18, 0.9);

// ---------------------------------------------------------------------------
// Pixel icons — 12x12 glyphs baked into Images at startup, tinted at use.
// ---------------------------------------------------------------------------

type Glyph = [&'static str; 12];

const ICON_GEM: Glyph = [
    "............",
    ".....##.....",
    "....####....",
    "...######...",
    "..########..",
    ".##########.",
    ".##########.",
    "..########..",
    "...######...",
    "....####....",
    ".....##.....",
    "............",
];
const ICON_HOME: Glyph = [
    ".....##.....",
    "....####....",
    "...##..##...",
    "..##....##..",
    ".##......##.",
    ".##########.",
    "..#......#..",
    "..#..##..#..",
    "..#..##..#..",
    "..#..##..#..",
    "..########..",
    "............",
];
const ICON_CRATE: Glyph = [
    "............",
    ".##########.",
    ".#...##...#.",
    ".#...##...#.",
    ".#...##...#.",
    ".##########.",
    ".#...##...#.",
    ".#...##...#.",
    ".#...##...#.",
    ".##########.",
    "............",
    "............",
];
const ICON_SPEED: Glyph = [
    "............",
    ".#....#.....",
    ".##...##....",
    "..##...##...",
    "...##...##..",
    "....##...##.",
    "....##...##.",
    "...##...##..",
    "..##...##...",
    ".##...##....",
    ".#....#.....",
    "............",
];
const ICON_BOLT: Glyph = [
    "......##....",
    ".....###....",
    "....###.....",
    "...###......",
    "..######....",
    ".....###....",
    "....###.....",
    "...###......",
    "..######....",
    ".....##.....",
    "....##......",
    "............",
];
const ICON_SNOW: Glyph = [
    ".....##.....",
    ".##..##..##.",
    "..##.##.##..",
    "...######...",
    ".####..####.",
    "...##..##...",
    ".####..####.",
    "...######...",
    "..##.##.##..",
    ".##..##..##.",
    ".....##.....",
    "............",
];
const ICON_MAGNET: Glyph = [
    "............",
    "..###..###..",
    "..###..###..",
    "..##....##..",
    "..##....##..",
    "..##....##..",
    "..##....##..",
    "...##..##...",
    "....####....",
    ".....##.....",
    "............",
    "............",
];
const ICON_BEACON: Glyph = [
    ".....##.....",
    "....####....",
    "...##..##...",
    "..#..##..#..",
    ".....##.....",
    ".....##.....",
    ".....##.....",
    "....####....",
    "...######...",
    "..########..",
    "............",
    "............",
];
const ICON_ORB: Glyph = [
    "............",
    "....####....",
    "..########..",
    "..########..",
    ".####..####.",
    ".####..####.",
    "..########..",
    "..########..",
    "....####....",
    "............",
    "............",
    "............",
];
const ICON_FLAG: Glyph = [
    "..#.........",
    "..########..",
    "..########..",
    "..#######...",
    "..######....",
    "..#.........",
    "..#.........",
    "..#.........",
    "..#.........",
    "..#.........",
    "..#.........",
    "............",
];

fn bake_icon(images: &mut Assets<Image>, glyph: Glyph) -> Handle<Image> {
    let (w, h) = (12usize, 12usize);
    let mut data = vec![0u8; w * h * 4];
    for (y, row) in glyph.iter().enumerate() {
        for (x, ch) in row.bytes().enumerate() {
            let a = match ch {
                b'#' => 255,
                b'+' => 120,
                _ => 0,
            };
            let i = (y * w + x) * 4;
            data[i] = 255;
            data[i + 1] = 255;
            data[i + 2] = 255;
            data[i + 3] = a;
        }
    }
    let mut img = Image::new(
        Extent3d {
            width: w as u32,
            height: h as u32,
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        data,
        TextureFormat::Rgba8UnormSrgb,
        RenderAssetUsages::RENDER_WORLD,
    );
    img.sampler = ImageSampler::nearest();
    images.add(img)
}

use bevy::asset::RenderAssetUsages;

#[derive(Resource)]
struct Icons {
    gem: Handle<Image>,
    home: Handle<Image>,
    crate_: Handle<Image>,
    speed: Handle<Image>,
    bolt: Handle<Image>,
    snow: Handle<Image>,
    magnet: Handle<Image>,
    beacon: Handle<Image>,
    orb: Handle<Image>,
    flag: Handle<Image>,
}

/// An icon as a UI node, tinted.
fn icon(handle: &Handle<Image>, size: f32, tint: Color) -> impl Bundle {
    (
        ImageNode {
            image: handle.clone(),
            color: tint,
            ..default()
        },
        Node {
            width: Val::Px(size),
            height: Val::Px(size),
            ..default()
        },
    )
}

// ---------------------------------------------------------------------------
// Markers
// ---------------------------------------------------------------------------

#[derive(Component)]
struct CreditsText;
#[derive(Component)]
struct RangeText;
#[derive(Component)]
struct BestText;
#[derive(Component)]
struct SectorText;
#[derive(Component)]
struct SpeedText;
#[derive(Component)]
struct HoldCountText;
/// One of the three hold-content slots: the swatch square + its count text.
#[derive(Component)]
struct HoldSwatch(usize);
#[derive(Component)]
struct HoldSwatchText(usize);
#[derive(Component)]
struct TargetPanel;
#[derive(Component)]
struct TargetNameText;
#[derive(Component)]
struct HpBarFill;
#[derive(Component)]
struct HeatBar;
#[derive(Component)]
struct HeatBarFill;
#[derive(Component)]
struct HeatWarnText;
/// Plasma pip circles, in order.
#[derive(Component)]
struct ChargePip(u32);
#[derive(Component)]
struct ChargeOverflowText;
#[derive(Component)]
struct ShopPanel;
#[derive(Component)]
struct ShopName(usize);
#[derive(Component)]
struct ShopDot {
    row: usize,
    idx: u32,
}
#[derive(Component)]
struct ShopPill(usize);
#[derive(Component)]
struct ShopPillText(usize);
#[derive(Component)]
struct StatusPanel;
#[derive(Component)]
struct StatusText;
#[derive(Component)]
struct ControlsText;
#[derive(Component)]
struct NavMarker;
#[derive(Component)]
struct NavDistText;

/// World-anchored floating text. Lives on a UI Text node; the animate system
/// projects `world_pos` to the screen each frame and fades it out.
#[derive(Component)]
struct FloatText {
    world_pos: Vec3,
    age: f32,
    ttl: f32,
    rise: f32,
}

pub struct HudPlugin;

impl Plugin for HudPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, setup_hud).add_systems(
            Update,
            (
                hud_stats,
                hud_hold,
                hud_target,
                hud_heat,
                hud_shop,
                hud_status,
                hud_controls,
                hud_nav_marker,
                animate_float_text,
            ),
        );
    }
}

#[inline]
fn font(size: f32) -> TextFont {
    TextFont {
        font_size: size,
        ..default()
    }
}

fn chip_node() -> Node {
    Node {
        padding: UiRect::axes(Val::Px(10.0), Val::Px(6.0)),
        border: UiRect::all(Val::Px(1.5)),
        align_items: AlignItems::Center,
        column_gap: Val::Px(7.0),
        border_radius: BorderRadius::all(Val::Px(9.0)),
        ..default()
    }
}

// ---------------------------------------------------------------------------
// Layout
// ---------------------------------------------------------------------------

fn setup_hud(mut commands: Commands, mut images: ResMut<Assets<Image>>) {
    let icons = Icons {
        gem: bake_icon(&mut images, ICON_GEM),
        home: bake_icon(&mut images, ICON_HOME),
        crate_: bake_icon(&mut images, ICON_CRATE),
        speed: bake_icon(&mut images, ICON_SPEED),
        bolt: bake_icon(&mut images, ICON_BOLT),
        snow: bake_icon(&mut images, ICON_SNOW),
        magnet: bake_icon(&mut images, ICON_MAGNET),
        beacon: bake_icon(&mut images, ICON_BEACON),
        orb: bake_icon(&mut images, ICON_ORB),
        flag: bake_icon(&mut images, ICON_FLAG),
    };

    // --- Credits chip (top-left) ---------------------------------------------
    commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                top: Val::Px(10.0),
                left: Val::Px(10.0),
                ..chip_node()
            },
            BackgroundColor(PANEL_BG),
            BorderColor::all(PANEL_BORDER),
        ))
        .with_children(|c| {
            c.spawn(icon(&icons.gem, 16.0, CREDITS));
            c.spawn((Text::new("0"), font(22.0), TextColor(CREDITS), CreditsText));
        });

    // --- Range / best / sector chip (top-center) ------------------------------
    commands
        .spawn(Node {
            position_type: PositionType::Absolute,
            top: Val::Px(10.0),
            width: Val::Percent(100.0),
            justify_content: JustifyContent::Center,
            ..default()
        })
        .with_children(|row| {
            row.spawn((
                chip_node(),
                BackgroundColor(PANEL_BG),
                BorderColor::all(PANEL_BORDER),
            ))
            .with_children(|c| {
                c.spawn(icon(&icons.home, 15.0, TEXT_DIM));
                c.spawn((Text::new("0m"), font(16.0), TextColor(TEXT_DIM), RangeText));
                c.spawn(Node {
                    width: Val::Px(10.0),
                    ..default()
                });
                c.spawn(icon(&icons.flag, 14.0, TEXT_FAINT));
                c.spawn((Text::new("0m"), font(16.0), TextColor(TEXT_FAINT), BestText));
                c.spawn(Node {
                    width: Val::Px(10.0),
                    ..default()
                });
                // Sector badge: a bordered circle with the numeral inside.
                c.spawn((
                    Node {
                        width: Val::Px(24.0),
                        height: Val::Px(24.0),
                        border: UiRect::all(Val::Px(1.5)),
                        align_items: AlignItems::Center,
                        justify_content: JustifyContent::Center,
                        border_radius: BorderRadius::all(Val::Px(12.0)),
                        ..default()
                    },
                    BorderColor::all(CYAN),
                    BackgroundColor(Color::srgba(0.05, 0.15, 0.20, 0.9)),
                ))
                .with_children(|b| {
                    b.spawn((Text::new("I"), font(12.0), TextColor(CYAN), SectorText));
                });
            });
        });

    // --- Speed chip (top-right) ------------------------------------------------
    commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                top: Val::Px(10.0),
                right: Val::Px(10.0),
                ..chip_node()
            },
            BackgroundColor(PANEL_BG),
            BorderColor::all(PANEL_BORDER),
        ))
        .with_children(|c| {
            c.spawn(icon(&icons.speed, 15.0, CYAN));
            c.spawn((Text::new("0.0"), font(18.0), TextColor(CYAN), SpeedText));
        });

    // --- Hold chip (bottom-left) -----------------------------------------------
    commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                bottom: Val::Px(12.0),
                left: Val::Px(12.0),
                ..chip_node()
            },
            BackgroundColor(PANEL_BG),
            BorderColor::all(PANEL_BORDER),
        ))
        .with_children(|c| {
            c.spawn(icon(&icons.crate_, 16.0, TEXT_DIM));
            c.spawn((
                Text::new("0"),
                font(17.0),
                TextColor(TEXT_DIM),
                HoldCountText,
            ));
            for i in 0..3 {
                c.spawn(Node {
                    width: Val::Px(8.0),
                    ..default()
                });
                c.spawn((
                    Node {
                        width: Val::Px(11.0),
                        height: Val::Px(11.0),
                        border_radius: BorderRadius::all(Val::Px(3.0)),
                        ..default()
                    },
                    BackgroundColor(Color::NONE),
                    HoldSwatch(i),
                ));
                c.spawn((
                    Text::new(""),
                    font(14.0),
                    TextColor(TEXT_FAINT),
                    HoldSwatchText(i),
                ));
            }
        });

    // --- Crosshair: four ticks + center dot -------------------------------------
    commands
        .spawn(Node {
            position_type: PositionType::Absolute,
            width: Val::Percent(100.0),
            height: Val::Percent(100.0),
            align_items: AlignItems::Center,
            justify_content: JustifyContent::Center,
            ..default()
        })
        .with_children(|p| {
            p.spawn(Node {
                width: Val::Px(28.0),
                height: Val::Px(28.0),
                ..default()
            })
            .with_children(|x| {
                let tick = |x: &mut ChildSpawnerCommands, w: f32, h: f32, l: f32, t: f32| {
                    x.spawn((
                        Node {
                            position_type: PositionType::Absolute,
                            width: Val::Px(w),
                            height: Val::Px(h),
                            left: Val::Px(l),
                            top: Val::Px(t),
                            ..default()
                        },
                        BackgroundColor(Color::srgba(0.7, 1.0, 1.0, 0.85)),
                    ));
                };
                tick(x, 2.0, 7.0, 13.0, 0.0); // top
                tick(x, 2.0, 7.0, 13.0, 21.0); // bottom
                tick(x, 7.0, 2.0, 0.0, 13.0); // left
                tick(x, 7.0, 2.0, 21.0, 13.0); // right
                tick(x, 2.0, 2.0, 13.0, 13.0); // dot
            });
        });

    // --- Heat bar + plasma pips (under the reticle) ------------------------------
    commands
        .spawn(Node {
            position_type: PositionType::Absolute,
            top: Val::Percent(53.0),
            width: Val::Percent(100.0),
            flex_direction: FlexDirection::Column,
            align_items: AlignItems::Center,
            row_gap: Val::Px(5.0),
            ..default()
        })
        .with_children(|col| {
            col.spawn((
                Node {
                    width: Val::Px(140.0),
                    height: Val::Px(7.0),
                    border: UiRect::all(Val::Px(1.0)),
                    border_radius: BorderRadius::all(Val::Px(3.0)),
                    ..default()
                },
                BackgroundColor(BAR_BG),
                BorderColor::all(Color::srgba(0.0, 0.0, 0.0, 0.6)),
                Visibility::Hidden,
                HeatBar,
            ))
            .with_children(|bar| {
                bar.spawn((
                    Node {
                        width: Val::Percent(0.0),
                        height: Val::Percent(100.0),
                        border_radius: BorderRadius::all(Val::Px(2.0)),
                        ..default()
                    },
                    BackgroundColor(CYAN),
                    HeatBarFill,
                ));
            });
            col.spawn((
                Text::new("OVERHEAT — VENTING"),
                font(13.0),
                TextColor(Color::srgb(1.0, 0.35, 0.25)),
                Visibility::Hidden,
                HeatWarnText,
            ));
            col.spawn(Node {
                column_gap: Val::Px(4.0),
                align_items: AlignItems::Center,
                ..default()
            })
            .with_children(|pips| {
                for i in 0..10u32 {
                    pips.spawn((
                        Node {
                            width: Val::Px(8.0),
                            height: Val::Px(8.0),
                            border: UiRect::all(Val::Px(1.0)),
                            border_radius: BorderRadius::all(Val::Px(4.0)),
                            ..default()
                        },
                        BorderColor::all(Color::srgba(0.9, 0.4, 1.0, 0.5)),
                        BackgroundColor(Color::NONE),
                        Visibility::Hidden,
                        ChargePip(i),
                    ));
                }
                pips.spawn((
                    Text::new(""),
                    font(13.0),
                    TextColor(MAGENTA),
                    ChargeOverflowText,
                ));
            });
        });

    // --- Target panel (below the heat cluster) --------------------------------
    commands
        .spawn(Node {
            position_type: PositionType::Absolute,
            top: Val::Percent(59.0),
            width: Val::Percent(100.0),
            justify_content: JustifyContent::Center,
            ..default()
        })
        .with_children(|row| {
            row.spawn((
                Node {
                    flex_direction: FlexDirection::Column,
                    align_items: AlignItems::Center,
                    padding: UiRect::axes(Val::Px(12.0), Val::Px(7.0)),
                    border: UiRect::all(Val::Px(1.0)),
                    row_gap: Val::Px(5.0),
                    border_radius: BorderRadius::all(Val::Px(8.0)),
                    ..default()
                },
                BackgroundColor(PANEL_BG),
                BorderColor::all(PANEL_BORDER),
                Visibility::Hidden,
                TargetPanel,
            ))
            .with_children(|t| {
                t.spawn((
                    Text::new(""),
                    font(16.0),
                    TextColor(Color::WHITE),
                    TargetNameText,
                ));
                t.spawn((
                    Node {
                        width: Val::Px(190.0),
                        height: Val::Px(12.0),
                        border: UiRect::all(Val::Px(1.0)),
                        border_radius: BorderRadius::all(Val::Px(3.0)),
                        ..default()
                    },
                    BackgroundColor(BAR_BG),
                    BorderColor::all(Color::srgba(0.0, 0.0, 0.0, 0.7)),
                ))
                .with_children(|bar| {
                    bar.spawn((
                        Node {
                            width: Val::Percent(100.0),
                            height: Val::Percent(100.0),
                            border_radius: BorderRadius::all(Val::Px(2.0)),
                            ..default()
                        },
                        BackgroundColor(Color::srgb(0.3, 0.85, 0.95)),
                        HpBarFill,
                    ));
                });
            });
        });

    // --- Depot panel: one row per item ------------------------------------------
    let shop_icons = [
        &icons.crate_, // sell
        &icons.bolt,
        &icons.snow,
        &icons.magnet,
        &icons.beacon,
        &icons.orb,
    ];
    let shop_keys = ["E", "1", "2", "3", "4", "5"];
    commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                top: Val::Percent(50.0),
                right: Val::Px(16.0),
                margin: UiRect::top(Val::Px(-160.0)),
                flex_direction: FlexDirection::Column,
                width: Val::Px(390.0),
                padding: UiRect::all(Val::Px(14.0)),
                border: UiRect::all(Val::Px(1.5)),
                row_gap: Val::Px(8.0),
                border_radius: BorderRadius::all(Val::Px(10.0)),
                ..default()
            },
            BackgroundColor(Color::srgba(0.02, 0.06, 0.08, 0.88)),
            BorderColor::all(CYAN),
            Visibility::Hidden,
            ShopPanel,
        ))
        .with_children(|p| {
            p.spawn((Text::new("SUPPLY DEPOT"), font(19.0), TextColor(CYAN)));
            for (i, (ic, key)) in shop_icons.iter().zip(shop_keys).enumerate() {
                p.spawn(Node {
                    align_items: AlignItems::Center,
                    column_gap: Val::Px(8.0),
                    ..default()
                })
                .with_children(|row| {
                    // Key chip.
                    row.spawn((
                        Node {
                            width: Val::Px(22.0),
                            height: Val::Px(22.0),
                            border: UiRect::all(Val::Px(1.0)),
                            align_items: AlignItems::Center,
                            justify_content: JustifyContent::Center,
                            border_radius: BorderRadius::all(Val::Px(5.0)),
                            ..default()
                        },
                        BorderColor::all(TEXT_FAINT),
                        BackgroundColor(Color::srgba(0.10, 0.13, 0.18, 0.9)),
                    ))
                    .with_children(|k| {
                        k.spawn((Text::new(key), font(12.0), TextColor(TEXT_DIM)));
                    });
                    row.spawn(icon(ic, 16.0, CYAN));
                    row.spawn((
                        Text::new(""),
                        font(15.0),
                        TextColor(TEXT_DIM),
                        ShopName(i),
                    ));
                    // Level dots (rows 1..=3 use them).
                    row.spawn(Node {
                        column_gap: Val::Px(3.0),
                        align_items: AlignItems::Center,
                        flex_grow: 1.0,
                        justify_content: JustifyContent::FlexEnd,
                        ..default()
                    })
                    .with_children(|dots| {
                        for d in 0..5u32 {
                            dots.spawn((
                                Node {
                                    width: Val::Px(7.0),
                                    height: Val::Px(7.0),
                                    border: UiRect::all(Val::Px(1.0)),
                                    border_radius: BorderRadius::all(Val::Px(4.0)),
                                    ..default()
                                },
                                BorderColor::all(Color::srgba(0.4, 0.7, 0.8, 0.5)),
                                BackgroundColor(Color::NONE),
                                Visibility::Hidden,
                                ShopDot { row: i, idx: d },
                            ));
                        }
                    });
                    // Cost pill.
                    row.spawn((
                        Node {
                            padding: UiRect::axes(Val::Px(9.0), Val::Px(3.0)),
                            border_radius: BorderRadius::all(Val::Px(9.0)),
                            align_items: AlignItems::Center,
                            ..default()
                        },
                        BackgroundColor(PILL_NO_BG),
                        ShopPill(i),
                    ))
                    .with_children(|pill| {
                        pill.spawn((
                            Text::new(""),
                            font(14.0),
                            TextColor(TOO_RICH),
                            ShopPillText(i),
                        ));
                    });
                });
            }
        });

    // --- Status toast (bottom-center) ---------------------------------------
    commands
        .spawn(Node {
            position_type: PositionType::Absolute,
            bottom: Val::Px(64.0),
            width: Val::Percent(100.0),
            justify_content: JustifyContent::Center,
            ..default()
        })
        .with_children(|row| {
            row.spawn((
                Node {
                    padding: UiRect::axes(Val::Px(16.0), Val::Px(8.0)),
                    border: UiRect::all(Val::Px(1.0)),
                    border_radius: BorderRadius::all(Val::Px(16.0)),
                    ..default()
                },
                BackgroundColor(Color::srgba(0.03, 0.06, 0.09, 0.9)),
                BorderColor::all(PANEL_BORDER),
                Visibility::Hidden,
                StatusPanel,
            ))
            .with_children(|t| {
                t.spawn((
                    Text::new(""),
                    font(17.0),
                    TextColor(Color::srgb(0.75, 0.95, 1.0)),
                    StatusText,
                ));
            });
        });

    // --- Controls hint: ONLY while unfocused ----------------------------------
    commands
        .spawn(Node {
            position_type: PositionType::Absolute,
            bottom: Val::Px(40.0),
            width: Val::Percent(100.0),
            justify_content: JustifyContent::Center,
            ..default()
        })
        .with_children(|row| {
            row.spawn((
                Text::new(""),
                font(14.0),
                TextColor(TEXT_FAINT),
                ControlsText,
            ));
        });

    // --- Depot nav marker (screen-space, repositioned every frame) ------------
    commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                flex_direction: FlexDirection::Column,
                align_items: AlignItems::Center,
                row_gap: Val::Px(1.0),
                ..default()
            },
            Visibility::Hidden,
            NavMarker,
        ))
        .with_children(|m| {
            m.spawn(icon(&icons.home, 18.0, Color::srgba(0.45, 0.9, 1.0, 0.9)));
            m.spawn((
                Text::new(""),
                font(12.0),
                TextColor(Color::srgba(0.45, 0.9, 1.0, 0.8)),
                NavDistText,
            ));
        });

    commands.insert_resource(icons);
}

// ---------------------------------------------------------------------------
// Update systems
// ---------------------------------------------------------------------------

/// Group a number with thousands separators: 12345 -> "12,345".
fn commas(n: u64) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let len = bytes.len();
    let mut out = String::with_capacity(len + len / 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (len - i) % 3 == 0 {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

fn roman(n: i32) -> String {
    const R: [&str; 10] = ["I", "II", "III", "IV", "V", "VI", "VII", "VIII", "IX", "X"];
    if (1..=10).contains(&n) {
        R[(n - 1) as usize].to_string()
    } else {
        n.to_string()
    }
}

#[allow(clippy::type_complexity)]
fn hud_stats(
    wallet: Res<Wallet>,
    player: Res<PlayerState>,
    mut q: ParamSet<(
        Query<&mut Text, With<CreditsText>>,
        Query<&mut Text, With<RangeText>>,
        Query<&mut Text, With<BestText>>,
        Query<&mut Text, With<SectorText>>,
        Query<&mut Text, With<SpeedText>>,
    )>,
) {
    if let Ok(mut t) = q.p0().single_mut() {
        t.0 = commas(wallet.credits);
    }
    if let Ok(mut t) = q.p1().single_mut() {
        t.0 = format!("{:.0}m", player.range_m());
    }
    if let Ok(mut t) = q.p2().single_mut() {
        t.0 = format!("{:.0}m", player.max_range);
    }
    if let Ok(mut t) = q.p3().single_mut() {
        t.0 = roman((player.range_m() / TIER_M) as i32 + 1);
    }
    if let Ok(mut t) = q.p4().single_mut() {
        t.0 = format!("{:.1}", player.vel.length());
    }
}

#[allow(clippy::type_complexity)]
fn hud_hold(
    inventory: Res<Inventory>,
    mut count_q: Query<&mut Text, (With<HoldCountText>, Without<HoldSwatchText>)>,
    mut swatches: Query<(&HoldSwatch, &mut BackgroundColor)>,
    mut swatch_texts: Query<(&HoldSwatchText, &mut Text), Without<HoldCountText>>,
) {
    if let Ok(mut t) = count_q.single_mut() {
        t.0 = format!("{} · {}", inventory.units, commas(inventory.total_value()));
    }
    // Top three stacks as color swatches + counts.
    let top: Vec<((u8, i32), u32)> = inventory
        .stacks
        .iter()
        .rev()
        .take(3)
        .map(|(&k, &v)| (k, v))
        .collect();
    for (sw, mut bg) in &mut swatches {
        bg.0 = match top.get(sw.0) {
            Some(&((id, tier), _)) => {
                let c = loot_color(id, tier);
                Color::linear_rgb(c[0] * 1.6, c[1] * 1.6, c[2] * 1.6)
            }
            None => Color::NONE,
        };
    }
    for (st, mut text) in &mut swatch_texts {
        text.0 = match top.get(st.0) {
            Some(&(_, count)) => format!("{count}"),
            None => String::new(),
        };
    }
}

fn hud_target(
    target: Res<TargetInfo>,
    mut panel_q: Query<&mut Visibility, With<TargetPanel>>,
    mut name_q: Query<&mut Text, With<TargetNameText>>,
    mut fill_q: Query<(&mut Node, &mut BackgroundColor), With<HpBarFill>>,
) {
    let Ok(mut vis) = panel_q.single_mut() else {
        return;
    };
    let Some(t) = &target.0 else {
        *vis = Visibility::Hidden;
        return;
    };
    *vis = Visibility::Visible;

    if let Ok(mut name) = name_q.single_mut() {
        name.0 = if t.indestructible {
            format!("{} — impervious", t.name)
        } else if t.value > 0 {
            format!("{}  ({} cr)", t.name, commas(t.value))
        } else {
            t.name.clone()
        };
    }
    if let Ok((mut node, mut color)) = fill_q.single_mut() {
        let frac = if t.indestructible { 1.0 } else { t.hp_frac };
        node.width = Val::Percent(frac * 100.0);
        color.0 = if t.indestructible {
            Color::srgb(0.5, 0.55, 0.62)
        } else {
            Color::srgb(0.3 + (1.0 - frac) * 0.7, 0.85, 0.95)
        };
    }
}

#[allow(clippy::type_complexity)]
fn hud_heat(
    laser: Res<LaserState>,
    upgrades: Res<Upgrades>,
    mut bar_q: Query<&mut Visibility, (With<HeatBar>, Without<HeatWarnText>, Without<ChargePip>)>,
    mut fill_q: Query<(&mut Node, &mut BackgroundColor), With<HeatBarFill>>,
    mut warn_q: Query<&mut Visibility, (With<HeatWarnText>, Without<HeatBar>, Without<ChargePip>)>,
    mut pips: Query<(&ChargePip, &mut Visibility, &mut BackgroundColor), (Without<HeatBar>, Without<HeatWarnText>, Without<HeatBarFill>)>,
    mut overflow_q: Query<&mut Text, With<ChargeOverflowText>>,
) {
    if let Ok(mut vis) = bar_q.single_mut() {
        *vis = if laser.heat > 0.02 {
            Visibility::Visible
        } else {
            Visibility::Hidden
        };
    }
    if let Ok((mut node, mut color)) = fill_q.single_mut() {
        node.width = Val::Percent(laser.heat * 100.0);
        let h = laser.heat;
        color.0 = if laser.locked {
            Color::srgb(1.0, 0.3, 0.2)
        } else if h < 0.6 {
            Color::srgb(0.45 + h * 0.5, 0.9, 1.0 - h * 0.4)
        } else {
            Color::srgb(1.0, 0.9 - (h - 0.6) * 1.5, 0.3)
        };
    }
    if let Ok(mut vis) = warn_q.single_mut() {
        *vis = if laser.locked {
            Visibility::Visible
        } else {
            Visibility::Hidden
        };
    }
    let n = upgrades.charges;
    for (pip, mut vis, mut bg) in &mut pips {
        *vis = if pip.0 < n.min(10) {
            Visibility::Visible
        } else if pip.0 < 10 && n < 10 {
            // Show empty sockets up to a soft max so the row reads as a gauge.
            if pip.0 < 5 {
                Visibility::Visible
            } else {
                Visibility::Hidden
            }
        } else {
            Visibility::Hidden
        };
        bg.0 = if pip.0 < n {
            MAGENTA
        } else {
            Color::NONE
        };
    }
    if let Ok(mut t) = overflow_q.single_mut() {
        t.0 = if n > 10 {
            format!("+{}", n - 10)
        } else {
            String::new()
        };
    }
}

fn hud_shop(
    near: Res<NearShop>,
    wallet: Res<Wallet>,
    inventory: Res<Inventory>,
    upgrades: Res<Upgrades>,
    mut panel_q: Query<&mut Visibility, (With<ShopPanel>, Without<ShopDot>)>,
    mut names: Query<(&ShopName, &mut Text), Without<ShopPillText>>,
    mut dots: Query<(&ShopDot, &mut Visibility, &mut BackgroundColor), Without<ShopPanel>>,
    mut pills: Query<(&ShopPill, &mut BackgroundColor), (Without<ShopDot>, Without<ShopPanel>)>,
    mut pill_texts: Query<(&ShopPillText, &mut Text, &mut TextColor), Without<ShopName>>,
) {
    let Ok(mut vis) = panel_q.single_mut() else {
        return;
    };
    if !near.0 {
        *vis = Visibility::Hidden;
        return;
    }
    *vis = Visibility::Visible;

    // (name, level for dots (None = no dots), cost text, affordable)
    let rows: [(String, Option<u32>, String, bool); 6] = [
        (
            "Sell hold".into(),
            None,
            format!("+{}", commas(inventory.total_value())),
            inventory.units > 0,
        ),
        (
            format!("Laser  {:.0} DPS", upgrades.dps()),
            Some(upgrades.power_lvl),
            commas(upgrades.power_cost()),
            wallet.credits >= upgrades.power_cost(),
        ),
        (
            "Coolant loop".into(),
            Some(upgrades.coolant_lvl),
            commas(upgrades.coolant_cost()),
            wallet.credits >= upgrades.coolant_cost(),
        ),
        (
            format!("Tractor  {:.1}m", upgrades.magnet_range()),
            Some(upgrades.tractor_lvl),
            commas(upgrades.tractor_cost()),
            wallet.credits >= upgrades.tractor_cost(),
        ),
        (
            "Recall rig".into(),
            None,
            if upgrades.recall {
                "OWNED".into()
            } else {
                commas(RECALL_COST)
            },
            !upgrades.recall && wallet.credits >= RECALL_COST,
        ),
        (
            format!("Plasma ×{}", crate::game::PLASMA_PACK_SIZE),
            None,
            commas(upgrades.plasma_cost()),
            wallet.credits >= upgrades.plasma_cost(),
        ),
    ];

    for (name, mut text) in &mut names {
        text.0 = rows[name.0].0.clone();
    }
    for (dot, mut vis, mut bg) in &mut dots {
        match rows[dot.row].1 {
            Some(lvl) => {
                *vis = Visibility::Visible;
                bg.0 = if dot.idx < lvl.min(5) {
                    CYAN
                } else {
                    Color::NONE
                };
            }
            None => *vis = Visibility::Hidden,
        }
    }
    for (pill, mut bg) in &mut pills {
        bg.0 = if rows[pill.0].3 { PILL_OK_BG } else { PILL_NO_BG };
    }
    for (pt, mut text, mut color) in &mut pill_texts {
        text.0 = rows[pt.0].2.clone();
        color.0 = if rows[pt.0].3 { AFFORD } else { TOO_RICH };
    }
}

fn hud_status(
    status: Res<StatusMsg>,
    mut panel_q: Query<&mut Visibility, With<StatusPanel>>,
    mut text_q: Query<&mut Text, With<StatusText>>,
) {
    let Ok(mut vis) = panel_q.single_mut() else {
        return;
    };
    if status.ttl > 0.0 {
        *vis = Visibility::Visible;
        if let Ok(mut text) = text_q.single_mut() {
            text.0.clone_from(&status.text);
        }
    } else {
        *vis = Visibility::Hidden;
    }
}

/// Full control reference only while the cursor is free; in play, the screen
/// belongs to the game.
fn hud_controls(focused: Res<Focused>, mut q: Query<&mut Text, With<ControlsText>>) {
    let Ok(mut text) = q.single_mut() else { return };
    text.0 = if !focused.0 {
        "Click to take the controls\nWASD thrust · Space up · C down · Shift brake · LMB laser · Q charge · E trade · T/G recall · Esc release"
            .to_string()
    } else {
        String::new()
    };
}

/// Point home: the home icon pinned toward the depot, clamped to the screen
/// edge when it's off-camera. Hidden once you're close enough to see the pad.
fn hud_nav_marker(
    player: Res<PlayerState>,
    cameras: Query<(&Camera, &GlobalTransform), With<Camera3d>>,
    mut markers: Query<(&mut Node, &mut Visibility), With<NavMarker>>,
    mut dist_q: Query<&mut Text, With<NavDistText>>,
) {
    let Ok((camera, cam_tf)) = cameras.single() else {
        return;
    };
    let Ok((mut node, mut vis)) = markers.single_mut() else {
        return;
    };
    let target = shop_pos() + Vec3::Y * 1.5;
    let dist = player.pos.distance(target);
    if dist < 12.0 {
        *vis = Visibility::Hidden;
        return;
    }
    *vis = Visibility::Visible;
    if let Ok(mut t) = dist_q.single_mut() {
        t.0 = format!("{:.0}m", dist);
    }

    let Some(view_size) = camera.logical_viewport_size() else {
        return;
    };
    let center = view_size * 0.5;
    let margin = 56.0;

    match camera.world_to_viewport(cam_tf, target) {
        Ok(screen) => {
            node.left = Val::Px(screen.x.clamp(margin, view_size.x - margin));
            node.top = Val::Px(screen.y.clamp(margin, view_size.y - margin));
        }
        Err(_) => {
            let local = cam_tf.affine().inverse().transform_point3(target);
            let dir2 = Vec2::new(local.x, -local.y).normalize_or_zero();
            let pos = center + dir2 * (center.min_element() - margin);
            node.left = Val::Px(pos.x.clamp(margin, view_size.x - margin));
            node.top = Val::Px(pos.y.clamp(margin, view_size.y - margin));
        }
    }
}

// ---------------------------------------------------------------------------
// Floating world-anchored text
// ---------------------------------------------------------------------------

/// Spawn floating text at a world position (damage numbers, credit pops, ...).
pub fn spawn_float_text(
    commands: &mut Commands,
    world_pos: Vec3,
    text: String,
    color: Color,
    size: f32,
    ttl: f32,
) {
    commands.spawn((
        Text::new(text),
        font(size),
        TextColor(color),
        Node {
            position_type: PositionType::Absolute,
            ..default()
        },
        FloatText {
            world_pos,
            age: 0.0,
            ttl,
            rise: 0.7,
        },
    ));
}

/// Damage number: a killing blow is bigger and gold; ticks are small + white.
pub fn spawn_damage_number(commands: &mut Commands, world_pos: Vec3, amount: f32, killed: bool) {
    let (size, color, ttl) = if killed {
        (30.0, GOLD, 0.95)
    } else {
        (19.0, Color::srgb(0.9, 1.0, 1.0), 0.6)
    };
    spawn_float_text(
        commands,
        world_pos,
        format!("{}", amount.round().max(1.0) as i64),
        color,
        size,
        ttl,
    );
}

fn animate_float_text(
    time: Res<Time>,
    cameras: Query<(&Camera, &GlobalTransform), With<Camera3d>>,
    mut texts: Query<(Entity, &mut FloatText, &mut Node, &mut TextColor)>,
    mut commands: Commands,
) {
    let Ok((camera, cam_tf)) = cameras.single() else {
        return;
    };
    let dt = time.delta_secs();
    for (entity, mut ft, mut node, mut color) in &mut texts {
        ft.age += dt;
        let t = ft.age / ft.ttl;
        if t >= 1.0 {
            commands.entity(entity).despawn();
            continue;
        }
        let world = ft.world_pos + Vec3::Y * (t * ft.rise);
        match camera.world_to_viewport(cam_tf, world) {
            Ok(screen) => {
                node.left = Val::Px(screen.x);
                node.top = Val::Px(screen.y);
                let alpha = (1.0 - (t - 0.33).max(0.0) / 0.67).clamp(0.0, 1.0);
                color.0 = color.0.with_alpha(alpha);
            }
            Err(_) => color.0 = color.0.with_alpha(0.0),
        }
    }
}

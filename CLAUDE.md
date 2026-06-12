# [Limits]

A Bevy 0.18 incremental space-mining roguelite: first-person **zero-G laser mining at the edge of a dying solar system** (0.25m voxels — the player is ~6 voxels tall). Each ~10-minute "day" opens with **the system's star collapsing into a black hole** — supernova, shockwave, the works — which then feeds all day: growing, advancing, swallowing planets one by one, and finally the belt and you. Mine asteroids and **land on huge voxel planets** (the ones orbiting closest to the hole carry endgame tiers — barely reachable before they're eaten), tractor the loot, sell at the depot, and **spend before the horizon takes you**: when you're consumed the screen whites out and a new day dawns over a reseeded system. Upgrades persist across days; credits, cargo, and the field do not. There is no timer — the deadline is gravity: pull ramps past your thrusters, the horizon eats the rocks you'd hide behind, and overtime only makes it hungrier. Grown out of (and still benchmarked like) a high-entity-count stress sandbox.

## Game loop & controls

- Click to grab the cursor. Mouse-look + **6DOF thruster flight**: W/S along the look ray, A/D strafe, **Space** up, **C** down, **Shift** brakes. On an asteroid surface you walk (flat WASD) and Space hops you off. Zero-G: nothing falls, including you.
- Hold **LMB** to fire the mining laser: continuous DPS vs block HP. The beam builds **heat**; at 100% it locks and vents for ~2s (red emitter, warning text). Coolant upgrades stretch fire time.
- Every destroyed voxel drops physical loot that tractor-beams to you in range; crystals glow in the walls (tier 0 "Carbon" is deliberately modest — colors get loud further out). Asteroids are richer toward their centers and have a pure-crystal core.
- **Q** throws a plasma charge: 2s fuse, carves a 4m sphere, loot arrives as *stacked* drops. You start with 2; more at the depot.
- **E** at the depot pad sells the hold; **1/2/3/4/5** buy laser power / coolant loop / tractor field / recall rig / plasma charges. **T** recalls home, **G** jumps back to your farthest-reached site (after buying recall). Teleports are dead during the dive — no warping out of the horizon's grip.
- **The black hole** (`blackhole.rs`) is the session clock, physically: born at half size from the star collapse ~4s into the day, it approaches from ~5.6km to ~1.35km and grows to full size while pull at home ramps 0.02 → 26 m/s² (thrust is 16). There is NO deadline trigger — past the nominal day it goes into **overtime**: keeps advancing along the system axis (sweeping the field, engulfing anyone braced behind rock) and its gm doubles every 45s (so fleeing into deep space only buys minutes). Pull applies to the player, loot, and plasma charges; >7 m/s² rips a grounded player off the surface. The dive triggers physically — grazing the horizon, or pull >40 m/s² while already dragged inward >12 m/s — locks the camera, ramps to ~700 m/s, whites out, then `day_reset` reseeds the world (`world::set_world_salt(day-1)`), clears chunks/impostors/loose entities, zeroes wallet + hold + max-range, and keeps `Upgrades` wholesale. The advancing horizon **consumes asteroids and planets** (lod.rs despawns them with shard-streak FX; an `eaten` set stops them resurrecting in the hole's wake). `LIMITS_DAY_S=<secs>` shortens the day for testing.
- **Planets** (`world::planet_list`, salt-cached): 6 huge landable bodies (160–380m radius, up to ~25x the biggest belt rock) strung along `world::SYSTEM_AXIS` between the belt (home, at the system's edge) and the star. Same voxel pipeline as asteroids — land, walk, mine, blast — but visually distinct: crater bowls baked into `surface_toward` (so voxels + impostor agree), saturated species palettes (`is_planet` branch in block_color/impostor_color), species-tinted regolith, an additive atmosphere shell, and rings on every third world. Tier climbs toward the star (4 → 12): the endgame lives deep in the kill zone and gets eaten mid-day, so reaching it is a race. Each planet gets one always-visible ico(5) impostor (never proximity-hidden — streamed chunks just draw over it locally); belt cells inside a planet's reach are suppressed.
- HUD is graphical, not textual: procedurally-baked 12x12 **pixel icons** (no asset files; see the `Glyph` constants in `hud.rs`), stat chips (credits / range·best·sector / speed), heat bar + plasma pip circles, loot-color swatches in the hold chip, a depot panel of key-chips + icons + level dots + cost pills, and a home-icon nav marker pinned to the screen edge when the depot is off-camera. The controls reference only renders while the cursor is free. Numbers stay text; prose labels don't.
- The whole progression curve (HP/value/cost growth factors) lives in the constants at the top of `src/game.rs` and `src/world.rs`.

## Architecture notes

- World: an unbounded 3D asteroid field of **rigid voxel BODIES the black hole physically drags in**. Each asteroid/planet keeps its terrain in "gen space" (its original frame — storage, edits, meshes never change) and carries a `BodyMotion` displacement toward the hole: chunk meshes ride a per-body root transform (smooth, free), while collision/raycast/mining map world→gen through a **voxel-quantized offset** (integer voxel shifts keep every grid boundary lattice-aligned, so all grid math works verbatim). The player standing on a body is **carried** by its per-frame delta (`carrier_delta`); a body whose true position crosses the horizon has its REAL chunks despawned (`consume_body`) — terrain genuinely eaten, ~hundreds of bodies per day. Newly-seen bodies get a deterministic **backfilled** offset (integrating the analytic `blackhole::hole_kinematics` from dawn). Bodies translate but never rotate; they don't collide with each other; the home rock is anchored. Storage is keyed `(body, chunk)`; edits/damage key by gen-space voxel (bodies are disjoint in gen space). A deterministic hash gives each 56m cell at most one asteroid — **very sparse (1.8% of cells) but individual**: per-seed ellipsoid stretch, displacement amplitude/frequency, and a **species** (8 palettes/names driving rock color; ore stays tier-based). A starter rock is guaranteed within 2 cells of home each day. Worldgen is a pure function of (gen coords, world salt) — `world::set_world_salt` is the one mutable input, bumped per day; tests assume salt 0 and must never change it (process-global, tests run in parallel).
- Chunk streaming: chunks materialize nearest-first within ~56m of the player (budgeted per frame) and unload behind them. Pure-vacuum chunks cost a set entry, never storage/entities. **Player edits live in a sparse overlay** that survives unload and is re-applied on regeneration (this is also exactly what a save file would serialize). `VoxelWorld::block()` falls back to pure worldgen + edits for unmaterialized chunks, so collision/raycasts are correct anywhere. A test asserts chunk contents always equal pure gen.
- Far-field LOD (`lod.rs`): every asteroid out to ~850m gets an **impostor** — a ~160-vert ico-sphere displaced by the same `surface_toward` noise the voxel gen uses, radius quantized to voxel steps and **inset 0.3m inside the true surface**, vertex-colored to the tier palette. Inset is the anti-pop trick: streamed voxel chunks draw OVER the impostor, so a rock "resolves" into voxels face-by-face and the final hide (38m) happens when it's already buried. New impostors grow in over ~1s, built nearest-first a few per frame, despawned far behind. This is what makes the view distance read as near-infinite; never raise raw chunk GEN_RADIUS for visibility.
- Lighting: no shadow maps. An upward-ray **Enclosure** probe (smoothed) fades sun + ambient when you're inside rock, and drives the helmet lamp the *opposite* way (dim in daylight, bright in tunnels — they never stack). A weak cool anti-sun fill keeps shadow-side voxel faces from being void-black stripes against space.
- Rendering: **two** naive-culled meshes per chunk, vertex-colored (no block textures): a lit mesh for rock, and an unlit mesh whose vertex colors run >1.0 for crystal faces — the HDR camera + bloom turn those into glowing ore veins. Remeshed on demand with a per-frame budget.
- The camera carries `Hdr` + `Bloom` + `Tonemapping::TonyMcMapface` (inserted by `sky.rs` in PostStartup). Every glow in the game — laser beam, crystals, sun, depot beacon, sparks — is just an unlit material with linear color >1.0 feeding that bloom pass. Keep light intensities modest: a spotlight concentrates lumens ~10x vs a point light and will white-disc any close wall.
- Sky (`sky.rs`): a starfield **cubemap** (milky way, nebulae, hashed stars) generated into an `Image` at startup + HDR sun ball, ringed gas giant, rust-red rocky world with polar caps, azure ice giant (all procedural equirect textures), moon, a comet on a slow orbit (soft-beam tail, always anti-sun), a hard-strobing pulsar, twinkling stars, and shooting stars — all parented to a `SkyAnchor` that follows the player, so they sit at effective infinity. Headlamp dust motes appear in tunnels. Zero texture/model assets in the repo.
- The black hole (`blackhole.rs`) is NOT sky furniture — it's a real world-space entity (camera far plane is 14km for it): unlit-black horizon sphere, billboarded HDR photon ring + lensing arc + halo (lensing reads from any angle), TWO counter-rotating doppler-beamed disc layers, and infalling debris streaks parented to the disc so its tilt/spin come free. **No beam/jet geometry on the hole** — tried twice, always reads as a stick of light. Any light-streak anywhere in the game must use `sky::beam_mesh()` (crossed quads, alpha-faded spine), NEVER a stretched cuboid/cone. A synthesized dread-rumble loop rides the published `BlackHole.dread` level; HUD shows a doom gauge (day pip + horizon bar + countdown), red vignette past dread 0.45, and the whiteout/DAY-N splash.
- Audio (`audio.rs`): every clip synthesized at startup into in-memory WAVs (`AudioSource { bytes }` — needs the `wav` cargo feature). Gameplay pushes `SfxEvent`s into the `SfxQueue` resource; loops (laser hum, jetpack, ambient drone) follow the `LaserState`/`JetState` resources. No audio files.
- Explosions aggregate loot into **stacked drops** (`count` per drop entity, ≤14 stacks per material group) — never one drop per voxel; a 4m blast carves ~12k voxels and one-entity-per-voxel was 553k entities / 9 FPS in playtesting.

## Pinned versions

- **Bevy: `0.18.1`** — when you bump this, re-read the migration guide before changing any spawn / scheduling / rendering code.
- Reference docs:
  - https://docs.rs/bevy/0.18.1/bevy/ — top-level module list and prelude
  - https://bevy.org/learn/quick-start/getting-started/setup/ — fast-compile recommendations
  - https://bevy.org/learn/migration-guides/0-17-to-0-18/ — what changed in 0.18

> **If you're about to write Bevy code in an area you haven't touched recently, fetch the relevant page from docs.rs first.** The model's training data lags the API. As of 0.18, bundles like `SpriteBundle` / `Camera2dBundle` / `TextBundle` are gone — `Sprite`, `Camera2d`, `Text` / `Text2d` are now plain components and use Required Components to auto-insert `Transform`, `Visibility`, etc. `FrameTimeDiagnosticsPlugin` is no longer a unit struct; use `::default()` or `::new(history_len)`.

## Conventions

- **ECS-first.** New behavior arrives as a system + components, not as a method on a god-resource.
- **Components small and flat.** A motion component should be a `Vec2` or `f32`, not a struct of structs. We deliberately use a separate `Position(Vec2)` rather than reusing `Transform` (40 bytes) to keep the hot motion loop cache-friendly.
- **Prefer `Query` over `ResMut`** wherever possible. Resources serialize systems; queries can be parallelized and scheduled independently.
- **No per-frame heap allocations in hot systems.** No `Vec::new()`, `format!`, `to_string()`, or `Box::new` inside a system that touches more than a handful of entities. Pre-allocate at startup, scratch via local resources.
- **Any system touching >1000 entities should use `par_iter` / `par_iter_mut`** unless there's a measured reason not to (e.g. write contention, ordering dependency).
- **Use `bevy_diagnostic` for observability** — never `println!` / `dbg!` in hot paths; they're synchronized I/O and will tank framerate on the way to telling you why your framerate is bad.

## Build cheat sheet

```sh
cargo run --features dev          # fast iteration (dynamic linking)
cargo run --release               # perf testing (statically linked, no dev feature)
cargo check --features dev        # type-check loop, should be sub-second on warm cache
```

The `dev` feature gates Bevy's `dynamic_linking`. Release builds intentionally don't enable it — shipping a dylib is awkward and it disables some optimizations.

The Windows linker config in `.cargo/config.toml` uses `rust-lld.exe` (bundled with rustup) for ~5–10x faster relinks vs the default MSVC linker.

### Toolchain on this machine (GNU, not MSVC)

`rust-toolchain.toml` pins `stable-x86_64-pc-windows-gnu` because the dev box doesn't have the Windows 10 SDK installed (so MSVC-ABI linking can't find `kernel32.lib` / `ws2_32.lib`). Two consequences:

1. **`PATH` must include the MSYS2 mingw-w64 binutils.** Rustup's bundled mingw subset is missing `dlltool`'s helpers (`ar`, `as`), which the bevy_dylib build needs when `dynamic_linking` is enabled. Add `C:\msys64\mingw64\bin` to your shell's `PATH`. Without it, `cargo build --features dev` fails with `dlltool ... CreateProcess`.
2. **Switching to MSVC.** If you ever install VS 2022 Build Tools + Win10 SDK, change `rust-toolchain.toml` to `stable-x86_64-pc-windows-msvc` and the existing `[target.x86_64-pc-windows-msvc]` block in `.cargo/config.toml` activates `rust-lld` automatically — Bevy's official recommended setup.

### AV gotcha — folder names with `[` `]`

Don't put this project inside a directory whose name contains square brackets. Both Windows Defender and AVG (and likely most AVs) treat `[` `]` as glob metacharacters in folder exclusions, so an exclusion for `D:\foo\[Bar]\` matches a character-class `B|a|r` rather than the literal folder. The linker then fails with `Permission denied` on every emitted `.exe`. Stick to plain ASCII letters/numbers/hyphens for any folder above `target/`.

## Performance discipline

1. **Profile before optimizing.** Cargo-flamegraph or Tracy. A guess about the bottleneck is wrong about half the time.
2. **Suspect allocation first.** A single `Vec::new()` inside a 100k-entity loop will eat your frame budget faster than any rendering inefficiency.
3. **Suspect the renderer last.** Bevy's batched sprite renderer is genuinely fast; if FPS is bad at 100k entities and your CPU profile shows render at 5%, the bug is in your sim, not the GPU.
4. **Measure, don't theorize.** "This system *should* be fine in serial" is a hypothesis. Confirm or refute it with the diagnostics plugin and a stopwatch.

## Deferred work — parking lot

Don't build these yet; note them so we don't forget.

- **Steam integration** — likely `bevy_steamworks` or raw `steamworks-rs`. Decision pending: which is more actively maintained against current Bevy.
- **Save system** — needed before any real release. The day loop shrank it: only `Upgrades` + the day counter survive a day anyway, so a save is just those (mid-day state is intentionally disposable). serde + bincode.
- **Greedy meshing + texture atlas** — naive per-face meshing is fine at current scale; revisit if hollowed-out worlds get deep enough to hurt.
- **Spatial audio** — SFX are flat mono today; `PlaybackSettings::with_spatial` exists when it matters.
- **Surface shadows** — DirectionalLight shadows would make the boulder field gorgeous; needs cascade tuning vs the deep shaft.

## Layout

```
src/
  main.rs    App wiring, GameplaySet, sun + fill + enclosure lighting, bench-exit hook
  world.rs   Asteroid-field worldgen (per-day salt, species/shape variety), chunk
             streaming + edit overlay, dual meshing (lit + glow), raycast (WorldPlugin)
  blackhole.rs  The singularity: visuals, gravity, the day clock, dive cinematic,
             and the day reset (BlackHolePlugin)
  player.rs  6DOF zero-G flight + surface walking, voxel AABB collision, laser
             mining + heat, Enclosure probe, beam/impact FX, viewmodel, shake (PlayerPlugin)
  items.rs   Stacked loot drops + tractor pickup, plasma charges, explosions
             (flash/shockwave/sparks/debris) (ItemsPlugin)
  game.rs    Credits/hold/upgrades/depot/teleports — the incremental economy (GamePlugin)
  hud.rs     Top bar, reticle + heat + pips, target panel, depot rows, nav marker,
             float text (HudPlugin)
  sky.rs     Starfield cubemap, HDR camera setup, SkyAnchor celestials, dust (SkyPlugin)
  lod.rs     Far-asteroid impostor meshes — near-infinite view distance (LodPlugin)
  audio.rs   Procedural WAV synthesis, SfxQueue, laser/jet/ambient loops (SoundPlugin)
  demo.rs    LIMITS_DEMO scripted tour + screenshots — hands-off verification (DemoPlugin)
.cargo/
  config.toml      Windows fast-link config (rust-lld for MSVC ABI)
rust-toolchain.toml  Pins GNU ABI on this machine; see "Toolchain" above
```

## Bench & verification hooks

- `LIMITS_BENCH_EXIT_AFTER=<seconds>` — process exits after that elapsed wall time.
- `LIMITS_DAY_S=<seconds>` — shorten the black-hole day (default 600). `LIMITS_DEMO=1 LIMITS_DAY_S=10` captures the dive + dawn in the demo's fixed screenshot beats; `LIMITS_DAY_S=18` with the demo verifies a full consume→reset→day-2 cycle in the breadcrumb log.
- `LIMITS_DEMO=1` — scripted ~25s tour (sky pan → laser to overheat → fly to the nearest neighbor asteroid → torpedo it → fly home → sell) that saves 8 PNGs into `shots/` and logs `[demo]` breadcrumbs to stderr. Combine both for a self-terminating visual smoke test:

```sh
LIMITS_DEMO=1 LIMITS_BENCH_EXIT_AFTER=30 ./target/release/limits.exe > demo.log 2>&1
```

`LogDiagnosticsPlugin` writes FPS / frame_time / entity_count to stdout once per second, so:

```sh
LIMITS_BENCH_EXIT_AFTER=15 ./target/release/limits.exe > bench.log 2>&1
```

…gives you a clean log to scrape.

//! eqoxide-renderer — the GPU/render core (the View's rendering layer, #544 Step 2n).
//!
//! This is the lowest View layer: it owns the wgpu device/pipelines/passes, the vertex & uniform
//! GPU structs, the zone/character/billboard draw code, model + animation building, and the
//! view/projection camera math. It has **zero up-refs into the app loop** — `app.rs`, `ui/*`, and
//! `main.rs` depend UP on it (never the reverse). Its only downward deps are the already-extracted
//! lower crates (`eqoxide-core` for game_state/coord/skills/race_class, `eqoxide-assets` for
//! mesh/texture/zone assets) plus the GPU/math externals (wgpu, glam, bytemuck, gltf, image).
//!
//! The app crate (`eqoxide`) re-exports these modules as `crate::{gpu, pass, …}` so every existing
//! `crate::renderer::…` / `crate::scene::…` call site across app.rs/ui/main.rs keeps resolving.

pub mod gpu;
pub mod nav_overlay;
pub mod pass;
pub mod pipeline;
pub mod renderer;
pub mod scene;
pub mod models;
pub mod anim;
pub mod billboard;
pub mod camera;
pub mod head;
pub mod frame_capture;

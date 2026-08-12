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

/// Proof-of-draw token (#867), re-exported at the crate root because it is a cross-crate contract
/// (`eqoxide::camera_state::CameraState::snapshot` requires one), not a renderer implementation
/// detail. See [`renderer::DrawnFrame`].
pub use renderer::DrawnFrame;

/// Proof-of-acquisition token (#895) — `DrawnFrame`'s pre-draw sibling, re-exported for the same
/// reason: `eqoxide::camera_state::TakenCameraCmd::apply_to` requires one, so it is a cross-crate
/// contract rather than a renderer internal. See [`renderer::AcquiredFrame`] for exactly what it
/// does and does not prove.
pub use renderer::AcquiredFrame;

pub mod gpu;
pub mod nav_overlay;
pub mod pass;
pub mod pipeline;
pub mod renderer;
pub mod scene;
pub mod skin_observation;
pub mod models;
pub mod anim;
pub mod billboard;
pub mod camera;
pub mod head;
pub mod frame_capture;

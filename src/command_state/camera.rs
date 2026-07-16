//! Camera command verbs — Wave-2 fan-out stub (see `mod.rs` "HOW TO MIGRATE A DOMAIN").
//!
//! Domain: `/v1/camera/*` — specifically the manual-move/jump escape hatch, held here as the lone
//! `self.camera_manual_move` slot (the rest of `ipc::CameraSlots` is read-path / snapshot and is not
//! on `CommandState`). This command is consumed by the RENDER thread, not `ActionLoop`, so migrating
//! it adds a `request_manual_move` (view write) but its drain lives on the render side, not a
//! `take_*` here. Copy the `request_*` shape of `combat.rs`.

use super::CommandState;

// TODO(#446-fanout): migrate camera manual-move call sites to these methods.
impl CommandState {}

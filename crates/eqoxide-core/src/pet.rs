//! Pet command constants — the `command` field of the RoF2 `PetCommand_Struct`.
//!
//! Pure protocol constants peeled out of `eq_net::protocol` (#544 Step 2h) so the http `pet`
//! endpoint (and its future crate) can resolve them without up-referencing the not-yet-extracted
//! `eq_net` module. Re-exported from `eq_net::protocol` so existing call sites are unchanged.
//!
//! Values from EQEmu zone/common.h: PET_ATTACK=2, PET_FOLLOWME=4 (GetOwner), PET_GUARDHERE=5,
//! PET_SIT=6, PET_BACKOFF=28. Value-identical to the pre-move definitions.

pub const PET_ATTACK: u32 = 2;
pub const PET_FOLLOWME: u32 = 4;
pub const PET_GUARDHERE: u32 = 5;
pub const PET_SIT: u32 = 6;
pub const PET_BACKOFF: u32 = 28;

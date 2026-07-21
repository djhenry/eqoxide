//! Task-journal packet builders (accept/cancel). Moved out of `navigation.rs` (cleanup
//! step 1) — pure `args -> Vec<u8>` builders with no navigation state.

/// OP_AcceptNewTask payload: AcceptNewTask_Struct (12 bytes, all u32): unknown00, task_id
/// (0 = decline all pending offers), task_master_id (the offering NPC's entity id; irrelevant for
/// a decline — only task_id==0 matters per the struct's own EQEmu comment).
pub fn build_accept_new_task(task_id: u32, task_master_id: u32) -> Vec<u8> {
    let mut buf = vec![0u8; 12];
    // buf[0..4] unknown00 = 0
    buf[4..8].copy_from_slice(&task_id.to_le_bytes());
    buf[8..12].copy_from_slice(&task_master_id.to_le_bytes());
    buf
}

/// OP_CancelTask payload: CancelTask_Struct (8 bytes, both u32): SequenceNumber (the task's
/// journal display-order slot, NOT its task_id — see ClientTaskState::CancelTask), type
/// (TaskType — 2 = Quest, the only type this server's content grants).
pub fn build_cancel_task(sequence_number: u32) -> Vec<u8> {
    const TASK_TYPE_QUEST: u32 = 2;
    let mut buf = vec![0u8; 8];
    buf[0..4].copy_from_slice(&sequence_number.to_le_bytes());
    buf[4..8].copy_from_slice(&TASK_TYPE_QUEST.to_le_bytes());
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_accept_new_task_layout() {
        let b = build_accept_new_task(42, 9001);
        assert_eq!(b.len(), 12);
        assert_eq!(u32::from_le_bytes([b[4], b[5], b[6], b[7]]), 42);
        assert_eq!(u32::from_le_bytes([b[8], b[9], b[10], b[11]]), 9001);
    }

    #[test]
    fn build_cancel_task_layout() {
        let b = build_cancel_task(3);
        assert_eq!(b.len(), 8);
        assert_eq!(u32::from_le_bytes([b[0], b[1], b[2], b[3]]), 3);
        assert_eq!(u32::from_le_bytes([b[4], b[5], b[6], b[7]]), 2); // TaskType::Quest
    }
}

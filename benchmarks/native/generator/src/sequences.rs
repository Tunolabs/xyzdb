//! Helpers to format zero-padded sequential IDs.

pub fn empresa_id(i: u64) -> String {
    format!("EMP{:04}", i)
}

pub fn producto_id(i: u64) -> String {
    format!("PRD{:04}", i)
}

pub fn credit_id(i: u64) -> String {
    format!("CR_{:010}", i)
}

pub fn installment_id(i: u64) -> String {
    format!("INS_{:012}", i)
}

pub fn payment_id(i: u64) -> String {
    format!("PAY_{:012}", i)
}

pub fn collection_id(i: u64) -> String {
    format!("COL_{:010}", i)
}

pub fn collection_action_id(i: u64) -> String {
    format!("ACT_{:012}", i)
}

pub fn application_id(i: u64) -> String {
    format!("APP_{:010}", i)
}

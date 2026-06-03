// VHDX backing store. Read path added in Tasks 13-14.
pub struct Vhdx;

mod crc32c;
mod structs;

#[cfg(test)]
pub(crate) mod fixtures;

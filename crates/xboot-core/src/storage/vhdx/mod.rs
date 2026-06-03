// VHDX backing store. Read path added in Task 14.
pub struct Vhdx;

mod crc32c;
mod metadata;
mod structs;

#[cfg(test)]
pub(crate) mod fixtures;

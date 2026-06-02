// Backing-store implementations for VHD. Open dispatch added in Task 9.
pub struct Vhd;

mod footer;

#[cfg(test)]
pub(crate) mod fixtures;

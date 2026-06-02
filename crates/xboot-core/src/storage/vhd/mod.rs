// Backing-store implementations for VHD. Open dispatch added in Task 9.
pub struct Vhd;

mod dynamic;
mod fixed;
mod footer;

pub use fixed::FixedVhd;

#[cfg(test)]
pub(crate) mod fixtures;

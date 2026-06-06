//! TFTP server (phase 06b) — serves prebuilt iPXE binaries over the
//! Trivial File Transfer Protocol (RFC 1350).  Pure codec (`packet`) +
//! async I/O (`server`).

pub mod packet;
pub mod server;

//! proxyDHCP — answers only the PXE part of DHCP; never assigns IP addresses
//! (phase 06a). Pure codec/decision core (`packet`, `options`, `decide`) +
//! thin async I/O (`server`).

pub mod packet;

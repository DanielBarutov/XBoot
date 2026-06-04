//! DHCP option-code constants, typed accessors on `DhcpMessage`, and PXE
//! option-43 (vendor-specific) sub-option builders.

use std::net::Ipv4Addr;

use crate::net::dhcp::packet::DhcpMessage;

// --- DHCP option codes ---
pub const MSG_TYPE: u8 = 53;
pub const PARAM_REQUEST_LIST: u8 = 55;
pub const SERVER_ID: u8 = 54;
pub const TFTP_SERVER_NAME: u8 = 66;
pub const BOOTFILE_NAME: u8 = 67;
pub const VENDOR_CLASS_ID: u8 = 60;
pub const USER_CLASS: u8 = 77;
pub const CLIENT_SYSTEM_ARCH: u8 = 93;
pub const CLIENT_MACHINE_ID: u8 = 97;
pub const VENDOR_SPECIFIC: u8 = 43;

/// DHCP message types (option 53 values).
pub mod msg_type {
    pub const DISCOVER: u8 = 1;
    pub const OFFER: u8 = 2;
    pub const REQUEST: u8 = 3;
    pub const ACK: u8 = 5;
}

/// PXE option-43 sub-option codes (Intel PXE spec).
pub mod pxe {
    pub const DISCOVERY_CONTROL: u8 = 6;
    pub const BOOT_SERVERS: u8 = 8;
    pub const BOOT_ITEM: u8 = 71;
    pub const END: u8 = 255;
}

impl DhcpMessage {
    /// Raw value of the first option with `code`, if present.
    pub fn option(&self, code: u8) -> Option<&[u8]> {
        self.options
            .iter()
            .find(|o| o.code == code)
            .map(|o| o.data.as_slice())
    }

    /// Option 53 message type.
    pub fn message_type(&self) -> Option<u8> {
        self.option(MSG_TYPE).and_then(|d| d.first().copied())
    }

    /// Option 60 vendor class identifier.
    pub fn vendor_class(&self) -> Option<&[u8]> {
        self.option(VENDOR_CLASS_ID)
    }

    /// Option 93 client system architecture (first 2 bytes, big-endian).
    pub fn client_arch(&self) -> Option<u16> {
        self.option(CLIENT_SYSTEM_ARCH)
            .filter(|d| d.len() >= 2)
            .map(|d| u16::from_be_bytes([d[0], d[1]]))
    }

    /// Option 77 user class.
    pub fn user_class(&self) -> Option<&[u8]> {
        self.option(USER_CLASS)
    }
}

/// Option-43 payload for the `:67` proxy OFFER: discovery control + a single
/// boot-server entry pointing the client at our `:4011` service.
pub fn pxe_offer_vendor_opts(server_ip: Ipv4Addr) -> Vec<u8> {
    let mut v = Vec::new();
    // PXE_DISCOVERY_CONTROL = disable broadcast + multicast, use boot-server list only.
    v.extend_from_slice(&[pxe::DISCOVERY_CONTROL, 1, 0x07]);
    // PXE_BOOT_SERVERS: one entry — server type 0x0000, IP count 1, our IP.
    let mut bs = Vec::new();
    bs.extend_from_slice(&[0x00, 0x00]); // boot server type
    bs.push(1); // IP count
    bs.extend_from_slice(&server_ip.octets());
    v.push(pxe::BOOT_SERVERS);
    v.push(bs.len() as u8);
    v.extend_from_slice(&bs);
    v.push(pxe::END);
    v
}

/// Option-43 payload for the `:4011` ACK: a single boot item (type 0, layer 0).
pub fn pxe_ack_vendor_opts() -> Vec<u8> {
    let mut v = Vec::new();
    v.push(pxe::BOOT_ITEM);
    v.push(4);
    v.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // boot item type + layer
    v.push(pxe::END);
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::dhcp::packet::{DhcpMessage, DhcpOption};
    use std::net::Ipv4Addr;

    fn msg(options: Vec<DhcpOption>) -> DhcpMessage {
        DhcpMessage {
            op: 1,
            htype: 1,
            hlen: 6,
            hops: 0,
            xid: 1,
            secs: 0,
            flags: 0,
            ciaddr: Ipv4Addr::UNSPECIFIED,
            yiaddr: Ipv4Addr::UNSPECIFIED,
            siaddr: Ipv4Addr::UNSPECIFIED,
            giaddr: Ipv4Addr::UNSPECIFIED,
            chaddr: [0; 16],
            options,
        }
    }

    #[test]
    fn typed_accessors() {
        let m = msg(vec![
            DhcpOption {
                code: MSG_TYPE,
                data: vec![msg_type::DISCOVER],
            },
            DhcpOption {
                code: VENDOR_CLASS_ID,
                data: b"PXEClient:Arch:00007".to_vec(),
            },
            DhcpOption {
                code: CLIENT_SYSTEM_ARCH,
                data: vec![0x00, 0x07],
            },
            DhcpOption {
                code: USER_CLASS,
                data: b"iPXE".to_vec(),
            },
        ]);
        assert_eq!(m.message_type(), Some(msg_type::DISCOVER));
        assert_eq!(m.vendor_class(), Some(b"PXEClient:Arch:00007".as_ref()));
        assert_eq!(m.client_arch(), Some(0x0007));
        assert_eq!(m.user_class(), Some(b"iPXE".as_ref()));
    }

    #[test]
    fn missing_and_short_options_are_none() {
        let m = msg(vec![DhcpOption {
            code: CLIENT_SYSTEM_ARCH,
            data: vec![0x00],
        }]);
        assert_eq!(m.message_type(), None);
        assert_eq!(m.client_arch(), None); // 1 byte is too short for u16
    }

    #[test]
    fn pxe_offer_vendor_opts_layout() {
        let v = pxe_offer_vendor_opts(Ipv4Addr::new(192, 168, 1, 10));
        // sub-opt 6 (discovery control), len 1, value 0x07
        assert_eq!(&v[0..3], &[pxe::DISCOVERY_CONTROL, 1, 0x07]);
        // sub-opt 8 (boot servers), len 7, type 0x0000, count 1, IP
        assert_eq!(v[3], pxe::BOOT_SERVERS);
        assert_eq!(v[4], 7);
        assert_eq!(&v[5..7], &[0x00, 0x00]);
        assert_eq!(v[7], 1);
        assert_eq!(&v[8..12], &[192, 168, 1, 10]);
        assert_eq!(*v.last().unwrap(), pxe::END);
    }

    #[test]
    fn pxe_ack_vendor_opts_layout() {
        let v = pxe_ack_vendor_opts();
        // sub-opt 71 (boot item), len 4, type 0x0000, layer 0x0000
        assert_eq!(&v[0..6], &[pxe::BOOT_ITEM, 4, 0x00, 0x00, 0x00, 0x00]);
        assert_eq!(*v.last().unwrap(), pxe::END);
    }
}

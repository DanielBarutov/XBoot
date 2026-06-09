//! Login negotiation: resolve the target, fold operational keys, advance stage.

use super::{Connection, Outbound, Stage};
use crate::iscsi::{LoginRequest, LoginResponse};

/// Handle a LoginRequest in the Login stage. Resolves TargetName, negotiates the
/// operational keys we care about, and (when the initiator sets transit→FullFeature)
/// moves the connection to FullFeature. On a missing/unknown target, fails the login
/// with status-class 0x02 and marks the connection Closing.
pub(super) fn handle_login(conn: &mut Connection, req: LoginRequest) -> Vec<Outbound> {
    tracing::info!(
        "iscsi: login PDU transit={} csg={} nsg={} isid={:x?} tsih={} keys={:?}",
        req.transit, req.csg, req.nsg, req.isid, req.tsih, req.text
    );
    // Initialize sequencing from the first login PDU.
    if conn.stat_sn == 0 {
        conn.stat_sn = req.exp_stat_sn;
    }
    conn.exp_cmd_sn = req.cmd_sn;

    // Resolve the target on first declaration of TargetName.
    if conn.target.is_none() {
        if let Some((_, iqn)) = req.text.iter().find(|(k, _)| k == "TargetName") {
            conn.target = conn.registry.read().unwrap().get(iqn);
            if conn.target.is_none() {
                return vec![fail(conn, &req)];
            }
        } else if req.transit && req.nsg == 3 {
            // Transiting to FullFeature without ever naming a target: error.
            return vec![fail(conn, &req)];
        }
        // NSG=1 with no TargetName is fine — initiator will send it in the next PDU.
    }

    // Negotiate operational keys (only during the operational phase, CSG=1, or when
    // jumping directly to FullFeature from security, CSG=0→NSG=3).
    let mut reply_keys: Vec<(String, String)> = Vec::new();
    if req.csg == 1 || (req.csg == 0 && req.nsg == 3) {
        for (k, v) in &req.text {
            conn.params.negotiate(k, v);
        }
        reply_keys.push((
            "MaxBurstLength".into(),
            conn.params.max_burst_length.to_string(),
        ));
        reply_keys.push((
            "FirstBurstLength".into(),
            conn.params.first_burst_length.to_string(),
        ));
        reply_keys.push(("ImmediateData".into(), yes_no(conn.params.immediate_data)));
        reply_keys.push(("InitialR2T".into(), yes_no(conn.params.initial_r2t)));
        reply_keys.push(("HeaderDigest".into(), "None".into()));
        reply_keys.push(("DataDigest".into(), "None".into()));
    }

    // iPXE does a 2-step login: CSG=0→NSG=1 (skip security), then CSG=1→NSG=3 (FullFeature).
    // We accept any requested transit and advance to FullFeature only when NSG=3.
    let transit = req.transit && (req.nsg == 1 || req.nsg == 3);
    if transit && req.nsg == 3 {
        conn.stage = Stage::FullFeature;
        tracing::info!("iscsi: login → FullFeature (transit approved)");
    } else if transit {
        tracing::info!("iscsi: login stage 0→1 accepted, waiting for operational PDU");
    } else {
        tracing::info!(
            "iscsi: login → still Login stage (transit={}, nsg={})",
            req.transit, req.nsg
        );
    }

    vec![Outbound::Login(LoginResponse {
        transit,
        continue_: false,
        csg: req.csg,
        nsg: req.nsg,
        version_max: 0,
        version_active: 0,
        isid: req.isid,
        tsih: if req.tsih == 0 { 1 } else { req.tsih },
        itt: req.itt,
        stat_sn: conn.next_stat_sn(),
        exp_cmd_sn: conn.exp_cmd_sn,
        max_cmd_sn: conn.max_cmd_sn(),
        status_class: 0,
        status_detail: 0,
        text: reply_keys,
    })]
}

/// A login failure response (status-class 0x02 = target/initiator error), closing.
fn fail(conn: &mut Connection, req: &LoginRequest) -> Outbound {
    conn.stage = Stage::Closing;
    Outbound::Login(LoginResponse {
        transit: false,
        continue_: false,
        csg: req.csg,
        nsg: req.csg,
        version_max: 0,
        version_active: 0,
        isid: req.isid,
        tsih: req.tsih,
        itt: req.itt,
        stat_sn: conn.next_stat_sn(),
        exp_cmd_sn: conn.exp_cmd_sn,
        max_cmd_sn: conn.max_cmd_sn(),
        status_class: 0x02,
        status_detail: 0x03, // "not found"
        text: Vec::new(),
    })
}

fn yes_no(b: bool) -> String {
    if b {
        "Yes".into()
    } else {
        "No".into()
    }
}

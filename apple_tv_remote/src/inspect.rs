//! Offline diagnostics: decode a btsnoop capture of the remote's traffic.

use std::path::Path;

use anyhow::{bail, Context, Result};

use crate::audio::{Session, Stats, FRAME_SAMPLES};
use crate::report::{parse_report, AttAssembler, InputReport};

const BTSNOOP_HEADER: &[u8; 8] = b"btsnoop\0";

/// Result of decoding one capture.
#[derive(Debug, Clone)]
pub struct Report {
    /// Monitor records read, regardless of type.
    pub records: u64,
    /// Histogram of decoded HID report lengths.
    pub lengths: std::collections::BTreeMap<usize, u64>,
    /// How many HID reports the ATT layer rejected.
    pub invalid: u64,
    /// Transport-side decoder statistics.
    pub stats: Stats,
}

impl Report {
    /// Decoded audio duration in seconds.
    pub fn seconds(&self) -> f64 {
        self.stats.frames as f64 * FRAME_SAMPLES as f64 / f64::from(crate::audio::SAMPLE_RATE)
    }
}

/// Decode every Opus packet in a BlueZ monitor capture and report statistics.
pub fn inspect_capture(path: &Path) -> Result<Report> {
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    if bytes.len() < 16 || &bytes[..8] != BTSNOOP_HEADER {
        bail!("expected a BlueZ monitor btsnoop capture");
    }
    // Version 1, datalink type 2001: unencrypted HCI with a 4 byte interface id.
    let version = u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
    let datalink = u32::from_be_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]);
    if version != 1 || datalink != 2001 {
        bail!("unsupported btsnoop version {version} datalink {datalink}");
    }
    let mut assembler = AttAssembler::default();
    let mut report = Vec::with_capacity(crate::report::MAX_REPORT);
    let mut session = Session::new().context("create decoder")?.0;
    let mut offset = 16;
    let mut records = 0u64;
    let mut lengths = std::collections::BTreeMap::<usize, u64>::new();
    while let Some(record) = bytes.get(offset..offset + 24) {
        let included = u32::from_be_bytes([record[4], record[5], record[6], record[7]]) as usize;
        let flags = u32::from_be_bytes([record[8], record[9], record[10], record[11]]);
        let data = bytes
            .get(offset + 24..offset + 24 + included)
            .ok_or_else(|| anyhow::anyhow!("truncated capture record"))?;
        offset += 24 + included;
        records += 1;
        // Packet type 5 is HCI ACL data received from the controller.
        if flags & 0xFFFF != 5 {
            continue;
        }
        if assembler.feed(data, &mut report).is_none() {
            continue;
        }
        *lengths.entry(report.len()).or_insert(0) += 1;
        match parse_report(&report) {
            Some(InputReport::Opus { sequence, payload }) => {
                session.push_transport(sequence, payload, std::time::Instant::now());
            }
            Some(InputReport::Buttons(_)) | Some(InputReport::Touch(_)) => {}
            None => {}
        }
    }
    let report = Report {
        records,
        lengths,
        invalid: assembler.invalid,
        stats: session.stats,
    };
    println!(
        "{}",
        serde_json::json!({
            "records": report.records,
            "lengths": report.lengths,
            "invalid": report.invalid,
            "frames": report.stats.frames,
            "decoded_seconds": report.seconds(),
            "sequence_skips": report.stats.sequence_skips,
            "concealed": report.stats.concealed,
            "errors": report.stats.errors,
            "peak": report.stats.peak,
        })
    );
    Ok(report)
}

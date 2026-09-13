//! Build script: embeds a Windows message table into the MSVC binaries so the Application
//! event log renders the agent's events from the agent's own executable (design §4.3).
//!
//! The table maps event IDs 1 through 5 to the template `%1`, so each event's text is its
//! first insertion string. The resource is written as a compiled `.res` file by hand (the
//! format is small and documented) and handed to `link.exe`, which accepts `.res` inputs
//! directly; no `mc.exe` or `rc.exe` is needed. Non-MSVC targets get no resource: the
//! cross-check from Linux only compiles, and the Unix build has no event log.

use std::env;
use std::fs;
use std::path::PathBuf;

/// Lowest and highest event IDs the table covers (`service::eventlog::EVENT_*`).
const LOW_ID: u32 = 1;
const HIGH_ID: u32 = 5;
/// `RT_MESSAGETABLE`.
const RT_MESSAGETABLE: u16 = 11;
/// English (United States); `FormatMessageW` falls back to it from any caller language.
const LANG_EN_US: u16 = 0x0409;
/// `MESSAGE_RESOURCE_UNICODE`: the entry text is UTF-16.
const MESSAGE_RESOURCE_UNICODE: u16 = 0x0001;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc") {
        return;
    }
    let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR is set by cargo"));
    let res = out.join("messages.res");
    fs::write(&res, res_file(&message_table())).expect("writing messages.res");
    println!("cargo:rustc-link-arg={}", res.display());
}

/// `MESSAGE_RESOURCE_DATA`: one block covering `LOW_ID..=HIGH_ID`, each entry the UTF-16
/// text `%1\r\n` (the same template `eventcreate.exe` ships), padded to 4 bytes.
fn message_table() -> Vec<u8> {
    let entry = {
        let text: Vec<u16> = "%1\r\n\0".encode_utf16().collect();
        let raw_len = 4 + text.len() * 2;
        let len = (raw_len + 3) & !3;
        let mut e = Vec::with_capacity(len);
        e.extend_from_slice(&(len as u16).to_le_bytes());
        e.extend_from_slice(&MESSAGE_RESOURCE_UNICODE.to_le_bytes());
        for unit in text {
            e.extend_from_slice(&unit.to_le_bytes());
        }
        e.resize(len, 0);
        e
    };
    let mut data = Vec::new();
    data.extend_from_slice(&1u32.to_le_bytes()); // NumberOfBlocks
    data.extend_from_slice(&LOW_ID.to_le_bytes());
    data.extend_from_slice(&HIGH_ID.to_le_bytes());
    data.extend_from_slice(&16u32.to_le_bytes()); // OffsetToEntries: after this one block
    for _ in LOW_ID..=HIGH_ID {
        data.extend_from_slice(&entry);
    }
    data
}

/// A `.res` file: the mandatory empty header resource, then one `RT_MESSAGETABLE`
/// resource with ID 1 carrying `data`. Numeric type and name make the header 32 bytes.
fn res_file(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    res_header(&mut out, 0, 0, 0, 0);
    out.resize(32, 0);
    res_header(&mut out, data.len() as u32, RT_MESSAGETABLE, 1, LANG_EN_US);
    out.extend_from_slice(data);
    let padded = (out.len() + 3) & !3;
    out.resize(padded, 0);
    out
}

fn res_header(out: &mut Vec<u8>, data_size: u32, type_id: u16, name_id: u16, lang: u16) {
    out.extend_from_slice(&data_size.to_le_bytes());
    out.extend_from_slice(&32u32.to_le_bytes()); // HeaderSize
    out.extend_from_slice(&0xFFFFu16.to_le_bytes()); // numeric type marker
    out.extend_from_slice(&type_id.to_le_bytes());
    out.extend_from_slice(&0xFFFFu16.to_le_bytes()); // numeric name marker
    out.extend_from_slice(&name_id.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // DataVersion
    out.extend_from_slice(&0x0030u16.to_le_bytes()); // MemoryFlags: MOVEABLE | PURE
    out.extend_from_slice(&lang.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // Version
    out.extend_from_slice(&0u32.to_le_bytes()); // Characteristics
}

extern crate zstd;

use std::env;
use std::fmt;
use std::fs::File;
use std::io;
use std::io::{Read, Write};

const BUF_SIZE: usize = 4 * 1024 * 1024; // 4MB buffer
const MAX_XCHECK_LEN: usize = 52;

pub fn main() -> Result<(), std::io::Error> {
    let tag_names = ["Unk", "Ent", "Exi", "Arg", "Ret"]
        .iter()
        .map(ToString::to_string)
        .chain((5..256).map(|n| n.to_string()))
        .collect::<Vec<_>>();

    let mut out = String::with_capacity(BUF_SIZE);
    for arg in env::args() {
        let file = File::open(arg)?;
        let mut reader = zstd::stream::Decoder::new(file)?;
        loop {
            let mut buf = [0u8; 9];
            if reader.read_exact(&mut buf).is_err() {
                break;
            }
            let mut val_buf = [0u8; 8];
            val_buf.copy_from_slice(&buf[1..]);
            let val = u64::from_le_bytes(val_buf);
            if out.len() >= BUF_SIZE - MAX_XCHECK_LEN {
                io::stdout().write_all(out.as_bytes())?;
                out.clear();
            }
            let old_len = out.len();
            let tag_name = &tag_names[buf[0] as usize];
            fmt::write(
                &mut out,
                format_args!("XCHECK({0}):{1:}/0x{1:08x}\n", tag_name, val),
            )
            .expect("Error formatting xcheck");
            assert!(out.len() <= old_len + MAX_XCHECK_LEN);
        }
    }
    // Flush the buffer
    io::stdout().write_all(out.as_bytes())?;
    Ok(())
}

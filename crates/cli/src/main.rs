//! `ferrodb-kv` — an interactive REPL over the ferrodb storage engine.
//!
//! Commands: `put <int-key> <value>`, `get <int-key>`, `scan [lo] [hi]`,
//! `.checkpoint`, `.exit`. Keys are `i64`, order-preserving-encoded.

use rustyline::DefaultEditor;
use storage::btree::tree::{load_meta, BPlusTree};
use storage::buffer::BufferPool;
use storage::disk::DiskManager;
use storage::encoding::{decode_i64, encode_i64};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "ferrodb.db".into());
    let dm = DiskManager::open(&path)?;
    let mut bp = BufferPool::new(dm, 256);
    let mut meta = load_meta(&mut bp)?;
    let mut rl = DefaultEditor::new()?;
    println!(
        "ferrodb-kv — {path}. commands: put <k> <v> | get <k> | scan [lo] [hi] | .checkpoint | .exit"
    );
    while let Ok(line) = rl.readline("kv> ") {
        let _ = rl.add_history_entry(line.as_str());
        let parts: Vec<&str> = line.split_whitespace().collect();
        let mut t = BPlusTree::open(&mut bp, &mut meta);
        match parts.as_slice() {
            [".exit"] => break,
            [".checkpoint"] => {
                t.checkpoint()?;
                println!("ok");
            }
            ["put", k, v] => {
                t.insert(&encode_i64(k.parse()?), v.as_bytes())?;
                println!("ok");
            }
            ["get", k] => match t.get(&encode_i64(k.parse()?))? {
                Some(v) => println!("{}", String::from_utf8_lossy(&v)),
                None => println!("(nil)"),
            },
            ["scan", rest @ ..] => {
                let lo = rest
                    .first()
                    .and_then(|s| s.parse::<i64>().ok())
                    .map(encode_i64);
                let hi = rest
                    .get(1)
                    .and_then(|s| s.parse::<i64>().ok())
                    .map(encode_i64);
                let lo = lo.as_ref().map(|a| &a[..]);
                let hi = hi.as_ref().map(|a| &a[..]);
                for (k, v) in t.scan(lo, hi)? {
                    println!("{} = {}", decode_i64(&k), String::from_utf8_lossy(&v));
                }
            }
            [] => {}
            _ => println!("unknown command"),
        }
    }
    let mut t = BPlusTree::open(&mut bp, &mut meta);
    t.checkpoint()?;
    Ok(())
}

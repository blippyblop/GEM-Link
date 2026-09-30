//! `dda-capture` — headless capture probe: proves the zero-copy capture path
//! on real hardware and reports its timing as machine-readable JSON.

#![allow(unsafe_code)]

use std::process::ExitCode;
use x_dda::{Duplicator, capture_session};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut seconds = 10.0f64;
    let mut adapter = 0u32;
    let mut output = 0u32;
    let mut timeout_ms = 20u32;
    let mut list_only = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--seconds" => seconds = args.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or(10.0),
            "--adapter" => adapter = args.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or(0),
            "--output" => output = args.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or(0),
            "--timeout" => timeout_ms = args.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or(20),
            "--list" => list_only = true,
            other => {
                eprintln!("unknown flag {other:?}");
                return ExitCode::from(2);
            }
        }
        i += 1 + (matches!(args.get(i), Some(v) if v.parse::<f64>().is_ok())) as usize;
    }

    let outputs = match Duplicator::enumerate() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("enumerate failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    println!("outputs: {:?}", outputs);
    if list_only {
        return ExitCode::SUCCESS;
    }
    if !outputs
        .iter()
        .any(|(a, o, _)| *a == adapter && *o == output)
    {
        eprintln!("adapter {adapter} output {output} not found — use --list");
        return ExitCode::from(2);
    }

    match capture_session(adapter, output, seconds, timeout_ms) {
        Ok(stats) => {
            println!("{}", serde_json::to_string_pretty(&stats).unwrap());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("capture failed: {e} (code 0x{:08X})", e.code().0);
            ExitCode::FAILURE
        }
    }
}

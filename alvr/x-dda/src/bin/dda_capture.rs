//! `dda-capture` — headless capture probe: proves the zero-copy capture path
//! on real hardware and reports its timing as machine-readable JSON.

#![allow(unsafe_code)]

use std::process::ExitCode;
use x_dda::{Duplicator, capture_session};

fn bad_flag(msg: &str) -> ExitCode {
    eprintln!("{msg}");
    ExitCode::from(2)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut seconds = 10.0f64;
    let mut adapter = 0u32;
    let mut output = 0u32;
    let mut timeout_ms = 20u32;
    let mut list_only = false;
    let need_num = |i: usize, name: &str| -> Result<f64, String> {
        args.get(i + 1)
            .and_then(|v| v.parse().ok())
            .ok_or_else(|| format!("{name} needs a number"))
    };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--seconds" => {
                seconds = need_num(i, "--seconds").map_err(|m| return bad_flag(&m))?;
                i += 2;
            }
            "--adapter" => {
                adapter = need_num(i, "--adapter").map_err(|m| return bad_flag(&m))? as u32;
                i += 2;
            }
            "--output" => {
                output = need_num(i, "--output").map_err(|m| return bad_flag(&m))? as u32;
                i += 2;
            }
            "--timeout" => {
                timeout_ms = need_num(i, "--timeout").map_err(|m| return bad_flag(&m))? as u32;
                i += 2;
            }
            "--list" => {
                list_only = true;
                i += 1;
            }
            other => return bad_flag(&format!("unknown flag {other:?}")),
        }
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

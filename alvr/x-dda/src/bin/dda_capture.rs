//! `dda-capture` — headless capture probe: proves the zero-copy capture path
//! on real hardware and reports its timing as machine-readable JSON.

#![allow(unsafe_code)]

use std::process::ExitCode;
use x_dda::{Duplicator, SDR_FORMATS, SOURCE_FORMATS, capture_session};

const USAGE: &str = "usage: dda-capture [--seconds N] [--adapter N] [--output N] \
                     [--timeout MS] [--list]";

fn main() -> ExitCode {
    match real_main() {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("{msg}");
            ExitCode::FAILURE
        }
    }
}

fn real_main() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut seconds = 10.0f64;
    let mut adapter = 0u32;
    let mut output = 0u32;
    let mut timeout_ms = 20u32;
    let mut list_only = false;
    let mut sdr_list = false;

    let need_num = |i: usize, name: &str| -> Result<f64, String> {
        args.get(i + 1)
            .and_then(|v| v.parse().ok())
            .ok_or_else(|| format!("{USAGE}\n{name} needs a number"))
    };

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--seconds" => {
                seconds = need_num(i, "--seconds")?;
                i += 2;
            }
            "--adapter" => {
                adapter = need_num(i, "--adapter")? as u32;
                i += 2;
            }
            "--output" => {
                output = need_num(i, "--output")? as u32;
                i += 2;
            }
            "--timeout" => {
                timeout_ms = need_num(i, "--timeout")? as u32;
                i += 2;
            }
            "--list" => {
                list_only = true;
                i += 1;
            }
            "--sdr-list" => {
                sdr_list = true;
                i += 1;
            }
            other => return Err(format!("{USAGE}\nunknown flag {other:?}")),
        }
    }

    let outputs = Duplicator::enumerate().map_err(|e| format!("enumerate failed: {e}"))?;
    println!(
        "outputs: {}",
        outputs
            .iter()
            .map(|(a, o, n)| format!("adapter {a} output {o}: {n}"))
            .collect::<Vec<_>>()
            .join(" | ")
    );
    if list_only {
        return Ok(());
    }
    if !outputs
        .iter()
        .any(|(a, o, _)| *a == adapter && *o == output)
    {
        return Err(format!(
            "adapter {adapter} output {output} not found — use --list"
        ));
    }

    let formats: &[windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT] = if sdr_list {
        &SDR_FORMATS
    } else {
        &SOURCE_FORMATS
    };
    let stats = capture_session(adapter, output, seconds, timeout_ms, formats)
        .map_err(|e| format!("capture failed: {e} (code 0x{:08X})", e.code().0))?;
    println!("{}", serde_json::to_string_pretty(&stats).unwrap());
    Ok(())
}

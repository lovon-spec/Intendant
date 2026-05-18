//! BlackHole audio bridge — runs on the HOST to expose virtual audio over TCP.
//!
//! Captures from one BlackHole device (app output) and plays to another
//! (app input), bridging audio to a remote client over a raw PCM TCP stream.
//!
//! Listens on 127.0.0.1 only. To reach from a VM, open a reverse SSH tunnel
//! from the host:
//!
//!   Host:  ./bh-bridge
//!   Host:  ssh -R 9900:127.0.0.1:9900 guest
//!   Guest: connects to 127.0.0.1:9900 (tunneled to host)
//!
//! Build:  rustc -O scripts/bh-bridge.rs -o bh-bridge
//! Run:    ./bh-bridge [--port 9900] [--rate 24000]
//!
//! Protocol: raw bidirectional PCM16 mono over TCP (no framing).
//!   Host -> Client: captured audio from BlackHole 16ch (app/call output)
//!   Client -> Host: audio to play into BlackHole 2ch (becomes app mic input)

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

const DEFAULT_PORT: u16 = 9900;
const DEFAULT_RATE: u32 = 24000;
const CAPTURE_DEVICE: &str = "BlackHole 16ch";
const PLAYBACK_DEVICE: &str = "BlackHole 2ch";
const CHUNK_SIZE: usize = 4800; // 100ms at 24kHz mono PCM16

fn spawn_capture(rate: u32) -> std::io::Result<Child> {
    Command::new("sox")
        .args([
            "-t",
            "coreaudio",
            CAPTURE_DEVICE,
            "-t",
            "raw",
            "-r",
            &rate.to_string(),
            "-e",
            "signed-integer",
            "-b",
            "16",
            "-c",
            "1",
            "-",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
}

fn spawn_playback(rate: u32) -> std::io::Result<Child> {
    Command::new("sox")
        .args([
            "-t",
            "raw",
            "-r",
            &rate.to_string(),
            "-e",
            "signed-integer",
            "-b",
            "16",
            "-c",
            "1",
            "-",
            "-t",
            "coreaudio",
            PLAYBACK_DEVICE,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
}

fn get_default(device_type: &str) -> Option<String> {
    let output = Command::new("SwitchAudioSource")
        .args(["-c", "-t", device_type])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        None
    }
}

fn set_default(device: &str, device_type: &str) {
    let _ = Command::new("SwitchAudioSource")
        .args(["-s", device, "-t", device_type])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn handle_client(stream: TcpStream, rate: u32) {
    let peer = stream.peer_addr().ok();
    eprintln!("[+] Client connected: {:?}", peer);

    // Save current defaults and switch to BlackHole so the VM's VirtIO
    // audio routes through BlackHole on the host.
    let prev_output = get_default("output");
    let prev_input = get_default("input");
    eprintln!(
        "[*] Switching host audio: output -> {}, input -> {}",
        CAPTURE_DEVICE, PLAYBACK_DEVICE
    );
    set_default(CAPTURE_DEVICE, "output");
    set_default(PLAYBACK_DEVICE, "input");

    let running = Arc::new(AtomicBool::new(true));

    // Capture: BlackHole 16ch → TCP
    let mut capture = match spawn_capture(rate) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[!] Failed to spawn capture sox: {}", e);
            return;
        }
    };

    // Playback: TCP → BlackHole 2ch
    let mut playback = match spawn_playback(rate) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[!] Failed to spawn playback sox: {}", e);
            let _ = capture.kill();
            return;
        }
    };

    let mut capture_stdout = capture.stdout.take().unwrap();
    let mut playback_stdin = playback.stdin.take().unwrap();

    // Thread 1: capture stdout → TCP write
    let mut write_stream = match stream.try_clone() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[!] Failed to clone stream: {}", e);
            let _ = capture.kill();
            let _ = playback.kill();
            return;
        }
    };
    let running_w = running.clone();
    let capture_thread = thread::spawn(move || {
        let mut buf = [0u8; CHUNK_SIZE];
        while running_w.load(Ordering::Relaxed) {
            match capture_stdout.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if write_stream.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        running_w.store(false, Ordering::Relaxed);
    });

    // Thread 2: TCP read → playback stdin
    let mut read_stream = stream;
    let running_r = running.clone();
    let playback_thread = thread::spawn(move || {
        let mut buf = [0u8; CHUNK_SIZE];
        while running_r.load(Ordering::Relaxed) {
            match read_stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if playback_stdin.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        running_r.store(false, Ordering::Relaxed);
    });

    let _ = capture_thread.join();
    let _ = playback_thread.join();

    let _ = capture.kill();
    let _ = playback.kill();
    let _ = capture.wait();
    let _ = playback.wait();

    // Restore original audio defaults
    if let Some(ref dev) = prev_output {
        eprintln!("[*] Restoring host output -> {}", dev);
        set_default(dev, "output");
    }
    if let Some(ref dev) = prev_input {
        eprintln!("[*] Restoring host input -> {}", dev);
        set_default(dev, "input");
    }

    eprintln!("[-] Client disconnected: {:?}", peer);
}

fn main() {
    let mut port = DEFAULT_PORT;
    let mut rate = DEFAULT_RATE;

    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--port" => {
                i += 1;
                port = args[i].parse().expect("invalid port");
            }
            "--rate" => {
                i += 1;
                rate = args[i].parse().expect("invalid rate");
            }
            "--help" | "-h" => {
                eprintln!("Usage: bh-bridge [--port PORT] [--rate SAMPLE_RATE]");
                eprintln!(
                    "  --port  TCP port to listen on (default: {})",
                    DEFAULT_PORT
                );
                eprintln!("  --rate  Sample rate in Hz (default: {})", DEFAULT_RATE);
                eprintln!();
                eprintln!("Listens on 127.0.0.1 only. To reach from a VM, use a reverse");
                eprintln!("SSH tunnel from the host:");
                eprintln!("  ssh -R 9900:127.0.0.1:9900 guest");
                eprintln!();
                eprintln!(
                    "Captures from '{}' and plays to '{}'.",
                    CAPTURE_DEVICE, PLAYBACK_DEVICE
                );
                eprintln!("Protocol: raw bidirectional PCM16 mono over TCP.");
                std::process::exit(0);
            }
            other => {
                eprintln!("Unknown argument: {}", other);
                std::process::exit(1);
            }
        }
        i += 1;
    }

    // Verify sox is available
    match Command::new("sox")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        Ok(s) if s.success() => {}
        _ => {
            eprintln!("[!] sox not found. Install with: brew install sox");
            std::process::exit(1);
        }
    }

    let addr = format!("127.0.0.1:{}", port);
    let listener = TcpListener::bind(&addr).unwrap_or_else(|e| {
        eprintln!("[!] Failed to bind {}: {}", addr, e);
        std::process::exit(1);
    });

    eprintln!("[*] bh-bridge listening on {} (rate={}Hz)", addr, rate);
    eprintln!("[*] Capture: {} -> TCP", CAPTURE_DEVICE);
    eprintln!("[*] Playback: TCP -> {}", PLAYBACK_DEVICE);
    eprintln!("[*] To expose to a VM: ssh -R 9900:127.0.0.1:9900 guest");

    for stream in listener.incoming() {
        match stream {
            Ok(s) => handle_client(s, rate),
            Err(e) => eprintln!("[!] Accept error: {}", e),
        }
    }
}

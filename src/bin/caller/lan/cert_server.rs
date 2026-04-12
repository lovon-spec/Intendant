//! Temporary HTTP server that distributes the client `.p12` cert to
//! LAN clients for initial import. Replaces `python3 -m http.server`
//! from the old bash script.
//!
//! Deliberately minimal: this is a short-lived helper that the user
//! runs once per fresh client device. It serves a small set of known
//! paths and ignores everything else.

use std::path::Path;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::{certs::CertState, instructions, LanError, LanResult};

/// Serve `ca.crt`, `client.p12`, and `client.pfx` (Android alias) plus
/// a small landing page with import instructions. Blocks until the
/// process is interrupted (Ctrl+C).
pub async fn serve(
    state: &CertState,
    port: u16,
    lan_ip: &str,
    https_port: u16,
) -> LanResult<()> {
    let bind_addr = format!("0.0.0.0:{port}");
    let listener = TcpListener::bind(&bind_addr)
        .await
        .map_err(|e| LanError(format!("bind {bind_addr}: {e}")))?;

    print_client_setup_banner(lan_ip, port, https_port, &state.p12_password);

    let cert_dir = state.cert_dir.clone();
    let landing = landing_html(lan_ip, port, https_port, &state.p12_password);

    let shutdown = async {
        let _ = tokio::signal::ctrl_c().await;
        println!();
        println!(":: stopping cert distribution server");
    };

    tokio::select! {
        _ = shutdown => {}
        _ = accept_loop(listener, cert_dir, landing) => {}
    }

    Ok(())
}

async fn accept_loop(listener: TcpListener, cert_dir: std::path::PathBuf, landing: String) {
    loop {
        let (stream, _peer) = match listener.accept().await {
            Ok(c) => c,
            Err(_) => continue,
        };
        let cert_dir = cert_dir.clone();
        let landing = landing.clone();
        tokio::spawn(async move {
            let _ = handle_conn(stream, &cert_dir, &landing).await;
        });
    }
}

async fn handle_conn(
    mut stream: TcpStream,
    cert_dir: &Path,
    landing: &str,
) -> std::io::Result<()> {
    let mut buf = [0u8; 4096];
    let n = stream.read(&mut buf).await?;
    if n == 0 {
        return Ok(());
    }
    let req = String::from_utf8_lossy(&buf[..n]);
    let path = req
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/");

    match path {
        "/" | "/index.html" => {
            write_response(&mut stream, "200 OK", "text/html; charset=utf-8", landing.as_bytes())
                .await
        }
        "/client.p12" | "/client.pfx" => {
            let p12 = cert_dir.join("client.p12");
            match std::fs::read(&p12) {
                Ok(bytes) => {
                    write_response_with_filename(
                        &mut stream,
                        "200 OK",
                        "application/x-pkcs12",
                        &bytes,
                        "client.p12",
                    )
                    .await
                }
                Err(_) => write_response(&mut stream, "404 Not Found", "text/plain", b"not found").await,
            }
        }
        "/ca.crt" => {
            let ca = cert_dir.join("ca.crt");
            match std::fs::read(&ca) {
                Ok(bytes) => {
                    write_response_with_filename(
                        &mut stream,
                        "200 OK",
                        "application/x-x509-ca-cert",
                        &bytes,
                        "ca.crt",
                    )
                    .await
                }
                Err(_) => write_response(&mut stream, "404 Not Found", "text/plain", b"not found").await,
            }
        }
        _ => write_response(&mut stream, "404 Not Found", "text/plain", b"not found").await,
    }
}

async fn write_response(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let headers = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(headers.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.flush().await
}

async fn write_response_with_filename(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &[u8],
    filename: &str,
) -> std::io::Result<()> {
    let headers = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Disposition: attachment; filename=\"{filename}\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(headers.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.flush().await
}

fn landing_html(lan_ip: &str, cert_port: u16, https_port: u16, password: &str) -> String {
    format!(
        r#"<!DOCTYPE html>
<html>
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>Intendant LAN Setup</title>
<style>
body {{ background: #1e1e2e; color: #cdd6f4; font-family: system-ui, sans-serif; max-width: 600px; margin: 2em auto; padding: 0 1em; }}
h1 {{ color: #89b4fa; }}
code, pre {{ background: #313244; color: #f9e2af; padding: 0.2em 0.4em; border-radius: 4px; }}
pre {{ padding: 1em; overflow-x: auto; }}
a.btn {{ display: inline-block; background: #89b4fa; color: #1e1e2e; padding: 0.8em 1.5em; border-radius: 8px; text-decoration: none; font-weight: bold; margin: 0.5em 0; }}
.pw {{ font-family: monospace; background: #45475a; padding: 0.3em 0.6em; border-radius: 4px; }}
</style>
</head>
<body>
<h1>Intendant LAN Setup</h1>
<p>Download the client certificate below, then import it into your device's keychain. Once imported, you can open the dashboard at <a href="https://{lan_ip}:{https_port}">https://{lan_ip}:{https_port}</a>.</p>

<p><a class="btn" href="/client.p12">Download client.p12</a></p>

<p>Certificate password: <span class="pw">{password}</span></p>

<h2>Next steps</h2>
<p>After importing the certificate, open the dashboard: <br><code>https://{lan_ip}:{https_port}</code></p>

<h2>Alternative downloads</h2>
<ul>
<li><a href="/client.pfx">client.pfx</a> — same file, Android-friendly extension</li>
<li><a href="/ca.crt">ca.crt</a> — CA certificate only (for advanced setups)</li>
</ul>

<p style="color: #a6adc8; margin-top: 3em; font-size: 0.9em;">
Distribution server on port {cert_port}. Press Ctrl+C on the daemon host to stop.
</p>
</body>
</html>
"#
    )
}

fn print_client_setup_banner(lan_ip: &str, cert_port: u16, https_port: u16, password: &str) {
    println!();
    println!("============================================================");
    println!("  Client setup");
    println!("============================================================");
    println!();
    println!("  On each client device, open:");
    println!("    http://{lan_ip}:{cert_port}/");
    println!();
    println!("  …and download client.p12.");
    println!();
    println!("  Certificate password: {password}");
    println!();
    println!("  After the cert is installed, the dashboard lives at:");
    println!("    https://{lan_ip}:{https_port}");
    println!();
    instructions::print_all(lan_ip, cert_port);
    println!();
}

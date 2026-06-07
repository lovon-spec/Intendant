//! Strict HTTPS enrollment server for LAN client certificates.
//!
//! This is intentionally a pairing ceremony, not a trust-on-first-use
//! download page. The server starts HTTPS using the same LAN server cert
//! as the mTLS proxy. The operator must copy the certificate fingerprint
//! observed by the browser into this CLI process. Only after it matches
//! the local `server.crt` does the CLI reveal a one-time enrollment secret.
//! The browser redeems that secret over the verified HTTPS connection to
//! unlock `ca.crt`, `client.p12`, and `client.pfx`.

use std::collections::HashSet;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use rand::RngCore;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

use crate::peer::transport::pinning::{format_fingerprint, parse_fingerprint};
use crate::web_tls::{build_acceptor, TlsCertSource};

use super::{certs::CertState, instructions, LanError, LanResult};

const ENROLL_COOKIE: &str = "intendant_lan_enroll";
const MAX_REQUEST_BYTES: usize = 16 * 1024;

#[derive(Default)]
struct EnrollmentGate {
    inner: Mutex<EnrollmentState>,
}

#[derive(Default)]
struct EnrollmentState {
    secret: Option<String>,
    sessions: HashSet<String>,
}

impl EnrollmentGate {
    fn arm_secret(&self, secret: String) {
        let mut inner = self.inner.lock().expect("enrollment gate poisoned");
        inner.secret = Some(secret);
    }

    fn redeem_secret(&self, presented: &str) -> Option<String> {
        let mut inner = self.inner.lock().expect("enrollment gate poisoned");
        let expected = inner.secret.as_deref()?;
        if expected != presented.trim() {
            return None;
        }
        inner.secret = None;
        let token = random_secret();
        inner.sessions.insert(token.clone());
        Some(token)
    }

    fn has_session(&self, token: &str) -> bool {
        let inner = self.inner.lock().expect("enrollment gate poisoned");
        inner.sessions.contains(token.trim())
    }
}

/// Serve `ca.crt`, `client.p12`, and `client.pfx` behind a strict
/// fingerprint-pairing flow. Blocks until interrupted with Ctrl+C.
pub async fn serve(state: &CertState, port: u16, lan_ip: &str, https_port: u16) -> LanResult<()> {
    let cert_path = state.cert_dir.join("server.crt");
    let key_path = state.cert_dir.join("server.key");
    let expected_fingerprint = super::certs::read_server_cert_fingerprint(&state.cert_dir)
        .ok_or_else(|| {
            LanError(format!(
                "no server.crt fingerprint found in {} — run `intendant lan setup` first",
                state.cert_dir.display()
            ))
        })?;
    let acceptor = build_acceptor(&TlsCertSource::Files {
        cert_path: cert_path.clone(),
        key_path: key_path.clone(),
    })
    .map_err(|e| LanError(format!("build enrollment TLS acceptor: {e}")))?;

    let bind_addr = format!("0.0.0.0:{port}");
    let listener = TcpListener::bind(&bind_addr)
        .await
        .map_err(|e| LanError(format!("bind {bind_addr}: {e}")))?;

    let gate = Arc::new(EnrollmentGate::default());
    print_client_setup_banner(lan_ip, port, https_port);

    let prompt_gate = Arc::clone(&gate);
    tokio::spawn(async move {
        prompt_for_pairing(expected_fingerprint, prompt_gate).await;
    });

    let cert_dir = state.cert_dir.clone();
    let p12_password = state.p12_password.clone();
    let lan_ip = lan_ip.to_string();

    let shutdown = async {
        let _ = tokio::signal::ctrl_c().await;
        println!();
        println!(":: stopping enrollment server");
    };

    tokio::select! {
        _ = shutdown => {}
        _ = accept_loop(listener, acceptor, cert_dir, p12_password, lan_ip, https_port, gate) => {}
    }

    Ok(())
}

async fn prompt_for_pairing(expected_fingerprint: String, gate: Arc<EnrollmentGate>) {
    let mut reader = tokio::io::BufReader::new(tokio::io::stdin());
    let mut line = String::new();

    loop {
        print!("  Browser-observed server cert SHA-256 fingerprint: ");
        let _ = std::io::stdout().flush();
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => {
                println!();
                println!("!! stdin closed before pairing completed; no enrollment secret revealed");
                return;
            }
            Ok(_) => {}
            Err(e) => {
                println!();
                println!("!! failed to read fingerprint: {e}");
                return;
            }
        }

        let observed = match normalize_fingerprint_input(&line) {
            Ok(fp) => fp,
            Err(e) => {
                println!("!! invalid fingerprint: {e}");
                continue;
            }
        };

        if observed != expected_fingerprint {
            println!("!! fingerprint did not match this Intendant server; no secret revealed");
            continue;
        }

        let secret = random_secret();
        gate.arm_secret(secret.clone());
        println!();
        println!("============================================================");
        println!("  Browser pairing verified");
        println!("============================================================");
        println!();
        println!("  Enrollment secret:");
        println!("    {secret}");
        println!();
        println!("  Enter that secret on the HTTPS enrollment page.");
        println!("  It can be redeemed once. Keep this server running until");
        println!("  the browser has downloaded ca.crt and client.p12.");
        println!();
        return;
    }
}

async fn accept_loop(
    listener: TcpListener,
    acceptor: TlsAcceptor,
    cert_dir: PathBuf,
    p12_password: String,
    lan_ip: String,
    https_port: u16,
    gate: Arc<EnrollmentGate>,
) {
    loop {
        let (stream, _peer) = match listener.accept().await {
            Ok(c) => c,
            Err(_) => continue,
        };
        let acceptor = acceptor.clone();
        let cert_dir = cert_dir.clone();
        let p12_password = p12_password.clone();
        let lan_ip = lan_ip.clone();
        let gate = Arc::clone(&gate);
        tokio::spawn(async move {
            let stream = match acceptor.accept(stream).await {
                Ok(stream) => stream,
                Err(_) => return,
            };
            let _ = handle_conn(stream, &cert_dir, &p12_password, &lan_ip, https_port, gate).await;
        });
    }
}

async fn handle_conn<S>(
    mut stream: S,
    cert_dir: &Path,
    p12_password: &str,
    lan_ip: &str,
    https_port: u16,
    gate: Arc<EnrollmentGate>,
) -> std::io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let Some(req) = read_request(&mut stream).await? else {
        return Ok(());
    };
    let path = req.path.split('?').next().unwrap_or("/");

    match (req.method.as_str(), path) {
        ("GET", "/") | ("GET", "/index.html") => {
            if request_has_session(&req, &gate) {
                write_html_response(
                    &mut stream,
                    "200 OK",
                    &unlocked_html(lan_ip, https_port, p12_password, None),
                    &[],
                )
                .await
            } else {
                write_html_response(&mut stream, "200 OK", &locked_html(lan_ip, None), &[]).await
            }
        }
        ("POST", "/enroll") => {
            let Some(secret) = form_value(&req.body, "secret") else {
                return write_html_response(
                    &mut stream,
                    "400 Bad Request",
                    &locked_html(
                        lan_ip,
                        Some("Enter the enrollment secret from the terminal."),
                    ),
                    &[],
                )
                .await;
            };
            let Some(token) = gate.redeem_secret(&secret) else {
                return write_html_response(
                    &mut stream,
                    "403 Forbidden",
                    &locked_html(
                        lan_ip,
                        Some("Enrollment secret was invalid or already redeemed."),
                    ),
                    &[],
                )
                .await;
            };
            let cookie =
                format!("Set-Cookie: {ENROLL_COOKIE}={token}; Max-Age=900; Path=/; Secure; HttpOnly; SameSite=Strict");
            write_html_response(
                &mut stream,
                "200 OK",
                &unlocked_html(lan_ip, https_port, p12_password, Some("Pairing complete.")),
                &[cookie],
            )
            .await
        }
        ("GET", "/client.p12") | ("GET", "/client.pfx") => {
            if !request_has_session(&req, &gate) {
                return write_html_response(
                    &mut stream,
                    "403 Forbidden",
                    &locked_html(
                        lan_ip,
                        Some("Pair in the terminal before downloading certs."),
                    ),
                    &[],
                )
                .await;
            }
            let p12 = cert_dir.join("client.p12");
            match std::fs::read(&p12) {
                Ok(bytes) => {
                    write_attachment_response(
                        &mut stream,
                        "200 OK",
                        "application/x-pkcs12",
                        &bytes,
                        "client.p12",
                    )
                    .await
                }
                Err(_) => {
                    write_response(
                        &mut stream,
                        "404 Not Found",
                        "text/plain",
                        b"not found",
                        &[],
                    )
                    .await
                }
            }
        }
        ("GET", "/ca.crt") => {
            if !request_has_session(&req, &gate) {
                return write_html_response(
                    &mut stream,
                    "403 Forbidden",
                    &locked_html(
                        lan_ip,
                        Some("Pair in the terminal before downloading certs."),
                    ),
                    &[],
                )
                .await;
            }
            let ca = cert_dir.join("ca.crt");
            match std::fs::read(&ca) {
                Ok(bytes) => {
                    write_attachment_response(
                        &mut stream,
                        "200 OK",
                        "application/x-x509-ca-cert",
                        &bytes,
                        "ca.crt",
                    )
                    .await
                }
                Err(_) => {
                    write_response(
                        &mut stream,
                        "404 Not Found",
                        "text/plain",
                        b"not found",
                        &[],
                    )
                    .await
                }
            }
        }
        _ => {
            write_response(
                &mut stream,
                "404 Not Found",
                "text/plain",
                b"not found",
                &[],
            )
            .await
        }
    }
}

#[derive(Debug)]
struct HttpRequest {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl HttpRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

async fn read_request<S>(stream: &mut S) -> std::io::Result<Option<HttpRequest>>
where
    S: AsyncRead + Unpin,
{
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    let mut header_end = None;
    let mut content_length = 0usize;

    while buf.len() < MAX_REQUEST_BYTES {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);

        if header_end.is_none() {
            if let Some(pos) = find_header_end(&buf) {
                header_end = Some(pos);
                let headers = String::from_utf8_lossy(&buf[..pos]);
                content_length = parse_content_length(&headers).unwrap_or(0);
            }
        }

        if let Some(pos) = header_end {
            let body_start = pos + 4;
            if buf.len() >= body_start.saturating_add(content_length) {
                break;
            }
        }
    }

    let Some(header_end) = header_end else {
        return Ok(None);
    };
    let header_text = String::from_utf8_lossy(&buf[..header_end]);
    let mut lines = header_text.lines();
    let Some(request_line) = lines.next() else {
        return Ok(None);
    };
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("/").to_string();
    let headers = lines
        .filter_map(|line| {
            let (key, value) = line.split_once(':')?;
            Some((key.trim().to_string(), value.trim().to_string()))
        })
        .collect::<Vec<_>>();
    let body_start = header_end + 4;
    let body_end = body_start
        .saturating_add(content_length)
        .min(buf.len())
        .min(MAX_REQUEST_BYTES);
    let body = if body_start <= body_end && body_start <= buf.len() {
        buf[body_start..body_end].to_vec()
    } else {
        Vec::new()
    };

    Ok(Some(HttpRequest {
        method,
        path,
        headers,
        body,
    }))
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn parse_content_length(headers: &str) -> Option<usize> {
    headers.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        if key.eq_ignore_ascii_case("content-length") {
            value.trim().parse().ok()
        } else {
            None
        }
    })
}

async fn write_html_response<S>(
    stream: &mut S,
    status: &str,
    body: &str,
    extra_headers: &[String],
) -> std::io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    write_response(
        stream,
        status,
        "text/html; charset=utf-8",
        body.as_bytes(),
        extra_headers,
    )
    .await
}

async fn write_response<S>(
    stream: &mut S,
    status: &str,
    content_type: &str,
    body: &[u8],
    extra_headers: &[String],
) -> std::io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    let mut headers = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nContent-Security-Policy: default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; base-uri 'none'; frame-ancestors 'none'\r\n",
        body.len()
    );
    for header in extra_headers {
        headers.push_str(header);
        headers.push_str("\r\n");
    }
    headers.push_str("\r\n");
    stream.write_all(headers.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.flush().await
}

async fn write_attachment_response<S>(
    stream: &mut S,
    status: &str,
    content_type: &str,
    body: &[u8],
    filename: &str,
) -> std::io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    let disposition = format!("Content-Disposition: attachment; filename=\"{filename}\"");
    write_response(stream, status, content_type, body, &[disposition]).await
}

fn request_has_session(req: &HttpRequest, gate: &EnrollmentGate) -> bool {
    let Some(cookie) = req.header("cookie") else {
        return false;
    };
    cookie.split(';').any(|part| {
        let Some((name, value)) = part.trim().split_once('=') else {
            return false;
        };
        name == ENROLL_COOKIE && gate.has_session(value)
    })
}

fn form_value(body: &[u8], name: &str) -> Option<String> {
    let body = String::from_utf8_lossy(body);
    body.split('&').find_map(|part| {
        let (key, value) = part.split_once('=')?;
        if decode_form_component(key)? == name {
            decode_form_component(value)
        } else {
            None
        }
    })
}

fn decode_form_component(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hi = hex_val(bytes[i + 1])?;
                let lo = hex_val(bytes[i + 2])?;
                out.push((hi << 4) | lo);
                i += 3;
            }
            b'%' => return None,
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(10 + b - b'a'),
        b'A'..=b'F' => Some(10 + b - b'A'),
        _ => None,
    }
}

fn normalize_fingerprint_input(input: &str) -> Result<String, String> {
    let compact = input
        .trim()
        .chars()
        .filter(|c| !c.is_ascii_whitespace())
        .collect::<String>();
    let parsed = parse_fingerprint(&compact)?;
    Ok(format_fingerprint(&parsed))
}

fn random_secret() -> String {
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

fn locked_html(lan_ip: &str, message: Option<&str>) -> String {
    let message = message
        .map(|m| format!(r#"<p class="warn">{}</p>"#, escape_html(m)))
        .unwrap_or_default();
    format!(
        r#"<!DOCTYPE html>
<html>
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>Intendant Enrollment</title>
<style>
body {{ background: #1e1e2e; color: #cdd6f4; font-family: system-ui, sans-serif; max-width: 760px; margin: 2em auto; padding: 0 1em; line-height: 1.5; }}
h1 {{ color: #89b4fa; }}
code, pre {{ background: #313244; color: #f9e2af; padding: 0.2em 0.4em; border-radius: 4px; }}
pre {{ padding: 1em; overflow-x: auto; white-space: pre-wrap; }}
input {{ box-sizing: border-box; width: 100%; background: #11111b; color: #cdd6f4; border: 1px solid #45475a; border-radius: 6px; padding: 0.8em; font: inherit; }}
button {{ background: #89b4fa; color: #1e1e2e; border: 0; padding: 0.8em 1.2em; border-radius: 6px; font-weight: bold; margin-top: 0.8em; cursor: pointer; }}
.warn {{ color: #f38ba8; font-weight: 700; }}
.box {{ border: 1px solid #45475a; border-radius: 8px; padding: 1em; background: #181825; }}
</style>
</head>
<body>
<h1>Intendant Strict Pairing</h1>
{message}
<div class="box">
<p>This page is locked until the terminal verifies that your browser is connected to the real Intendant server certificate.</p>
<ol>
<li>In the browser certificate details for this HTTPS page, copy the server certificate's SHA-256 fingerprint.</li>
<li>Paste that fingerprint into the Intendant terminal.</li>
<li>If the terminal accepts it, enter the one-time enrollment secret below.</li>
</ol>
<p>Do not enter any secret unless the terminal accepted the browser-observed fingerprint.</p>
</div>
<form method="post" action="/enroll" autocomplete="off">
<p><label>Enrollment secret<br><input name="secret" type="password" required autofocus></label></p>
<button type="submit">Unlock Certificate Downloads</button>
</form>
<p style="color:#a6adc8;margin-top:2em;font-size:.9em">Enrollment host: {lan_ip}</p>
</body>
</html>
"#
    )
}

fn unlocked_html(lan_ip: &str, https_port: u16, password: &str, message: Option<&str>) -> String {
    let message = message
        .map(|m| format!(r#"<p class="ok">{}</p>"#, escape_html(m)))
        .unwrap_or_default();
    format!(
        r#"<!DOCTYPE html>
<html>
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>Intendant LAN Setup</title>
<style>
body {{ background: #1e1e2e; color: #cdd6f4; font-family: system-ui, sans-serif; max-width: 760px; margin: 2em auto; padding: 0 1em; line-height: 1.5; }}
h1 {{ color: #89b4fa; }}
code, pre {{ background: #313244; color: #f9e2af; padding: 0.2em 0.4em; border-radius: 4px; }}
a.btn {{ display: inline-block; background: #89b4fa; color: #1e1e2e; padding: 0.8em 1.5em; border-radius: 8px; text-decoration: none; font-weight: bold; margin: 0.5em 0.5em 0.5em 0; }}
.pw {{ font-family: monospace; background: #45475a; padding: 0.3em 0.6em; border-radius: 4px; }}
.ok {{ color: #a6e3a1; font-weight: 700; }}
.box {{ border: 1px solid #45475a; border-radius: 8px; padding: 1em; background: #181825; }}
</style>
</head>
<body>
<h1>Intendant LAN Setup</h1>
{message}
<div class="box">
<p>Download and import these files on this browser/device.</p>
<p><a class="btn" href="/ca.crt">Download ca.crt</a><a class="btn" href="/client.p12">Download client.p12</a></p>
<p>Certificate password: <span class="pw">{}</span></p>
</div>

<h2>Next steps</h2>
<p>After importing the CA and client certificate, open the dashboard:<br><code>https://{lan_ip}:{https_port}</code></p>

<h2>Alternative downloads</h2>
<ul>
<li><a href="/client.pfx">client.pfx</a> — same file, Android-friendly extension</li>
</ul>

<p style="color:#a6adc8;margin-top:3em;font-size:.9em">Press Ctrl+C on the daemon host to stop this enrollment server.</p>
</body>
</html>
"#,
        escape_html(password)
    )
}

fn escape_html(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn print_client_setup_banner(lan_ip: &str, cert_port: u16, https_port: u16) {
    println!();
    println!("============================================================");
    println!("  Strict client enrollment");
    println!("============================================================");
    println!();
    println!("  On the client browser/device, open:");
    println!("    https://{lan_ip}:{cert_port}/");
    println!();
    println!("  The browser will warn because the Intendant CA is not");
    println!("  installed yet. Before entering any secret, inspect the");
    println!("  browser-observed server certificate and copy its SHA-256");
    println!("  fingerprint into this terminal.");
    println!();
    println!("  This terminal intentionally does not print the expected");
    println!("  fingerprint. It will only reveal a one-time enrollment");
    println!("  secret after the pasted fingerprint matches the live");
    println!("  Intendant server certificate.");
    println!();
    println!("  After enrollment, the dashboard lives at:");
    println!("    https://{lan_ip}:{https_port}");
    println!();
    instructions::print_all(lan_ip, cert_port);
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn fingerprint_input_accepts_browser_colon_uppercase_format() {
        let plain = "aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899";
        let colon_upper = "AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:
                           AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99";
        assert_eq!(normalize_fingerprint_input(colon_upper).unwrap(), plain);
    }

    #[test]
    fn fingerprint_input_rejects_labeled_page_text() {
        let err = normalize_fingerprint_input(
            "SHA-256 Fingerprint: aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899",
        )
        .unwrap_err();
        assert!(err.contains("chars"), "{err}");
    }

    #[test]
    fn enrollment_secret_is_redeemed_once() {
        let gate = EnrollmentGate::default();
        gate.arm_secret("secret".to_string());
        let token = gate
            .redeem_secret("secret")
            .expect("first redemption works");
        assert!(gate.has_session(&token));
        assert!(gate.redeem_secret("secret").is_none());
    }

    #[test]
    fn form_value_decodes_urlencoded_secret() {
        assert_eq!(
            form_value(b"secret=abc%2B123+xyz&other=nope", "secret").as_deref(),
            Some("abc+123 xyz")
        );
    }

    #[test]
    fn request_cookie_checks_session_token() {
        let gate = EnrollmentGate::default();
        gate.arm_secret("secret".to_string());
        let token = gate.redeem_secret("secret").unwrap();
        let req = HttpRequest {
            method: "GET".to_string(),
            path: "/client.p12".to_string(),
            headers: vec![(
                "Cookie".to_string(),
                format!("foo=bar; {ENROLL_COOKIE}={token}; theme=dark"),
            )],
            body: Vec::new(),
        };
        assert!(request_has_session(&req, &gate));
    }

    #[test]
    fn enrollment_tls_acceptor_builds_from_lan_server_cert() {
        let tmp = TempDir::new().unwrap();
        super::super::certs::ensure_certs(tmp.path(), "127.0.0.1", "enroll-test", false).unwrap();
        build_acceptor(&TlsCertSource::Files {
            cert_path: tmp.path().join("server.crt"),
            key_path: tmp.path().join("server.key"),
        })
        .expect("lan server cert/key should build an enrollment TLS acceptor");
    }

    #[tokio::test]
    async fn enrollment_handler_gates_downloads_behind_one_time_secret() {
        let tmp = TempDir::new().unwrap();
        let state =
            super::super::certs::ensure_certs(tmp.path(), "127.0.0.1", "enroll-test", false)
                .unwrap();
        let gate = Arc::new(EnrollmentGate::default());

        let blocked = exchange(
            "GET /client.p12 HTTP/1.1\r\nHost: localhost\r\n\r\n",
            tmp.path(),
            &state.p12_password,
            Arc::clone(&gate),
        )
        .await;
        let blocked_text = String::from_utf8_lossy(&blocked);
        assert!(blocked_text.starts_with("HTTP/1.1 403 Forbidden"));

        gate.arm_secret("secret".to_string());
        let unlock = exchange(
            "POST /enroll HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: 13\r\n\r\nsecret=secret",
            tmp.path(),
            &state.p12_password,
            Arc::clone(&gate),
        )
        .await;
        let unlock_text = String::from_utf8_lossy(&unlock);
        assert!(unlock_text.starts_with("HTTP/1.1 200 OK"));
        let token = extract_enrollment_cookie(&unlock_text).expect("unlock response sets cookie");

        let p12 = exchange(
            &format!(
                "GET /client.p12 HTTP/1.1\r\nHost: localhost\r\nCookie: {ENROLL_COOKIE}={token}\r\n\r\n"
            ),
            tmp.path(),
            &state.p12_password,
            Arc::clone(&gate),
        )
        .await;
        let p12_head = String::from_utf8_lossy(&p12[..p12.len().min(256)]);
        assert!(p12_head.starts_with("HTTP/1.1 200 OK"));
        assert!(p12_head.contains("Content-Type: application/x-pkcs12"));

        let second_unlock = exchange(
            "POST /enroll HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: 13\r\n\r\nsecret=secret",
            tmp.path(),
            &state.p12_password,
            Arc::clone(&gate),
        )
        .await;
        let second_text = String::from_utf8_lossy(&second_unlock);
        assert!(second_text.starts_with("HTTP/1.1 403 Forbidden"));
    }

    async fn exchange(
        request: &str,
        cert_dir: &Path,
        p12_password: &str,
        gate: Arc<EnrollmentGate>,
    ) -> Vec<u8> {
        let (mut client, server) = tokio::io::duplex(128 * 1024);
        let cert_dir = cert_dir.to_path_buf();
        let p12_password = p12_password.to_string();
        let task = tokio::spawn(async move {
            handle_conn(server, &cert_dir, &p12_password, "127.0.0.1", 8443, gate)
                .await
                .unwrap();
        });
        client.write_all(request.as_bytes()).await.unwrap();
        client.shutdown().await.unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        task.await.unwrap();
        response
    }

    fn extract_enrollment_cookie(response: &str) -> Option<String> {
        response.lines().find_map(|line| {
            let value = line.strip_prefix("Set-Cookie: ")?;
            let (cookie, _) = value.split_once(';')?;
            let (name, token) = cookie.split_once('=')?;
            (name == ENROLL_COOKIE).then(|| token.to_string())
        })
    }
}

//! nginx reverse-proxy config template shared by Linux and macOS backends.
//!
//! The only thing that varies between platforms is the path prefix where
//! the cert files live, which is passed in as `cert_dir`.

use std::path::Path;

/// Render the nginx site config for an mTLS reverse proxy in front of
/// the upstream `backend` address, listening on `https_port`, with certs
/// sourced from `cert_dir`.
///
/// Notes on the auth model:
/// - `ssl_verify_client optional` — Safari drops the client cert on
///   WebSocket upgrades, so we can't require it on every request. We
///   gate HTML with the cert check; the WS upgrade is waved through
///   because an unauthenticated browser never loaded the page that
///   could have initiated it.
/// - The CORS header echoes any allowed origin. This is a no-op for
///   single-daemon usage and lays the groundwork for the multi-host
///   dashboard, where one browser page may need to `fetch()` sibling
///   daemons under different origins.
pub fn render(cert_dir: &Path, backend: &str, https_port: u16) -> String {
    let cert_dir = cert_dir.display();
    format!(
        r#"map $http_upgrade $connection_upgrade {{
    default upgrade;
    '' close;
}}

server {{
    listen {https_port} ssl;

    ssl_certificate     {cert_dir}/server.crt;
    ssl_certificate_key {cert_dir}/server.key;

    # mTLS — clients with a cert signed by our CA get access.
    # "optional" so Safari WebSocket connections work (they don't send
    # client certs). The HTML page itself requires a valid cert to load,
    # so unauthenticated clients get no content to interact with.
    ssl_client_certificate {cert_dir}/ca.crt;
    ssl_verify_client optional;

    # Block requests without a valid client cert (except WebSocket upgrades,
    # which Safari sends without certs after the page is already loaded).
    set $auth_ok 0;
    if ($ssl_client_verify = SUCCESS) {{ set $auth_ok 1; }}
    if ($http_upgrade = websocket)    {{ set $auth_ok 1; }}
    if ($auth_ok = 0) {{ return 403; }}

    # CORS for future multi-host dashboard cross-origin fetches.
    # Harmless on single-daemon setups.
    add_header Access-Control-Allow-Origin  $http_origin always;
    add_header Access-Control-Allow-Credentials true always;
    add_header Access-Control-Allow-Headers  "Content-Type, Authorization" always;
    add_header Access-Control-Allow-Methods  "GET, POST, PUT, DELETE, OPTIONS" always;
    if ($request_method = OPTIONS) {{
        return 204;
    }}

    location / {{
        proxy_pass http://{backend};
        proxy_http_version 1.1;
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection $connection_upgrade;
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;

        # Keep WebSocket alive (24h)
        proxy_read_timeout 86400s;
        proxy_send_timeout 86400s;

        # Custom error page when intendant isn't running
        proxy_intercept_errors on;
        error_page 502 =502 @intendant_down;
    }}

    location @intendant_down {{
        default_type text/html;
        return 502 '<!DOCTYPE html><html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>Intendant</title><style>body{{background:#1e1e2e;color:#cdd6f4;font-family:system-ui,sans-serif;display:flex;align-items:center;justify-content:center;height:100vh;margin:0}}div{{text-align:center}}.status{{font-size:1.5em;margin-bottom:.5em;color:#f9e2af}}p{{color:#a6adc8}}</style></head><body><div><div class="status">Waiting for intendant</div><p>The agent is not running yet. Start it with:<br><code style="color:#89b4fa">intendant --web</code></p><script>setTimeout(()=>location.reload(),3000)</script></div></body></html>';
    }}
}}
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn renders_with_substitutions() {
        let out = render(&PathBuf::from("/etc/intendant-lan"), "127.0.0.1:8765", 8443);
        assert!(out.contains("listen 8443 ssl;"));
        assert!(out.contains("ssl_certificate     /etc/intendant-lan/server.crt;"));
        assert!(out.contains("proxy_pass http://127.0.0.1:8765;"));
    }

    #[test]
    fn includes_cors_headers() {
        let out = render(&PathBuf::from("/tmp/c"), "127.0.0.1:8765", 8443);
        assert!(out.contains("Access-Control-Allow-Origin"));
        assert!(out.contains("Access-Control-Allow-Credentials"));
    }
}

//! Per-platform client cert import walkthroughs.
//!
//! Straight port of the bash heredocs in `scripts/setup-lan.sh`.
//! Printed at the end of `intendant lan setup` so the user can pick
//! the right set of steps for whichever client device they're setting
//! up.

/// Print every platform block. Users glance at the list and follow
/// the one matching their device — mirrors the old script's layout.
pub fn print_all(lan_ip: &str, cert_port: u16) {
    println!();
    println!("------------------------------------------------------------");
    println!("  Client certificate installation");
    println!("------------------------------------------------------------");
    println!();
    println!("  Follow the steps for whichever device you're setting up.");
    println!();

    println!("  ── iPhone / iPad (Safari) ──");
    print_ios(lan_ip, cert_port);

    println!("  ── Android (Chrome) ──");
    print_android(lan_ip, cert_port);

    println!("  ── Desktop Firefox ──");
    print_firefox(lan_ip, cert_port);

    println!("  ── Desktop Chrome / Edge (Linux) ──");
    print_chrome_linux(lan_ip, cert_port);

    println!("  ── Desktop Chrome / Edge (macOS) ──");
    print_chrome_mac(lan_ip, cert_port);

    println!("  ── Desktop Chrome / Edge (Windows) ──");
    print_chrome_windows(lan_ip, cert_port);
}

fn print_ios(lan_ip: &str, port: u16) {
    println!();
    println!("    Step 1 — Pair in Safari:");
    println!("      Open Safari → https://{lan_ip}:{port}/");
    println!("      Copy the server certificate's SHA-256 fingerprint into");
    println!("      the Intendant terminal, then enter the enrollment secret.");
    println!();
    println!("    Step 2 — Install Apple profile:");
    println!("      Download intendant.mobileconfig from the unlocked pairing page.");
    println!("      Settings → General → VPN & Device Management → Install");
    println!("      The profile includes the CA and client identity.");
    println!("      Settings → General → About → Certificate Trust Settings → Enable");
    println!();
    println!("    Manual fallback:");
    println!("      Download ca.crt and client.p12 separately from the unlocked page.");
    println!("      Settings → General → About → Certificate Trust Settings → Enable");
    println!();
}

fn print_android(lan_ip: &str, port: u16) {
    println!();
    println!("    Step 1 — Pair in Chrome:");
    println!("      Open Chrome → https://{lan_ip}:{port}/");
    println!("      Copy the server certificate's SHA-256 fingerprint into");
    println!("      the Intendant terminal, then enter the enrollment secret.");
    println!();
    println!("    Step 2 — Install CA certificate:");
    println!("      Download ca.crt from the unlocked pairing page.");
    println!("      Settings → Security → Encryption & Credentials");
    println!("        → Install a certificate → CA certificate");
    println!();
    println!("    Step 3 — Install client certificate:");
    println!("      Download client.p12 from the unlocked pairing page.");
    println!("      Settings → Security → Encryption & Credentials");
    println!("        → Install a certificate → VPN & app user certificate");
    println!();
    println!("      (If .p12 doesn't work, use client.pfx from the same page.)");
    println!();
}

fn print_firefox(lan_ip: &str, port: u16) {
    println!();
    println!("    Step 1 — Pair in Firefox:");
    println!("      Open Firefox → https://{lan_ip}:{port}/");
    println!("      Copy the server certificate's SHA-256 fingerprint into");
    println!("      the Intendant terminal, then enter the enrollment secret.");
    println!();
    println!("    Step 2 — Install CA certificate:");
    println!("      Option A (import from browser):");
    println!("        Settings → Privacy & Security → Certificates → View Certificates");
    println!("        → Authorities tab → Import → select ca.crt");
    println!("        → Check \"Trust this CA to identify websites\"");
    println!();
    println!("      Option B (download):");
    println!("        Download ca.crt from the unlocked pairing page.");
    println!("        Firefox may prompt to trust it directly.");
    println!();
    println!("    Step 3 — Install client certificate:");
    println!("      Settings → Privacy & Security → Certificates → View Certificates");
    println!("      → Your Certificates tab → Import → select client.p12");
    println!();
    println!("      (Download client.p12 from the unlocked pairing page.)");
    println!();
}

fn print_chrome_linux(lan_ip: &str, port: u16) {
    println!();
    println!("    Step 1 — Pair in Chrome:");
    println!("      Open Chrome → https://{lan_ip}:{port}/");
    println!("      Copy the server certificate's SHA-256 fingerprint into");
    println!("      the Intendant terminal, then enter the enrollment secret.");
    println!("      Download ca.crt and client.p12 from the unlocked page.");
    println!();
    println!("    Step 2 — Install CA certificate (run in terminal):");
    println!("      certutil -d sql:$HOME/.pki/nssdb -A -t \"C,,\" \\");
    println!("        -n \"Intendant CA\" -i /path/to/downloaded/ca.crt");
    println!();
    println!(
        "      (Install libnss3-tools if certutil is missing: sudo apt install libnss3-tools)"
    );
    println!();
    println!("    Step 3 — Install client certificate (run in terminal):");
    println!("      pk12util -d sql:$HOME/.pki/nssdb -i /path/to/downloaded/client.p12");
    println!();
    println!("    Restart Chrome after importing.");
    println!();
}

fn print_chrome_mac(lan_ip: &str, port: u16) {
    println!();
    println!("    Step 1 — Pair in Chrome:");
    println!("      Open Chrome → https://{lan_ip}:{port}/");
    println!("      Copy the server certificate's SHA-256 fingerprint into");
    println!("      the Intendant terminal, then enter the enrollment secret.");
    println!();
    println!("    Step 2 — Install Apple profile:");
    println!("      Download intendant.mobileconfig from the unlocked pairing page.");
    println!("      System Settings → Privacy & Security → Profiles → Install");
    println!("      If needed, set the Intendant CA to Always Trust in Keychain Access.");
    println!();
    println!("    Manual fallback — install CA certificate:");
    println!("      Download ca.crt from the unlocked pairing page.");
    println!("      Double-click → opens Keychain Access → add to \"login\" keychain");
    println!("      Find \"Intendant CA\" → Get Info → Trust → \"Always Trust\"");
    println!();
    println!("    Manual fallback — install client certificate:");
    println!("      Download client.p12 from the unlocked pairing page.");
    println!();
    println!("      Double-click → opens Keychain Access → enter password");
    println!();
    println!("    Restart Chrome after importing.");
    println!();
}

fn print_chrome_windows(lan_ip: &str, port: u16) {
    println!();
    println!("    Step 1 — Pair in Chrome / Edge:");
    println!("      Open Chrome / Edge → https://{lan_ip}:{port}/");
    println!("      Copy the server certificate's SHA-256 fingerprint into");
    println!("      the Intendant terminal, then enter the enrollment secret.");
    println!();
    println!("    Step 2 — Install CA certificate:");
    println!("      Download ca.crt from the unlocked pairing page.");
    println!();
    println!("      Double-click → Install Certificate → Local Machine");
    println!("        → \"Trusted Root Certification Authorities\"");
    println!();
    println!("      Or via PowerShell (admin):");
    println!("        certutil.exe -addstore Root ca.crt");
    println!();
    println!("    Step 3 — Install client certificate:");
    println!("      Download client.p12 from the unlocked pairing page.");
    println!();
    println!("      Double-click → Import → enter password → place in \"Personal\"");
    println!();
    println!("    Restart Chrome after importing.");
    println!();
}

//! Sending a PDF to a printer on Linux: through the XDG desktop portal's
//! Print interface (the system print dialog; works in Flatpak too), or
//! straight to CUPS with `lp` when the portal isn't there.

use std::path::Path;
use std::process::Command;

/// What printing is possible here.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Printers {
    /// The desktop portal answers on its Print interface.
    pub portal: bool,
    /// CUPS's `lp` command works.
    pub lp: bool,
    /// CUPS destinations (`lpstat -e`), and the default one.
    pub list: Vec<String>,
    pub default: Option<String>,
}

/// CUPS destinations and the default, if CUPS's tools are installed.
pub fn cups_printers() -> Option<(Vec<String>, Option<String>)> {
    let out = Command::new("lpstat").arg("-e").output().ok()?;
    if !out.status.success() && out.stdout.is_empty() {
        // lpstat exists but there's no scheduler running.
        return Some((Vec::new(), None));
    }
    let list: Vec<String> = String::from_utf8_lossy(&out.stdout).lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect();
    let default = Command::new("lpstat").arg("-d").output().ok().and_then(|o| {
        let text = String::from_utf8_lossy(&o.stdout).to_string();
        // "system default destination: NAME"
        text.rsplit_once(':').map(|(_, n)| n.trim().to_string()).filter(|n| !n.is_empty() && list.contains(n))
    });
    Some((list, default))
}

/// Does the desktop portal offer printing?
#[cfg_attr(test, allow(dead_code))]
pub async fn portal_available() -> bool {
    ashpd::desktop::print::PrintProxy::new().await.is_ok()
}

/// Print through the portal: it shows the system print dialog, then
/// prints the file. Ok(false) if the user cancelled.
pub async fn print_with_portal(pdf: &Path, title: &str, copies: u32) -> Result<bool, String> {
    use ashpd::desktop::print::{PageSetup, PreparePrintOptions, PrintOptions, PrintProxy, Settings};
    let proxy = PrintProxy::new().await.map_err(|e| e.to_string())?;
    let prepared = proxy
        .prepare_print(None, title, Settings::default().set_n_copies(copies.max(1)), PageSetup::default(), PreparePrintOptions::default().set_modal(true))
        .await
        .map_err(|e| e.to_string())?;
    let prepared = match prepared.response() {
        Ok(p) => p,
        Err(ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled)) => return Ok(false),
        Err(e) => return Err(e.to_string()),
    };
    let file = std::fs::File::open(pdf).map_err(|e| e.to_string())?;
    let req = proxy.print(None, title, &file, PrintOptions::default().set_token(prepared.token).set_modal(true)).await.map_err(|e| e.to_string())?;
    match req.response() {
        Ok(()) => Ok(true),
        Err(ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled)) => Ok(false),
        Err(e) => Err(e.to_string()),
    }
}

/// The `lp` command line for a job (without running it).
pub fn lp_args(pdf: &Path, printer: Option<&str>, copies: u32, pages: &str, title: &str) -> Vec<String> {
    let mut args = Vec::new();
    if let Some(p) = printer {
        args.push("-d".into());
        args.push(p.to_string());
    }
    args.push("-n".into());
    args.push(copies.max(1).to_string());
    args.push("-t".into());
    args.push(title.to_string());
    let pages = pages.trim();
    if !pages.is_empty() && !pages.eq_ignore_ascii_case("all") {
        args.push("-P".into());
        args.push(pages.replace(' ', ""));
    }
    args.push("--".into());
    args.push(pdf.display().to_string());
    args
}

/// Send a PDF to CUPS. Returns lp's message ("request id is …").
pub fn print_with_lp(pdf: &Path, printer: Option<&str>, copies: u32, pages: &str, title: &str) -> Result<String, String> {
    let out = Command::new("lp").args(lp_args(pdf, printer, copies, pages, title)).output().map_err(|e| format!("couldn't run lp ({e}) — is CUPS installed?"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        Err(if err.is_empty() { "lp failed".into() } else { err })
    }
}

/// Check a page list like "1-3, 5" (empty or "all" means every page).
pub fn valid_pages(pages: &str, count: usize) -> Result<(), String> {
    let p = pages.trim();
    if p.is_empty() || p.eq_ignore_ascii_case("all") {
        return Ok(());
    }
    for part in p.split(',').map(str::trim) {
        let (a, b) = part.split_once('-').unwrap_or((part, part));
        let (a, b) = (a.trim().parse::<usize>(), b.trim().parse::<usize>());
        match (a, b) {
            (Ok(a), Ok(b)) if a >= 1 && a <= b && b <= count => {}
            _ => return Err(format!("“{part}” isn't a page range in 1–{count}")),
        }
    }
    Ok(())
}

/// Why printing isn't possible, for the dialog.
pub const NO_PRINTING: &str = "No printing system was found: neither the desktop print portal nor CUPS (lp) is available. Install CUPS (or a desktop portal with printing), or use Export as PDF… and print the file elsewhere.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_lp_command_lines() {
        let args = lp_args(Path::new("/tmp/x.pdf"), Some("Office"), 2, "1-3, 5", "Book");
        assert_eq!(args, vec!["-d", "Office", "-n", "2", "-t", "Book", "-P", "1-3,5", "--", "/tmp/x.pdf"]);
        let args = lp_args(Path::new("/tmp/x.pdf"), None, 0, "all", "B");
        assert_eq!(args, vec!["-n", "1", "-t", "B", "--", "/tmp/x.pdf"]);
    }

    #[test]
    fn checks_page_lists() {
        assert!(valid_pages("", 3).is_ok() && valid_pages("1-2,3", 3).is_ok());
        assert!(valid_pages("4", 3).is_err() && valid_pages("2-1", 3).is_err() && valid_pages("x", 3).is_err());
    }
}

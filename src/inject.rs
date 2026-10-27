//! Deliver one clipboard paste to the window captured at PTT release.
//! Never stream characters into whichever window happens to have focus.
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct WindowTarget {
    pub address: String,
    pub pid: u32,
    #[serde(default, rename = "stableId")]
    pub stable_id: String,
    #[serde(default)]
    pub class: String,
}
impl WindowTarget {
    fn valid(&self) -> bool {
        self.pid > 0 && self.address.strip_prefix("0x").is_some_and(|s| {
            !s.is_empty() && s != "0" && s.bytes().all(|c| c.is_ascii_hexdigit())
        })
    }
    fn same_window(&self, other: &Self) -> bool {
        self.address == other.address && self.pid == other.pid
            && (self.stable_id.is_empty() || self.stable_id == other.stable_id)
    }
    fn paste_modifiers(&self) -> &'static str {
        let class = self.class.to_ascii_lowercase();
        if ["kitty", "alacritty", "foot", "wezterm", "ghostty", "konsole", "gnome.terminal", "gnome-terminal", "tilix", "xterm"]
            .iter().any(|name| class.contains(name)) { "CTRL SHIFT" } else { "CTRL" }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Delivery { Pasted, ClipboardOnly }

/// Called by the input-reader thread, before transcription or the owner queue.
pub fn capture_target() -> Option<WindowTarget> {
    match Hyprland.active_window() {
        Ok(target) => target,
        Err(e) => { eprintln!("[whisp] cannot capture destination: {e:#}"); None }
    }
}

pub fn inject_to(text: &str, target: Option<&WindowTarget>, wait_clear: impl Fn() -> bool) -> Result<Delivery> {
    let start = Instant::now();
    while !wait_clear() {
        if start.elapsed() >= Duration::from_secs(5) {
            Hyprland.copy(&format!("{text} "))?;
            notify("Keys are still held. Your transcript was copied instead of pasted.");
            return Ok(Delivery::ClipboardOnly);
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let result = deliver_with(&Hyprland, text, target)?;
    if result == Delivery::ClipboardOnly {
        notify("The destination window is unavailable. Your transcript was copied instead of pasted.");
    }
    Ok(result)
}

trait Desktop {
    fn windows(&self) -> Result<Vec<WindowTarget>>;
    fn active_window(&self) -> Result<Option<WindowTarget>>;
    fn copy(&self, text: &str) -> Result<()>;
    fn paste(&self, target: &WindowTarget, restore: Option<&WindowTarget>) -> Result<()>;
}
fn deliver_with(desktop: &impl Desktop, text: &str, target: Option<&WindowTarget>) -> Result<Delivery> {
    // Keep the transcript recoverable if the app closed. There is deliberately
    // no fallback to an unaddressed paste into the currently focused window.
    desktop.copy(&format!("{text} "))?;
    let Some(target) = target.filter(|t| t.valid()) else { return Ok(Delivery::ClipboardOnly); };
    let windows = desktop.windows()?;
    if !windows.iter().any(|w| target.same_window(w)) { return Ok(Delivery::ClipboardOnly); }
    let restore = desktop.active_window()?.filter(|w| {
        w.valid() && !target.same_window(w) && windows.iter().any(|v| w.same_window(v))
    });
    desktop.paste(target, restore.as_ref())?;
    Ok(Delivery::Pasted)
}

struct Hyprland;
impl Desktop for Hyprland {
    fn windows(&self) -> Result<Vec<WindowTarget>> {
        serde_json::from_str(&request("j/clients")?).context("decoding Hyprland windows")
    }
    fn active_window(&self) -> Result<Option<WindowTarget>> {
        let value: serde_json::Value = serde_json::from_str(&request("j/activewindow")?)?;
        if value.as_object().is_none_or(|v| v.is_empty()) { return Ok(None); }
        let window: WindowTarget = serde_json::from_value(value)?;
        Ok(window.valid().then_some(window))
    }
    fn copy(&self, text: &str) -> Result<()> {
        let mut child = Command::new("wl-copy").args(["--type", "text/plain;charset=utf-8"])
            .stdin(Stdio::piped()).stdout(Stdio::null()).spawn().context("starting wl-copy")?;
        child.stdin.take().context("clipboard stdin unavailable")?.write_all(text.as_bytes())?;
        anyhow::ensure!(child.wait()?.success(), "wl-copy failed");
        Ok(())
    }
    fn paste(&self, target: &WindowTarget, restore: Option<&WindowTarget>) -> Result<()> {
        // One compositor batch, with an explicit address on every operation.
        // Focusing first also handles XWayland; restore the user's newer focus.
        let mut commands = vec![
            format!("dispatch focuswindow address:{}", target.address),
            format!("dispatch sendshortcut {},V,address:{}", target.paste_modifiers(), target.address),
        ];
        if let Some(w) = restore { commands.push(format!("dispatch focuswindow address:{}", w.address)); }
        let reply = request(&format!("[[BATCH]]{}", commands.join(";")))?;
        let replies: Vec<_> = reply.lines().filter(|l| !l.trim().is_empty()).collect();
        anyhow::ensure!(replies.len() == commands.len() && replies.iter().all(|l| l.trim() == "ok"), "targeted paste failed: {reply}");
        Ok(())
    }
}

fn request(command: &str) -> Result<String> {
    let runtime = std::env::var("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR missing")?;
    let signature = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").context("HYPRLAND_INSTANCE_SIGNATURE missing")?;
    let path = std::path::Path::new(&runtime).join("hypr").join(signature).join(".socket.sock");
    let mut stream = UnixStream::connect(path).context("connecting to Hyprland")?;
    stream.set_read_timeout(Some(Duration::from_millis(500)))?;
    stream.set_write_timeout(Some(Duration::from_millis(500)))?;
    stream.write_all(command.as_bytes())?;
    stream.shutdown(std::net::Shutdown::Write)?;
    let mut reply = String::new();
    (&mut stream).take(4 * 1024 * 1024).read_to_string(&mut reply)?;
    Ok(reply)
}
fn notify(message: &str) { let _ = Command::new("notify-send").args(["Whisp", message]).spawn(); }

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    struct FakeDesktop {
        windows: Vec<WindowTarget>, active: Option<WindowTarget>,
        clipboard: RefCell<String>, delivered: RefCell<Vec<(String, Option<String>)>>,
    }
    impl Desktop for FakeDesktop {
        fn windows(&self) -> Result<Vec<WindowTarget>> { Ok(self.windows.clone()) }
        fn active_window(&self) -> Result<Option<WindowTarget>> { Ok(self.active.clone()) }
        fn copy(&self, text: &str) -> Result<()> { *self.clipboard.borrow_mut() = text.to_string(); Ok(()) }
        fn paste(&self, t: &WindowTarget, r: Option<&WindowTarget>) -> Result<()> {
            self.delivered.borrow_mut().push((t.address.clone(), r.map(|w| w.address.clone()))); Ok(())
        }
    }
    fn window(address: &str, id: &str) -> WindowTarget {
        WindowTarget { address: address.into(), pid: 50, stable_id: id.into(), class: "brave-browser".into() }
    }
    fn fake(windows: Vec<WindowTarget>, active: Option<WindowTarget>) -> FakeDesktop {
        FakeDesktop { windows, active, clipboard: RefCell::new(String::new()), delivered: RefCell::new(vec![]) }
    }
    #[test]
    fn changing_focus_during_transcription_does_not_redirect_text() {
        let a = window("0xabc", "original"); let b = window("0xdef", "new-focus");
        let d = fake(vec![a.clone(), b.clone()], Some(b));
        assert_eq!(deliver_with(&d, "one utterance", Some(&a)).unwrap(), Delivery::Pasted);
        assert_eq!(&*d.delivered.borrow(), &[("0xabc".into(), Some("0xdef".into()))]);
        assert_eq!(&*d.clipboard.borrow(), "one utterance ");
    }
    #[test]
    fn closed_destination_never_pastes_into_current_window() {
        let a = window("0xabc", "closed"); let b = window("0xdef", "new");
        let d = fake(vec![b.clone()], Some(b));
        assert_eq!(deliver_with(&d, "recoverable text", Some(&a)).unwrap(), Delivery::ClipboardOnly);
        assert!(d.delivered.borrow().is_empty()); assert_eq!(&*d.clipboard.borrow(), "recoverable text ");
    }
    #[test]
    fn reused_address_is_not_the_original_window() {
        let a = window("0xabc", "old"); let b = window("0xabc", "new"); let d = fake(vec![b.clone()], Some(b));
        assert_eq!(deliver_with(&d, "text", Some(&a)).unwrap(), Delivery::ClipboardOnly);
        assert!(d.delivered.borrow().is_empty());
    }
    #[test]
    fn unknown_destination_keeps_text_without_typing() {
        let d = fake(vec![], None);
        assert_eq!(deliver_with(&d, "text", None).unwrap(), Delivery::ClipboardOnly);
        assert!(d.delivered.borrow().is_empty());
    }
    #[test]
    fn correct_shortcuts_and_safe_addresses() {
        let mut t = window("0xabc", "term"); t.class = "kitty".into();
        assert_eq!(t.paste_modifiers(), "CTRL SHIFT");
        assert_eq!(window("0xdef", "browser").paste_modifiers(), "CTRL");
        assert!(!window("0xabc;dispatch exit", "bad").valid());
    }
}

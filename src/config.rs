//! Apps fixados na dock: `~/.config/rdock/pinned` (um ID .desktop por linha).

use std::path::PathBuf;

use crate::apps::App;

fn path() -> PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::apps::home().join(".config"));
    base.join("rdock").join("pinned")
}

pub fn load_pinned(apps: &[App]) -> Vec<String> {
    if let Ok(text) = std::fs::read_to_string(path()) {
        return text.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect();
    }
    // Primeira execução: um gerenciador de arquivos, um navegador, um terminal + favoritos do Cinnamon.
    let groups: [&[&str]; 3] = [
        &["nemo.desktop", "org.gnome.Nautilus.desktop", "thunar.desktop", "caja.desktop"],
        &["firefox.desktop", "google-chrome.desktop", "chromium.desktop", "brave-browser.desktop"],
        &["org.gnome.Terminal.desktop", "xfce4-terminal.desktop", "mate-terminal.desktop", "Alacritty.desktop"],
    ];
    let installed = |id: &str| apps.iter().any(|a| a.id == id);
    let mut ids: Vec<String> =
        groups.iter().filter_map(|g| g.iter().find(|id| installed(id)).map(|s| s.to_string())).collect();
    if let Ok(out) = std::process::Command::new("gsettings").args(["get", "org.cinnamon", "favorite-apps"]).output() {
        let s = String::from_utf8_lossy(&out.stdout);
        ids.extend(s.split('\'').filter(|p| p.ends_with(".desktop")).map(String::from));
    }
    let mut seen = std::collections::HashSet::new();
    ids.retain(|id| apps.iter().any(|a| &a.id == id) && seen.insert(id.clone()));
    ids
}

pub fn save_pinned(ids: &[String]) {
    let p = path();
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = p.with_extension("tmp");
    if std::fs::write(&tmp, ids.join("\n") + "\n").is_ok() {
        let _ = std::fs::rename(&tmp, &p);
    }
}

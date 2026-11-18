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
    let tmp = p.with_extension("pinned.tmp");
    if std::fs::write(&tmp, ids.join("\n") + "\n").is_ok() {
        let _ = std::fs::rename(&tmp, &p);
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Edge {
    Left,
    Bottom,
    Right,
}

/// Quando a dock se esconde.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HideMode {
    Never,
    /// Sempre que o mouse não está nela.
    Always,
    /// Só quando a janela ativa encosta nela (como os painéis do Cinnamon).
    Smart,
}

/// Preferências da dock: `~/.config/rdock/settings` (chave=valor).
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Settings {
    pub icon_size: f32,
    pub magnification: bool,
    pub max_size: f32,
    pub edge: Edge,
    pub hide: HideMode,
    pub bounce: bool,
    pub indicators: bool,
    /// Multiplica o tamanho de todo texto da interface.
    pub text_scale: f32,
}

pub const ICON_MIN: f32 = 32.0;
pub const ICON_MAX: f32 = 96.0;
pub const MAG_MIN: f32 = 48.0;
pub const MAG_MAX: f32 = 128.0;
pub const TEXT_MIN: f32 = 1.0;
pub const TEXT_MAX: f32 = 1.8;

impl Default for Settings {
    fn default() -> Self {
        Settings { icon_size: 52.0, magnification: true, max_size: 96.0, edge: Edge::Bottom, hide: HideMode::Never, bounce: true, indicators: true, text_scale: 1.3 }
    }
}

impl Settings {
    /// Fator máximo de ampliação efetivo (1 = sem ampliação).
    pub fn max_scale(&self) -> f32 {
        if self.magnification { self.max_size.max(self.icon_size) / self.icon_size } else { 1.0 }
    }
}

fn settings_path() -> PathBuf {
    path().with_file_name("settings")
}

pub fn load_settings() -> Settings {
    let mut s = Settings::default();
    let Ok(text) = std::fs::read_to_string(settings_path()) else { return s };
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else { continue };
        let (k, v) = (k.trim(), v.trim());
        let num = |d: f32| v.parse::<f32>().unwrap_or(d);
        let flag = |d: bool| v.parse::<bool>().unwrap_or(d);
        match k {
            "icon_size" => s.icon_size = num(s.icon_size).clamp(ICON_MIN, ICON_MAX).round(),
            "magnification" => s.magnification = flag(s.magnification),
            "max_size" => s.max_size = num(s.max_size).clamp(MAG_MIN, MAG_MAX).round(),
            "position" => {
                s.edge = match v {
                    "left" => Edge::Left,
                    "right" => Edge::Right,
                    _ => Edge::Bottom,
                }
            }
            // compatibilidade com a versão anterior
            "autohide" if flag(false) => s.hide = HideMode::Always,
            "hide" => {
                s.hide = match v {
                    "always" => HideMode::Always,
                    "smart" => HideMode::Smart,
                    _ => HideMode::Never,
                }
            }
            "bounce" => s.bounce = flag(s.bounce),
            "indicators" => s.indicators = flag(s.indicators),
            "text_scale" => s.text_scale = num(s.text_scale).clamp(TEXT_MIN, TEXT_MAX),
            _ => {}
        }
    }
    s
}

pub fn save_settings(s: &Settings) {
    let edge = match s.edge {
        Edge::Left => "left",
        Edge::Bottom => "bottom",
        Edge::Right => "right",
    };
    let hide = match s.hide {
        HideMode::Never => "never",
        HideMode::Always => "always",
        HideMode::Smart => "smart",
    };
    let text = format!(
        "icon_size={}\nmagnification={}\nmax_size={}\nposition={edge}\nhide={hide}\nbounce={}\nindicators={}\ntext_scale={:.2}\n",
        s.icon_size, s.magnification, s.max_size, s.bounce, s.indicators, s.text_scale
    );
    let p = settings_path();
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = p.with_extension("settings.tmp");
    if std::fs::write(&tmp, text).is_ok() {
        let _ = std::fs::rename(&tmp, &p);
    }
}

/// Tema da prateleira: o vidro embutido ou um PNG de uma pasta de temas.
#[derive(Clone, Debug)]
pub struct Theme {
    pub name: String,
    pub path: Option<PathBuf>,
}

pub const DEFAULT_THEME: &str = "Vidro";

/// Pasta onde o usuário coloca seus temas.
pub fn user_theme_dir() -> PathBuf {
    path().with_file_name("themes")
}

/// Temas disponíveis: o embutido primeiro, depois os PNGs de `~/.config/rdock/themes`
/// e `~/.local/share/rdock/themes` (o primeiro nome encontrado vence).
pub fn list_themes() -> Vec<Theme> {
    let mut found: Vec<Theme> = Vec::new();
    let dirs = [user_theme_dir(), crate::apps::data_dirs()[0].join("rdock/themes")];
    for dir in dirs {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for p in rd.flatten().map(|e| e.path()) {
            if !p.extension().is_some_and(|e| e.eq_ignore_ascii_case("png")) {
                continue;
            }
            let name = p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            if !name.is_empty() && name != DEFAULT_THEME && !found.iter().any(|t| t.name == name) {
                found.push(Theme { name, path: Some(p) });
            }
        }
    }
    found.sort_by_key(|t| crate::apps::normalize(&t.name));
    found.insert(0, Theme { name: DEFAULT_THEME.into(), path: None });
    found
}

fn theme_file() -> PathBuf {
    path().with_file_name("theme")
}

pub fn load_theme_name() -> String {
    std::fs::read_to_string(theme_file()).map(|s| s.trim().to_string()).ok().filter(|s| !s.is_empty()).unwrap_or_else(|| DEFAULT_THEME.into())
}

pub fn save_theme_name(name: &str) {
    let p = theme_file();
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(p, format!("{name}\n"));
}

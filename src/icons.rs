//! Resolução de ícones pelo tema do sistema (spec XDG Icon Theme) e rasterização SVG/PNG.

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use tiny_skia::{
    Color, FillRule, FilterQuality, GradientStop, LinearGradient, Paint, Pixmap, PixmapPaint, Point,
    SpreadMode, Transform,
};

use crate::draw::rrect;
use crate::Msg;

pub const RES: u32 = 128;

/// Ícone pré-rasterizado em duas resoluções (evita aliasing ao reduzir) e uma versão escurecida (clique).
pub struct Icon {
    pub big: Pixmap,
    pub small: Pixmap,
    pub dark: Pixmap,
}

impl Icon {
    pub fn new(big: Pixmap) -> Icon {
        let small = fit(&big, RES / 2);
        let mut dark = big.clone();
        for px in dark.data_mut().chunks_exact_mut(4) {
            for c in &mut px[..3] {
                *c = (*c as f32 * 0.55) as u8;
            }
        }
        Icon { big, small, dark }
    }
}

struct Dir {
    path: PathBuf,
    size: u32,
    scalable: bool,
}

pub struct Theme {
    chain: Vec<Vec<Dir>>,
}

fn theme_name() -> String {
    for schema in ["org.cinnamon.desktop.interface", "org.gnome.desktop.interface", "org.mate.interface"] {
        if let Ok(out) = std::process::Command::new("gsettings").args(["get", schema, "icon-theme"]).output() {
            let s = String::from_utf8_lossy(&out.stdout).trim().trim_matches('\'').to_string();
            if out.status.success() && !s.is_empty() {
                return s;
            }
        }
    }
    "hicolor".into()
}

fn bases() -> Vec<PathBuf> {
    let mut v = vec![crate::apps::home().join(".icons")];
    v.extend(crate::apps::data_dirs().into_iter().map(|d| d.join("icons")));
    v
}

impl Theme {
    pub fn load() -> Theme {
        let bases = bases();
        let mut chain = Vec::new();
        let mut visited = HashSet::new();
        let mut queue = VecDeque::from([theme_name()]);
        while let Some(name) = queue.pop_front() {
            if !visited.insert(name.clone()) {
                continue;
            }
            let (dirs, inherits) = load_theme(&bases, &name);
            if !dirs.is_empty() {
                chain.push(dirs);
            }
            queue.extend(inherits);
            if queue.is_empty() && !visited.contains("hicolor") {
                queue.push_back("hicolor".into());
            }
        }
        Theme { chain }
    }

    pub fn find(&self, name: &str) -> Option<PathBuf> {
        if name.starts_with('/') {
            let p = PathBuf::from(name);
            return p.exists().then_some(p);
        }
        let name = name.trim_end_matches(".png").trim_end_matches(".svg");
        for dirs in &self.chain {
            let mut best: Option<(u32, PathBuf)> = None;
            for d in dirs {
                for ext in ["svg", "png"] {
                    let p = d.path.join(format!("{name}.{ext}"));
                    if !p.exists() {
                        continue;
                    }
                    let score = match (ext, d.scalable) {
                        (_, true) => 9256,
                        ("svg", _) if d.size >= 48 => 9000 + d.size.min(512),
                        (_, _) if d.size >= RES => 5000 - (d.size - RES).min(4000),
                        _ => d.size,
                    };
                    if best.as_ref().is_none_or(|(s, _)| score > *s) {
                        best = Some((score, p));
                    }
                }
            }
            if let Some((_, p)) = best {
                return Some(p);
            }
        }
        for ext in ["svg", "png"] {
            let p = Path::new("/usr/share/pixmaps").join(format!("{name}.{ext}"));
            if p.exists() {
                return Some(p);
            }
        }
        None
    }
}

fn load_theme(bases: &[PathBuf], name: &str) -> (Vec<Dir>, Vec<String>) {
    let mut dirs = Vec::new();
    let mut inherits = Vec::new();
    let Some(index) = bases.iter().map(|b| b.join(name).join("index.theme")).find(|p| p.exists()) else {
        return (dirs, inherits);
    };
    let text = std::fs::read_to_string(&index).unwrap_or_default();
    let mut section = String::new();
    let mut list: Vec<String> = Vec::new();
    let mut info: std::collections::HashMap<String, (u32, bool)> = Default::default();
    for line in text.lines() {
        let l = line.trim();
        if let Some(s) = l.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            section = s.to_string();
            continue;
        }
        let Some((k, v)) = l.split_once('=') else { continue };
        let (k, v) = (k.trim(), v.trim());
        if section == "Icon Theme" {
            match k {
                "Inherits" => inherits = v.split(',').map(|s| s.trim().to_string()).collect(),
                "Directories" | "ScaledDirectories" => list.extend(v.split(',').map(|s| s.trim().to_string())),
                _ => {}
            }
        } else {
            let e = info.entry(section.clone()).or_insert((0, false));
            match k {
                "Size" => e.0 = v.parse().unwrap_or(0),
                "Type" => e.1 = v == "Scalable",
                _ => {}
            }
        }
    }
    for base in bases {
        let root = base.join(name);
        if !root.is_dir() {
            continue;
        }
        for d in &list {
            let path = root.join(d);
            if path.is_dir() {
                let (size, scalable) = info.get(d).copied().unwrap_or((0, false));
                dirs.push(Dir { path, size, scalable });
            }
        }
    }
    (dirs, inherits)
}

/// Redimensiona mantendo proporção, centralizado num quadrado `res`×`res`.
pub fn fit(src: &Pixmap, res: u32) -> Pixmap {
    let mut out = Pixmap::new(res, res).unwrap();
    let scale = res as f32 / src.width().max(src.height()) as f32;
    let tx = (res as f32 - src.width() as f32 * scale) / 2.0;
    let ty = (res as f32 - src.height() as f32 * scale) / 2.0;
    let paint = PixmapPaint { quality: FilterQuality::Bicubic, ..Default::default() };
    out.draw_pixmap(0, 0, src.as_ref(), &paint, Transform::from_row(scale, 0.0, 0.0, scale, tx, ty), None);
    out
}

fn rasterize(path: &Path) -> Option<Pixmap> {
    let data = std::fs::read(path).ok()?;
    let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    if ext == "svg" || ext == "svgz" {
        let tree = resvg::usvg::Tree::from_data(&data, &resvg::usvg::Options::default()).ok()?;
        let s = tree.size();
        let scale = RES as f32 / s.width().max(s.height());
        let tx = (RES as f32 - s.width() * scale) / 2.0;
        let ty = (RES as f32 - s.height() * scale) / 2.0;
        let mut p = Pixmap::new(RES, RES)?;
        resvg::render(&tree, Transform::from_row(scale, 0.0, 0.0, scale, tx, ty), &mut p.as_mut());
        Some(p)
    } else {
        Pixmap::decode_png(&data).ok().map(|p| fit(&p, RES))
    }
}

pub fn gradient_paint(top: Color, bottom: Color, y0: f32, y1: f32) -> Paint<'static> {
    let shader = LinearGradient::new(
        Point::from_xy(0.0, y0),
        Point::from_xy(0.0, y1),
        vec![GradientStop::new(0.0, top), GradientStop::new(1.0, bottom)],
        SpreadMode::Pad,
        Transform::identity(),
    )
    .unwrap();
    Paint { shader, anti_alias: true, ..Default::default() }
}

/// Ícone do Launchpad desenhado proceduralmente: squircle escuro com grade 3×3 colorida.
pub fn launchpad_icon() -> Pixmap {
    let s = RES as f32;
    let mut p = Pixmap::new(RES, RES).unwrap();
    let m = s * 0.06;
    let bg = rrect(m, m, s - 2.0 * m, s - 2.0 * m, s * 0.22).unwrap();
    let paint = gradient_paint(Color::from_rgba8(112, 118, 132, 255), Color::from_rgba8(38, 41, 50, 255), 0.0, s);
    p.fill_path(&bg, &paint, FillRule::Winding, Transform::identity(), None);
    let colors = [
        (255, 69, 58), (255, 159, 10), (255, 214, 10),
        (48, 209, 88), (100, 210, 255), (10, 132, 255),
        (94, 92, 230), (191, 90, 242), (255, 55, 95),
    ];
    let cell = s * 0.17;
    let gap = s * 0.065;
    let start = (s - (3.0 * cell + 2.0 * gap)) / 2.0;
    for (i, (r, g, b)) in colors.iter().enumerate() {
        let x = start + (i % 3) as f32 * (cell + gap);
        let y = start + (i / 3) as f32 * (cell + gap);
        let sq = rrect(x, y, cell, cell, cell * 0.3).unwrap();
        let light = |c: u8| (c as u16 + 50).min(255) as u8;
        let paint = gradient_paint(
            Color::from_rgba8(light(*r), light(*g), light(*b), 255),
            Color::from_rgba8(*r, *g, *b, 255),
            y,
            y + cell,
        );
        p.fill_path(&sq, &paint, FillRule::Winding, Transform::identity(), None);
    }
    p
}

/// Ícone genérico quando o tema não tem nenhum.
pub fn generic_icon() -> Pixmap {
    let s = RES as f32;
    let mut p = Pixmap::new(RES, RES).unwrap();
    let m = s * 0.08;
    let bg = rrect(m, m, s - 2.0 * m, s - 2.0 * m, s * 0.2).unwrap();
    let paint = gradient_paint(Color::from_rgba8(150, 156, 168, 255), Color::from_rgba8(84, 90, 102, 255), 0.0, s);
    p.fill_path(&bg, &paint, FillRule::Winding, Transform::identity(), None);
    let win = rrect(s * 0.27, s * 0.3, s * 0.46, s * 0.4, s * 0.05).unwrap();
    let mut white = Paint::default();
    white.set_color_rgba8(255, 255, 255, 220);
    white.anti_alias = true;
    p.fill_path(&win, &white, FillRule::Winding, Transform::identity(), None);
    p
}

pub fn from_argb(w: u32, h: u32, data: &[u32]) -> Option<Pixmap> {
    let mut p = Pixmap::new(w, h)?;
    for (px, &argb) in p.data_mut().chunks_exact_mut(4).zip(data) {
        let a = (argb >> 24) & 0xff;
        let pm = |c: u32| ((c & 0xff) * a / 255) as u8;
        px.copy_from_slice(&[pm(argb >> 16), pm(argb >> 8), pm(argb), a as u8]);
    }
    Some(fit(&p, RES))
}

/// Carrega ícones em threads de fundo, na ordem pedida (ícones da dock primeiro).
/// Carrega ícones em segundo plano. Uma chave pode ser:
/// - um nome do tema, ou vários separados por `|` (vale o primeiro encontrado);
/// - `thumb:<caminho>`: miniatura de uma imagem PNG/JPEG;
/// - `@launchpad`: o ícone desenhado da grade de apps.
pub fn spawn_loader(names: Vec<String>, tx: Sender<Msg>) {
    static THEME: std::sync::OnceLock<Arc<Theme>> = std::sync::OnceLock::new();
    let theme = THEME.get_or_init(|| Arc::new(Theme::load())).clone();
    let threads = names.len().clamp(1, 4);
    let queue = Arc::new(Mutex::new(VecDeque::from(names)));
    for _ in 0..threads {
        let (theme, queue, tx) = (theme.clone(), queue.clone(), tx.clone());
        std::thread::spawn(move || loop {
            let Some(name) = queue.lock().unwrap().pop_front() else { break };
            let pix = if name == "@launchpad" {
                launchpad_icon()
            } else if let Some(path) = name.strip_prefix("thumb:") {
                thumbnail(std::path::Path::new(path)).unwrap_or_else(generic_icon)
            } else {
                name.split('|')
                    .find_map(|n| theme.find(n))
                    .and_then(|p| rasterize(&p))
                    .or_else(|| theme.find("application-x-executable").and_then(|p| rasterize(&p)))
                    .unwrap_or_else(generic_icon)
            };
            if tx.send(Msg::Icon(name, Icon::new(pix))).is_err() {
                break;
            }
        });
    }
}

/// Miniatura (até 256 px) de uma imagem PNG ou JPEG.
fn thumbnail(path: &std::path::Path) -> Option<Pixmap> {
    let data = std::fs::read(path).ok()?;
    let pix = if data.starts_with(&[0x89, b'P', b'N', b'G']) {
        Pixmap::decode_png(&data).ok()?
    } else {
        let mut d = zune_jpeg::JpegDecoder::new(&data[..]);
        let rgb = d.decode().ok()?;
        let info = d.info()?;
        let (w, h) = (info.width as u32, info.height as u32);
        let mut p = Pixmap::new(w, h)?;
        for (px, c) in p.data_mut().chunks_exact_mut(4).zip(rgb.chunks_exact(3)) {
            px.copy_from_slice(&[c[0], c[1], c[2], 255]);
        }
        p
    };
    // Reduz pela maior dimensão, mantendo a proporção.
    let k = (256.0 / pix.width().max(pix.height()) as f32).min(1.0);
    let (w, h) = (((pix.width() as f32 * k).round() as u32).max(1), ((pix.height() as f32 * k).round() as u32).max(1));
    let mut out = Pixmap::new(w, h)?;
    let paint = tiny_skia::PixmapPaint { quality: tiny_skia::FilterQuality::Bicubic, ..Default::default() };
    out.draw_pixmap(0, 0, pix.as_ref(), &paint, Transform::from_scale(k, k), None);
    Some(out)
}

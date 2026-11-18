//! Primitivas de desenho: retângulos arredondados, sombras, ícones e texto.

use std::collections::HashMap;

use ab_glyph::{point, Font, FontVec, GlyphId, PxScale, ScaleFont};
use tiny_skia::{
    BlendMode, FillRule, FilterQuality, Paint, Path, PathBuilder, Pixmap, PixmapPaint, Stroke, Transform,
};

use crate::icons::Icon;

pub fn rrect(x: f32, y: f32, w: f32, h: f32, r: f32) -> Option<Path> {
    let r = r.min(w / 2.0).min(h / 2.0).max(0.0);
    let k = 0.5523 * r;
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.cubic_to(x + w - r + k, y, x + w, y + r - k, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.cubic_to(x + w, y + h - r + k, x + w - r + k, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.cubic_to(x + r - k, y + h, x, y + h - r + k, x, y + h - r);
    pb.line_to(x, y + r);
    pb.cubic_to(x, y + r - k, x + r - k, y, x + r, y);
    pb.close();
    pb.finish()
}

pub fn color(r: u8, g: u8, b: u8, a: f32) -> Paint<'static> {
    let mut p = Paint::default();
    p.set_color_rgba8(r, g, b, (a.clamp(0.0, 1.0) * 255.0) as u8);
    p.anti_alias = true;
    p
}

pub fn fill(pix: &mut Pixmap, path: &Option<Path>, paint: &Paint, tf: Transform) {
    if let Some(p) = path {
        pix.fill_path(p, paint, FillRule::Winding, tf, None);
    }
}

pub fn stroke(pix: &mut Pixmap, path: &Option<Path>, paint: &Paint, width: f32, tf: Transform) {
    if let Some(p) = path {
        let s = Stroke { width, ..Default::default() };
        pix.stroke_path(p, paint, &s, tf, None);
    }
}

pub fn circle(pix: &mut Pixmap, cx: f32, cy: f32, r: f32, paint: &Paint) {
    if let Some(p) = PathBuilder::from_circle(cx, cy, r) {
        pix.fill_path(&p, paint, FillRule::Winding, Transform::identity(), None);
    }
}

#[allow(clippy::too_many_arguments)]
/// Sombra suave barata: camadas concêntricas de baixa opacidade.
pub fn shadow(pix: &mut Pixmap, x: f32, y: f32, w: f32, h: f32, r: f32, strength: f32, tf: Transform) {
    shadow_n(pix, x, y, w, h, r, strength, tf, 10);
}

#[allow(clippy::too_many_arguments)]
/// Sombra com `n` camadas (menos camadas = mais barata, para elementos pequenos redesenhados todo quadro).
pub fn shadow_n(pix: &mut Pixmap, x: f32, y: f32, w: f32, h: f32, r: f32, strength: f32, tf: Transform, n: usize) {
    let step = 16.0 / n as f32;
    for i in (1..=n).rev() {
        let d = i as f32 * step;
        let a = strength * 0.6 / n as f32 * (1.0 - i as f32 / (n as f32 + 1.0)).powi(2);
        fill(pix, &rrect(x - d, y - d + 3.0, w + 2.0 * d, h + 2.0 * d, r + d), &color(0, 0, 0, a), tf);
    }
}


/// Desenha um ícone com centro horizontal `cx` e topo `top`.
/// Usa uma cópia pré-escalada no tamanho inteiro mais próximo e compõe direto na memória:
/// redimensionar com filtro a cada quadro custaria ~30ns/pixel.
#[allow(clippy::too_many_arguments)]
pub fn icon(
    pix: &mut Pixmap,
    cache: &mut ScaleCache,
    icons: &HashMap<String, Icon>,
    key: &str,
    cx: f32,
    top: f32,
    size: f32,
    opacity: f32,
    darken: f32,
) {
    if size < 1.0 || opacity <= 0.003 {
        return;
    }
    let s = size.round() as u32;
    let x = (cx - s as f32 / 2.0).round() as i32;
    let y = top.round() as i32;
    if let Some(p) = cache.get(icons, key, s) {
        blit_fast(pix, p, x, y, opacity);
    }
    if darken > 0.01 {
        if let Some(icon) = icons.get(key) {
            let sc = s as f32 / icon.dark.width() as f32;
            let tf = Transform::from_row(sc, 0.0, 0.0, sc, x as f32, y as f32);
            let paint = PixmapPaint { opacity: (opacity * darken).min(1.0), blend_mode: BlendMode::SourceOver, quality: FilterQuality::Bilinear };
            pix.draw_pixmap(0, 0, icon.dark.as_ref(), &paint, tf, None);
        }
    }
}

/// Composição SourceOver (pré-multiplicada) em posição inteira, com opacidade.
pub fn blit_fast(dst: &mut Pixmap, src: &Pixmap, x: i32, y: i32, opacity: f32) {
    let op = (opacity.clamp(0.0, 1.0) * 256.0) as u32;
    if op == 0 {
        return;
    }
    let (dw, dh) = (dst.width() as i32, dst.height() as i32);
    let (sw, sh) = (src.width() as i32, src.height() as i32);
    let (x0, x1, y0, y1) = (x.max(0), (x + sw).min(dw), y.max(0), (y + sh).min(dh));
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    let sdata = src.data();
    let ddata = dst.data_mut();
    for dy in y0..y1 {
        let srow = ((dy - y) * sw) as usize * 4;
        let drow = (dy * dw) as usize * 4;
        for dx in x0..x1 {
            let si = srow + (dx - x) as usize * 4;
            let s = &sdata[si..si + 4];
            let sa = (s[3] as u32 * op) >> 8;
            if sa == 0 {
                continue;
            }
            let d = &mut ddata[drow + dx as usize * 4..][..4];
            let inv = 255 - sa;
            for c in 0..3 {
                d[c] = (((s[c] as u32 * op) >> 8) + (d[c] as u32 * inv + 127) / 255).min(255) as u8;
            }
            d[3] = (sa + (d[3] as u32 * inv + 127) / 255).min(255) as u8;
        }
    }
}

/// Cópias dos ícones já redimensionadas, por (nome, tamanho em px).
#[derive(Default)]
pub struct ScaleCache {
    map: HashMap<String, HashMap<u32, Pixmap>>,
    count: usize,
}

impl ScaleCache {
    pub fn get<'a>(&'a mut self, icons: &HashMap<String, Icon>, key: &str, size: u32) -> Option<&'a Pixmap> {
        let size = size.clamp(1, 512);
        if !self.map.get(key).is_some_and(|m| m.contains_key(&size)) {
            let icon = icons.get(key)?;
            if self.count > 2500 {
                self.map.clear();
                self.count = 0;
            }
            let src = if size as f32 <= icon.small.width() as f32 * 1.15 { &icon.small } else { &icon.big };
            self.map.entry(key.to_string()).or_default().insert(size, crate::icons::fit(src, size));
            self.count += 1;
        }
        self.map.get(key)?.get(&size)
    }
}

/// Desenha um pixmap (ex.: texto) com transformação e opacidade.
pub fn blit(pix: &mut Pixmap, src: &Pixmap, tf: Transform, opacity: f32) {
    if opacity <= 0.003 {
        return;
    }
    let paint = PixmapPaint { opacity: opacity.min(1.0), blend_mode: BlendMode::SourceOver, quality: FilterQuality::Bilinear };
    pix.draw_pixmap(0, 0, src.as_ref(), &paint, tf, None);
}

/// Copia `src` (mesmo tamanho) para `dst` multiplicando todos os canais por `opacity`
/// (pré-multiplicado: é o mesmo que desenhar com essa opacidade sobre transparente).
pub fn copy_faded(dst: &mut Pixmap, src: &Pixmap, opacity: f32) {
    let a = (opacity.clamp(0.0, 1.0) * 256.0) as u16;
    if a >= 256 {
        dst.data_mut().copy_from_slice(src.data());
        return;
    }
    for (d, s) in dst.data_mut().iter_mut().zip(src.data()) {
        *d = ((*s as u16 * a) >> 8) as u8;
    }
}

/// Zera o retângulo [x0,x1)×[y0,y1) direto na memória (sem pipeline de composição).
pub fn clear_rect(pix: &mut Pixmap, x0: i32, y0: i32, x1: i32, y1: i32) {
    let (w, h) = (pix.width() as i32, pix.height() as i32);
    let (x0, x1, y0, y1) = (x0.clamp(0, w), x1.clamp(0, w), y0.clamp(0, h), y1.clamp(0, h));
    if x1 <= x0 {
        return;
    }
    let data = pix.data_mut();
    for y in y0..y1 {
        let row = (y * w) as usize * 4;
        data[row + x0 as usize * 4..row + x1 as usize * 4].fill(0);
    }
}

/// Preenche o retângulo w×h em (x, y) repetindo `src` (ladrilhado a partir do canto).
/// Com w×h igual ao tamanho de `src` é uma cópia simples. Sobrescreve os pixels:
/// use só onde o destino ainda está transparente (9-slice da prateleira).
pub fn copy(dst: &mut Pixmap, src: &Pixmap, x: i32, y: i32, w: i32, h: i32) {
    let (dw, dh) = (dst.width() as i32, dst.height() as i32);
    let (sw, sh) = (src.width() as i32, src.height() as i32);
    let (x0, x1) = (x.max(0), (x + w).min(dw));
    if x1 <= x0 || sw == 0 || sh == 0 {
        return;
    }
    let sdata = src.data();
    let ddata = dst.data_mut();
    for dy in y.max(0)..(y + h).min(dh) {
        let sy = (dy - y) % sh;
        let srow = &sdata[(sy * sw) as usize * 4..((sy + 1) * sw) as usize * 4];
        let drow = (dy * dw) as usize * 4;
        let mut dx = x0;
        while dx < x1 {
            let sx = (dx - x) % sw;
            let n = (sw - sx).min(x1 - dx);
            ddata[drow + dx as usize * 4..drow + (dx + n) as usize * 4].copy_from_slice(&srow[sx as usize * 4..(sx + n) as usize * 4]);
            dx += n;
        }
    }
}

/// Gira 90° no sentido horário (`cw`) ou anti-horário.
pub fn rotate(src: &Pixmap, cw: bool) -> Pixmap {
    let (w, h) = (src.width(), src.height());
    let mut out = Pixmap::new(h, w).unwrap();
    let (s, d) = (src.data(), out.data_mut());
    for y in 0..h {
        for x in 0..w {
            let (nx, ny) = if cw { (h - 1 - y, x) } else { (y, w - 1 - x) };
            let si = ((y * w + x) * 4) as usize;
            let di = ((ny * h + nx) * 4) as usize;
            d[di..di + 4].copy_from_slice(&s[si..si + 4]);
        }
    }
    out
}

pub struct Text {
    /// Fator de tamanho do texto (preferência "Tamanho do texto"), aplicado a todo texto.
    pub scale: f32,
    font: FontVec,
    cache: HashMap<(String, u32, u32), Pixmap>,
}

impl Text {
    pub fn load() -> Text {
        let mut candidates = Vec::new();
        if let Ok(out) = std::process::Command::new("fc-match").args(["-f", "%{file}", "sans-serif"]).output() {
            candidates.push(String::from_utf8_lossy(&out.stdout).to_string());
        }
        candidates.extend(
            [
                "/usr/share/fonts/truetype/ubuntu/Ubuntu-R.ttf",
                "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
                "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
            ]
            .map(String::from),
        );
        for c in candidates {
            if let Some(font) = std::fs::read(&c).ok().and_then(|b| FontVec::try_from_vec(b).ok()) {
                return Text { scale: 1.0, font, cache: HashMap::new() };
            }
        }
        panic!("rdock: nenhuma fonte encontrada");
    }

    fn layout(&self, s: &str, size: f32) -> (Vec<(GlyphId, f32)>, f32) {
        let sf = self.font.as_scaled(PxScale::from(size));
        let mut x = 0.0;
        let mut prev: Option<GlyphId> = None;
        let mut out = Vec::new();
        for ch in s.chars() {
            let g = self.font.glyph_id(ch);
            if let Some(p) = prev {
                x += sf.kern(p, g);
            }
            out.push((g, x));
            x += sf.h_advance(g);
            prev = Some(g);
        }
        (out, x)
    }

    pub fn width(&self, s: &str, size: f32) -> f32 {
        self.layout(s, size * self.scale).1
    }

    pub fn ellipsize(&self, s: &str, size: f32, max_w: f32) -> String {
        if self.width(s, size) <= max_w {
            return s.to_string();
        }
        let mut chars: Vec<char> = s.chars().collect();
        while !chars.is_empty() {
            chars.pop();
            let t = chars.iter().collect::<String>().trim_end().to_string() + "…";
            if self.width(&t, size) <= max_w {
                return t;
            }
        }
        "…".into()
    }

    /// Pixmap do texto (2px de margem; linha de base em ascent+2). Cacheado.
    pub fn get(&mut self, s: &str, size: f32, rgb: u32) -> &Pixmap {
        let size = size * self.scale;
        let key = (s.to_string(), (size * 10.0) as u32, rgb);
        if !self.cache.contains_key(&key) {
            if self.cache.len() > 4000 {
                self.cache.clear();
            }
            let p = self.render(s, size, rgb);
            self.cache.insert(key.clone(), p);
        }
        &self.cache[&key]
    }

    fn render(&self, s: &str, size: f32, rgb: u32) -> Pixmap {
        let sf = self.font.as_scaled(PxScale::from(size));
        let (asc, desc) = (sf.ascent(), sf.descent());
        let (glyphs, width) = self.layout(s, size);
        let w = width.ceil() as u32 + 4;
        let h = (asc - desc).ceil() as u32 + 4;
        let mut pix = Pixmap::new(w.max(1), h.max(1)).unwrap();
        let (r, g, b) = ((rgb >> 16) & 0xff, (rgb >> 8) & 0xff, rgb & 0xff);
        let data = pix.data_mut();
        for (gid, x) in glyphs {
            let glyph = gid.with_scale_and_position(PxScale::from(size), point(x + 2.0, asc + 2.0));
            let Some(o) = self.font.outline_glyph(glyph) else { continue };
            let bb = o.px_bounds();
            o.draw(|gx, gy, c| {
                let px = bb.min.x as i32 + gx as i32;
                let py = bb.min.y as i32 + gy as i32;
                if px < 0 || py < 0 || px >= w as i32 || py >= h as i32 {
                    return;
                }
                let i = (py as usize * w as usize + px as usize) * 4;
                let a = ((c.min(1.0) * 255.0) as u32).max(data[i + 3] as u32);
                data[i] = (r * a / 255) as u8;
                data[i + 1] = (g * a / 255) as u8;
                data[i + 2] = (b * a / 255) as u8;
                data[i + 3] = a as u8;
            });
        }
        pix
    }
}

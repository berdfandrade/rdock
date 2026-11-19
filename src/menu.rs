//! Menu de contexto (clique direito), compartilhado pela dock e pelo Launchpad.
//! Aceita seções com rótulo, separadores, uma segunda linha (ex.: caminho) e marca de seleção.

use tiny_skia::{Pixmap, Transform};

use crate::anim::Spring;
use crate::draw::{self, Text};
use crate::Action;

const ITEM_H: f32 = 30.0;
const PAD: f32 = 5.0;
const FONT: f32 = 15.5;
const SUB: f32 = 12.0;
const HEADER: f32 = 12.5;
const MAX_W: f32 = 420.0;

pub enum Entry {
    Item { label: String, sub: Option<String>, action: Action, check: bool },
    /// Rótulo de seção (não clicável).
    Header(String),
    Sep,
}

pub struct Menu {
    entries: Vec<Entry>,
    /// Topo de cada entrada, relativo ao menu, e a altura total no fim.
    tops: Vec<f32>,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    /// Ponto de onde o menu "cresce" na animação de abertura.
    origin: (f32, f32),
    pub hovered: Option<usize>,
    pub anim: Spring,
}

impl Menu {
    pub fn new(items: Vec<(String, Action)>, text: &Text) -> Menu {
        Menu::rich(items.into_iter().map(|(label, action)| Entry::Item { label, sub: None, action, check: false }).collect(), text)
    }

    pub fn rich(entries: Vec<Entry>, text: &Text) -> Menu {
        let item_h = (ITEM_H * text.scale).round();
        let mut w: f32 = 170.0;
        let mut tops = Vec::with_capacity(entries.len() + 1);
        let mut y = PAD;
        for e in &entries {
            tops.push(y);
            y += match e {
                Entry::Item { label, sub, .. } => {
                    w = w.max(text.width(label, FONT) + 64.0);
                    if let Some(s) = sub {
                        w = w.max(text.width(s, SUB) + 64.0);
                    }
                    if sub.is_some() { (item_h * 1.4).round() } else { item_h }
                }
                Entry::Header(_) => (item_h * 0.85).round(),
                Entry::Sep => 9.0,
            };
        }
        tops.push(y);
        // Textos longos (caminhos) são encurtados com "…" para caber na largura máxima.
        let w = w.min(MAX_W);
        let mut entries = entries;
        for e in &mut entries {
            if let Entry::Item { label, sub, .. } = e {
                *label = text.ellipsize(label, FONT, w - 64.0);
                if let Some(s) = sub {
                    *s = text.ellipsize(s, SUB, w - 40.0);
                }
            }
        }
        let mut anim = Spring::new(0.0, 700.0, 0.8);
        anim.target = 1.0;
        Menu { entries, tops, x: 0.0, y: 0.0, w, h: y + PAD, origin: (0.0, 0.0), hovered: None, anim }
    }

    pub fn size(&self) -> (f32, f32) {
        (self.w, self.h)
    }

    /// Posiciona o canto superior esquerdo em (x, y), mantendo o menu dentro de `bounds`.
    pub fn place(&mut self, x: f32, y: f32, origin: (f32, f32), bounds: (f32, f32)) {
        self.x = x.clamp(6.0, (bounds.0 - self.w - 6.0).max(6.0)).round();
        self.y = y.clamp(6.0, (bounds.1 - self.h - 6.0).max(6.0)).round();
        self.origin = origin;
    }

    pub fn rect(&self) -> (f32, f32, f32, f32) {
        (self.x, self.y, self.w, self.h)
    }

    /// Índice da entrada clicável sob (x, y).
    pub fn hit(&self, x: f32, y: f32) -> Option<usize> {
        if x < self.x || x > self.x + self.w {
            return None;
        }
        let ry = y - self.y;
        let i = (0..self.entries.len()).find(|&i| ry >= self.tops[i] && ry < self.tops[i + 1])?;
        matches!(self.entries[i], Entry::Item { .. }).then_some(i)
    }

    pub fn action(&self, i: usize) -> Option<Action> {
        match self.entries.get(i)? {
            Entry::Item { action, .. } => Some(*action),
            _ => None,
        }
    }

    pub fn draw(&self, pix: &mut Pixmap, text: &mut Text) {
        let a = self.anim.value.clamp(0.0, 1.0);
        let s = 0.92 + 0.08 * self.anim.value;
        let (ox, oy) = self.origin;
        let tf = Transform::from_translate(ox, oy).pre_scale(s, s).pre_translate(-ox, -oy);
        let (x, y, w, h) = self.rect();
        draw::shadow_n(pix, x, y, w, h, 9.0, a * 1.2, tf, 5);
        draw::fill(pix, &draw::rrect(x, y, w, h, 9.0), &draw::color(38, 38, 42, 0.96 * a), tf);
        draw::stroke(pix, &draw::rrect(x + 0.5, y + 0.5, w - 1.0, h - 1.0, 8.5), &draw::color(255, 255, 255, 0.14 * a), 1.0, tf);
        for (i, e) in self.entries.iter().enumerate() {
            let (top, eh) = (y + self.tops[i], self.tops[i + 1] - self.tops[i]);
            match e {
                Entry::Item { label, sub, check, .. } => {
                    if self.hovered == Some(i) {
                        draw::fill(pix, &draw::rrect(x + PAD, top, w - 2.0 * PAD, eh, 5.0), &draw::color(10, 110, 230, a), tf);
                    }
                    let tp = text.get(label, FONT, 0xffffff);
                    let th = tp.height() as f32;
                    let ty = if sub.is_some() { top + eh * 0.5 - th + 3.0 } else { top + (eh - th) / 2.0 };
                    draw::blit(pix, tp, tf.pre_translate(x + 14.0, ty), a);
                    if let Some(sub) = sub {
                        let sp = text.get(sub, SUB, 0xffffff);
                        draw::blit(pix, sp, tf.pre_translate(x + 14.0, top + eh * 0.5 + 1.0), 0.6 * a);
                    }
                    if *check {
                        let (cx, cy) = (x + w - 22.0, top + eh / 2.0);
                        let mut pb = tiny_skia::PathBuilder::new();
                        pb.move_to(cx - 5.0, cy);
                        pb.line_to(cx - 1.5, cy + 3.5);
                        pb.line_to(cx + 5.0, cy - 4.0);
                        if let Some(p) = pb.finish() {
                            let s = tiny_skia::Stroke { width: 1.8, line_cap: tiny_skia::LineCap::Round, line_join: tiny_skia::LineJoin::Round, ..Default::default() };
                            pix.stroke_path(&p, &draw::color(255, 255, 255, a), &s, tf, None);
                        }
                    }
                }
                Entry::Header(l) => {
                    let tp = text.get(l, HEADER, 0xffffff);
                    draw::blit(pix, tp, tf.pre_translate(x + 14.0, top + eh - tp.height() as f32 - 2.0), 0.5 * a);
                }
                Entry::Sep => {
                    let r = draw::rrect(x + 12.0, top + 4.0, w - 24.0, 1.0, 0.0);
                    draw::fill(pix, &r, &draw::color(255, 255, 255, 0.1 * a), tf);
                }
            }
        }
    }
}

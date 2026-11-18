//! Menu de contexto (clique direito), compartilhado pela dock e pelo Launchpad.

use tiny_skia::{Pixmap, Transform};

use crate::anim::Spring;
use crate::draw::{self, Text};
use crate::Action;

const ITEM_H: f32 = 30.0;
const PAD: f32 = 5.0;
const FONT: f32 = 15.5;

pub struct Menu {
    pub items: Vec<(String, Action)>,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    item_h: f32,
    /// Ponto de onde o menu "cresce" na animação de abertura.
    origin: (f32, f32),
    pub hovered: Option<usize>,
    pub anim: Spring,
}

impl Menu {
    pub fn new(items: Vec<(String, Action)>, text: &Text) -> Menu {
        let w = (items.iter().map(|(l, _)| text.width(l, FONT)).fold(0.0, f32::max) + 40.0).max(170.0);
        let item_h = (ITEM_H * text.scale).round();
        let h = items.len() as f32 * item_h + 2.0 * PAD;
        let mut anim = Spring::new(0.0, 700.0, 0.8);
        anim.target = 1.0;
        Menu { items, x: 0.0, y: 0.0, w, h, item_h, origin: (0.0, 0.0), hovered: None, anim }
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

    pub fn hit(&self, x: f32, y: f32) -> Option<usize> {
        if x < self.x || x > self.x + self.w || y < self.y + PAD || y > self.y + self.h - PAD {
            return None;
        }
        let i = ((y - self.y - PAD) / self.item_h) as usize;
        (i < self.items.len()).then_some(i)
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
        for (i, (label, _)) in self.items.iter().enumerate() {
            let iy = y + PAD + i as f32 * self.item_h;
            if self.hovered == Some(i) {
                draw::fill(pix, &draw::rrect(x + PAD, iy, w - 2.0 * PAD, self.item_h, 5.0), &draw::color(10, 110, 230, a), tf);
            }
            let tp = text.get(label, FONT, 0xffffff);
            let ty = iy + (self.item_h - tp.height() as f32) / 2.0;
            draw::blit(pix, tp, tf.pre_translate(x + 14.0, ty), a);
        }
    }
}

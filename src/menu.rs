//! Menu de contexto (clique direito), compartilhado pela dock e pelo Launchpad.

use tiny_skia::{Pixmap, Transform};

use crate::anim::Spring;
use crate::draw::{self, Text};
use crate::Action;

const ITEM_H: f32 = 26.0;
const PAD: f32 = 5.0;
const FONT: f32 = 13.5;

pub struct Menu {
    pub items: Vec<(String, Action)>,
    cx: f32,
    bottom: f32,
    w: f32,
    pub hovered: Option<usize>,
    pub anim: Spring,
}

impl Menu {
    /// Menu centrado em `cx`, com a borda inferior em `bottom`, limitado à largura `bound_w`.
    pub fn new(items: Vec<(String, Action)>, cx: f32, bottom: f32, text: &Text, bound_w: f32) -> Menu {
        let w = items.iter().map(|(l, _)| text.width(l, FONT)).fold(0.0, f32::max) + 36.0;
        let w = w.max(150.0);
        let cx = cx.clamp(w / 2.0 + 6.0, (bound_w - w / 2.0 - 6.0).max(w / 2.0 + 6.0));
        let mut anim = Spring::new(0.0, 700.0, 0.8);
        anim.target = 1.0;
        Menu { items, cx, bottom, w, hovered: None, anim }
    }

    pub fn rect(&self) -> (f32, f32, f32, f32) {
        let h = self.items.len() as f32 * ITEM_H + 2.0 * PAD;
        (self.cx - self.w / 2.0, self.bottom - h, self.w, h)
    }

    pub fn hit(&self, x: f32, y: f32) -> Option<usize> {
        let (rx, ry, rw, rh) = self.rect();
        if x < rx || x > rx + rw || y < ry + PAD || y > ry + rh - PAD {
            return None;
        }
        let i = ((y - ry - PAD) / ITEM_H) as usize;
        (i < self.items.len()).then_some(i)
    }

    pub fn draw(&self, pix: &mut Pixmap, text: &mut Text) {
        let a = self.anim.value.clamp(0.0, 1.0);
        let s = 0.92 + 0.08 * self.anim.value;
        let tf = Transform::from_translate(self.cx, self.bottom).pre_scale(s, s).pre_translate(-self.cx, -self.bottom);
        let (x, y, w, h) = self.rect();
        draw::shadow_n(pix, x, y, w, h, 9.0, a * 1.2, tf, 5);
        draw::fill(pix, &draw::rrect(x, y, w, h, 9.0), &draw::color(38, 38, 42, 0.96 * a), tf);
        draw::stroke(pix, &draw::rrect(x + 0.5, y + 0.5, w - 1.0, h - 1.0, 8.5), &draw::color(255, 255, 255, 0.14 * a), 1.0, tf);
        for (i, (label, _)) in self.items.iter().enumerate() {
            let iy = y + PAD + i as f32 * ITEM_H;
            if self.hovered == Some(i) {
                draw::fill(pix, &draw::rrect(x + PAD, iy, w - 2.0 * PAD, ITEM_H, 5.0), &draw::color(10, 110, 230, a), tf);
            }
            let tp = text.get(label, FONT, 0xffffff);
            let ty = iy + (ITEM_H - tp.height() as f32) / 2.0;
            draw::blit(pix, tp, tf.pre_translate(x + 14.0, ty), a);
        }
    }
}

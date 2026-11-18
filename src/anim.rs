/// Mola amortecida usada por todas as animações (magnificação, presença, FLIP, fades).
#[derive(Clone, Copy, Debug)]
pub struct Spring {
    pub value: f32,
    pub vel: f32,
    pub target: f32,
    k: f32,
    c: f32,
}

impl Spring {
    /// `stiffness` controla a velocidade; `zeta` = 1 é criticamente amortecida, < 1 tem overshoot.
    pub fn new(value: f32, stiffness: f32, zeta: f32) -> Self {
        Spring { value, vel: 0.0, target: value, k: stiffness, c: 2.0 * zeta * stiffness.sqrt() }
    }

    /// Avança a simulação. Retorna `true` enquanto ainda está em movimento.
    pub fn step(&mut self, dt: f32) -> bool {
        if self.value == self.target && self.vel == 0.0 {
            return false;
        }
        let n = (dt / 0.004).ceil().max(1.0) as u32;
        let h = dt / n as f32;
        for _ in 0..n {
            let a = self.k * (self.target - self.value) - self.c * self.vel;
            self.vel += a * h;
            self.value += self.vel * h;
        }
        if (self.target - self.value).abs() < 0.002 && self.vel.abs() < 0.02 {
            self.value = self.target;
            self.vel = 0.0;
            return false;
        }
        true
    }

    pub fn snap(&mut self, v: f32) {
        self.value = v;
        self.target = v;
        self.vel = 0.0;
    }
}

pub fn ease_out_cubic(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

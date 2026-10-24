//! «Всё вместе» — система и микрофон одной дорожкой. Её играют плееры, Discord и мессенджеры,
//! поэтому голос и игра в ней выравниваются друг к другу: тихий голос поднимается, а если
//! голос намного громче игры (частый случай: микрофон близко, игра и друзья в Discord
//! тихие), голос немного приглушается и игра поднимается. Сумма проходит через
//! ограничитель — без хрипа на перегрузке.
//!
//! Сравниваются средние уровни: голоса — пока он звучит, игры — пока звучит она. Так
//! выравнивание работает и когда говорят по очереди (Discord-комната: друзья в «Звуке
//! системы», вы — в микрофоне). Усиление меняется медленно (секунды) и только пока звучит
//! голос: в паузах оно держится, поэтому шум между фразами не «дышит».

use crate::audio::RATE;
use crate::mixer::soft_clip;

pub struct VoiceMix {
    boost: f32,
    sys_boost: f32,
    voice: f32,
    game: f32,
    /// Средние уровни голоса и игры, пока они звучат (0 — ещё не звучали).
    voice_lvl: f32,
    game_lvl: f32,
    /// Сколько отсчётов они звучали: поначалу уровень — простое среднее, потом скользящее.
    voice_n: f32,
    game_n: f32,
    lim: f32,
}

impl VoiceMix {
    /// Тише этого микрофон — не голос (−40 дБ).
    const VOICE_FLOOR: f32 = 0.01;
    /// Тише этого — игры не слышно, сравнивать не с чем (−50 дБ).
    const GAME_FLOOR: f32 = 0.003;
    /// Громкий голос приглушаем не больше чем на 6 дБ…
    const MIN_BOOST: f32 = 0.5;
    /// …а тихий поднимаем не больше чем на 15,5 дБ (остальное уже сделало автоусиление).
    const MAX_BOOST: f32 = 6.0;
    /// …остальное добирает игра: её поднимаем не больше чем на 9 дБ.
    const MAX_SYS: f32 = 2.8;
    /// Голос чуть громче игры (+1 дБ): речь должна быть разборчива.
    const OVER: f32 = 1.12;
    /// Потолок ограничителя (−1 дБ).
    const CEIL: f32 = 0.89;

    pub fn new() -> Self {
        Self { boost: 1.0, sys_boost: 1.0, voice: 0.0, game: 0.0, voice_lvl: 0.0, game_lvl: 0.0, voice_n: 0.0, game_n: 0.0, lim: 1.0 }
    }

    /// Для готового звука (отрезок, монтаж, копия для отправки): уровни голоса и игры
    /// заранее измерены по `sys` и `mic` целиком, и выравнивание действует с первой секунды.
    pub fn primed(sys: &[f32], mic: &[f32]) -> Self {
        let mut vm = Self::new();
        vm.process(sys, mic, 1.0, 1.0);
        if vm.voice_lvl > 0.0 && vm.game_lvl > 0.0 {
            let c = vm.game_lvl * Self::OVER / vm.voice_lvl;
            vm.boost = c.clamp(Self::MIN_BOOST, Self::MAX_BOOST);
            vm.sys_boost = (vm.boost / c).clamp(1.0, Self::MAX_SYS);
        }
        Self { voice: 0.0, game: 0.0, lim: 1.0, ..vm }
    }

    /// Во сколько раз сейчас усилен голос.
    pub fn boost(&self) -> f32 {
        self.boost
    }

    /// Во сколько раз сейчас усилена игра.
    pub fn sys_boost(&self) -> f32 {
        self.sys_boost
    }

    /// Свести блок. `sys` и `mic` — стерео подряд (чего не хватает — тишина).
    /// `g_sys` и `g_mic` — громкость дорожек, выбранная пользователем: применяется после
    /// выравнивания, чтобы «микрофон 50 %» действительно делал голос тише.
    pub fn process(&mut self, sys: &[f32], mic: &[f32], g_sys: f32, g_mic: f32) -> Vec<f32> {
        let n = sys.len().max(mic.len()) / 2;
        let r = RATE as f32;
        let voice_release = (-1.0 / (0.5 * r)).exp();
        let game_release = (-1.0 / (1.5 * r)).exp();
        let up = 1.0 / (2.0 * r);
        let down = 1.0 / (0.5 * r);
        let lim_release = 1.0 / (0.15 * r);
        let slow = 1.0 / (3.0 * r);
        let track = |lvl: &mut f32, n: &mut f32, env: f32| {
            *n += 1.0;
            *lvl += (env - *lvl) * (1.0 / *n).max(slow);
        };
        let at = |v: &[f32], i: usize| v.get(i).copied().unwrap_or(0.0);
        let mut out = Vec::with_capacity(n * 2);
        for i in 0..n {
            let (sl, sr, ml, mr) = (at(sys, 2 * i), at(sys, 2 * i + 1), at(mic, 2 * i), at(mic, 2 * i + 1));
            let v = ml.abs().max(mr.abs());
            let g = sl.abs().max(sr.abs());
            self.voice = if v > self.voice { v } else { self.voice * voice_release };
            self.game = if g > self.game { g } else { self.game * game_release };
            // «звучит» — на самом звуке, а не пока огибающая гаснет после него
            let voice_on = v > Self::VOICE_FLOOR && v > self.voice * 0.5;
            if voice_on {
                track(&mut self.voice_lvl, &mut self.voice_n, self.voice);
            }
            if g > Self::GAME_FLOOR && g > self.game * 0.5 {
                track(&mut self.game_lvl, &mut self.game_n, self.game);
            }
            if voice_on && self.game_lvl > 0.0 {
                let c = self.game_lvl * Self::OVER / self.voice_lvl;
                let want = c.clamp(Self::MIN_BOOST, Self::MAX_BOOST);
                let want_sys = (want / c).clamp(1.0, Self::MAX_SYS);
                let k = if want > self.boost { up } else { down };
                self.boost += (want - self.boost) * k;
                let k = if want_sys > self.sys_boost { up } else { down };
                self.sys_boost += (want_sys - self.sys_boost) * k;
            }
            let (m, s) = (self.boost * g_mic, self.sys_boost * g_sys);
            let (yl, yr) = (sl * s + ml * m, sr * s + mr * m);
            // ограничитель: мгновенно прижимает пик к потолку и плавно отпускает
            let peak = yl.abs().max(yr.abs());
            self.lim += (1.0 - self.lim) * lim_release;
            if peak * self.lim > Self::CEIL {
                self.lim = Self::CEIL / peak;
            }
            out.push(soft_clip(yl * self.lim));
            out.push(soft_clip(yr * self.lim));
        }
        out
    }
}

impl Default for VoiceMix {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[path = "tests/voicemix.rs"]
mod tests;

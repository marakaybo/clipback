//! Сторож звука: поток сведения (`mixer`) мог упасть или тихо завершиться — тогда звук
//! пропадает из буфера, а захват экрана идёт дальше и никто об этом не знает.
//! Рекордер раз в секунду спрашивает сторожа, пора ли перезапустить звук.

use std::time::{Duration, Instant};

/// После стольких минут без сбоев счётчик перезапусков обнуляется.
const CALM: Duration = Duration::from_secs(300);

pub(crate) struct AudioWatch {
    /// Перезапусков подряд (сбои без долгой спокойной работы между ними).
    restarts: u32,
    /// Звук умер — когда можно перезапустить.
    wait_until: Option<Instant>,
    last_restart: Option<Instant>,
}

impl AudioWatch {
    pub(crate) fn new() -> Self {
        Self { restarts: 0, wait_until: None, last_restart: None }
    }

    /// Пауза перед перезапуском: 1, 2, 4 … 64 с — если звук падает сразу после запуска
    /// (например, кодировщик AAC не создаётся), журнал не засыпается сообщениями.
    pub(crate) fn backoff(restarts: u32) -> Duration {
        Duration::from_secs(1 << restarts.min(6))
    }

    /// Звук не работает. true — перезапустить сейчас (номер попытки — `restarts()`).
    pub(crate) fn dead(&mut self, now: Instant) -> bool {
        if self.last_restart.is_some_and(|t| now.duration_since(t) > CALM) {
            self.restarts = 0;
            self.last_restart = None;
        }
        match self.wait_until {
            None => {
                self.wait_until = Some(now + Self::backoff(self.restarts));
                false
            }
            Some(t) if now >= t => {
                self.wait_until = None;
                self.restarts += 1;
                self.last_restart = Some(now);
                true
            }
            Some(_) => false,
        }
    }

    pub(crate) fn restarts(&self) -> u32 {
        self.restarts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waits_before_restart_and_backs_off() {
        let t = Instant::now();
        let mut w = AudioWatch::new();
        assert!(!w.dead(t));
        assert!(!w.dead(t + Duration::from_millis(500)));
        assert!(w.dead(t + Duration::from_secs(1)));
        assert_eq!(w.restarts(), 1);
        // снова упал сразу — ждём уже 2 с
        let t2 = t + Duration::from_secs(2);
        assert!(!w.dead(t2));
        assert!(!w.dead(t2 + Duration::from_secs(1)));
        assert!(w.dead(t2 + Duration::from_secs(2)));
        assert_eq!(w.restarts(), 2);
    }

    #[test]
    fn backoff_is_capped() {
        assert_eq!(AudioWatch::backoff(0), Duration::from_secs(1));
        assert_eq!(AudioWatch::backoff(3), Duration::from_secs(8));
        assert_eq!(AudioWatch::backoff(50), Duration::from_secs(64));
    }

    #[test]
    fn long_calm_resets_counter() {
        let t = Instant::now();
        let mut w = AudioWatch::new();
        w.dead(t);
        assert!(w.dead(t + Duration::from_secs(1)));
        let later = t + Duration::from_secs(1) + CALM + Duration::from_secs(1);
        assert!(!w.dead(later));
        assert!(w.dead(later + Duration::from_secs(1)), "после спокойной работы снова ждём 1 с");
        assert_eq!(w.restarts(), 1);
    }
}

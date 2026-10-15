//! Кольцевой буфер последних N минут: хранит уже сжатые пакеты в оперативке.
//! Старые пакеты выбрасываются целыми группами (от ключевого кадра до ключевого),
//! поэтому буфер всегда начинается с ключевого кадра и его можно сохранить как есть.

use crate::aac::AudioPacket;
use crate::encoder::Packet;
use std::collections::VecDeque;
use std::sync::Arc;

/// Позиция звука (отсчёты 48 кГц) -> единицы 100 нс, как у видео.
pub fn audio_time(pos: i64) -> i64 {
    pos * 10_000_000 / crate::audio::RATE as i64
}

pub struct Ring {
    packets: VecDeque<Arc<Packet>>,
    /// Кадры AAC по дорожкам.
    audio: Vec<VecDeque<Arc<AudioPacket>>>,
    max_dur: i64,
    bytes: usize,
}

/// Снимок буфера для сохранения.
pub struct Snapshot {
    pub video: Vec<Arc<Packet>>,
    pub audio: Vec<Vec<Arc<AudioPacket>>>,
}

impl Ring {
    pub fn new(seconds: u32) -> Self {
        Self { packets: VecDeque::new(), audio: Vec::new(), max_dur: seconds as i64 * 10_000_000, bytes: 0 }
    }

    pub fn set_audio_tracks(&mut self, n: usize) {
        self.audio = (0..n).map(|_| VecDeque::new()).collect();
    }

    pub fn push_audio(&mut self, track: usize, p: AudioPacket) {
        self.push_audio_arc(track, Arc::new(p))
    }

    pub fn push_audio_arc(&mut self, track: usize, p: Arc<AudioPacket>) {
        if let Some(q) = self.audio.get_mut(track) {
            self.bytes += p.data.len();
            q.push_back(p);
        }
        self.trim_audio();
    }

    /// Звук храним с запасом в секунду до начала видео, остальное выбрасываем.
    fn trim_audio(&mut self) {
        let keep_from = match self.packets.front() {
            Some(f) => f.pts - 10_000_000,
            // видео ещё нет — держим не больше длины буфера
            None => self.audio.iter().filter_map(|q| q.back()).map(|p| audio_time(p.pos)).max().unwrap_or(0) - self.max_dur,
        };
        for q in &mut self.audio {
            while q.front().is_some_and(|p| audio_time(p.pos) < keep_from) {
                let p = q.pop_front().unwrap();
                self.bytes -= p.data.len();
            }
        }
    }

    pub fn set_length(&mut self, seconds: u32) {
        self.max_dur = seconds as i64 * 10_000_000;
        self.trim();
    }

    pub fn push(&mut self, p: Packet) {
        self.push_arc(Arc::new(p))
    }

    pub fn push_arc(&mut self, p: Arc<Packet>) {
        // пока не пришёл первый ключевой кадр, сохранять нечего
        if self.packets.is_empty() && !p.key {
            return;
        }
        self.bytes += p.data.len();
        self.packets.push_back(p);
        self.trim();
    }

    fn trim(&mut self) {
        let Some(last) = self.packets.back().map(|p| p.pts) else { return };
        // выбрасываем первую группу целиком, пока буфер длиннее нужного
        while self.packets.front().is_some_and(|f| last - f.pts > self.max_dur) {
            self.pop_group();
        }
        self.trim_audio();
    }

    fn pop_group(&mut self) {
        if let Some(p) = self.packets.pop_front() {
            self.bytes -= p.data.len();
        }
        while self.packets.front().is_some_and(|p| !p.key) {
            let p = self.packets.pop_front().unwrap();
            self.bytes -= p.data.len();
        }
    }

    /// Длина буфера в секундах.
    pub fn duration(&self) -> f64 {
        match (self.packets.front(), self.packets.back()) {
            (Some(a), Some(b)) => (b.pts - a.pts) as f64 / 1e7,
            _ => 0.0,
        }
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// Копия последних `seconds` секунд (или всего буфера), начиная с ключевого кадра.
    /// Копируются только указатели — данные не дублируются.
    pub fn snapshot(&self, seconds: Option<u32>) -> Vec<Arc<Packet>> {
        self.snapshot_all(seconds).video
    }

    /// Видео и звук за последние `seconds` секунд.
    pub fn snapshot_all(&self, seconds: Option<u32>) -> Snapshot {
        let video = self.video_from(seconds);
        let audio = match (video.first(), video.last()) {
            (Some(a), Some(b)) => {
                // кадр AAC ~21 мс: берём всё, что пересекается с видео
                let (from, to) = (a.pts - 300_000, b.pts + 300_000);
                self.audio
                    .iter()
                    .map(|q| q.iter().filter(|p| (from..=to).contains(&audio_time(p.pos))).cloned().collect())
                    .collect()
            }
            _ => self.audio.iter().map(|_| Vec::new()).collect(),
        };
        Snapshot { video, audio }
    }

    /// Начало для полноценной записи: от последнего ключевого кадра до сейчас.
    pub fn since_last_key(&self) -> Snapshot {
        let Some(k) = self.packets.iter().rposition(|p| p.key) else {
            return Snapshot { video: Vec::new(), audio: self.audio.iter().map(|_| Vec::new()).collect() };
        };
        let from = self.packets[k].pts - 300_000;
        Snapshot {
            video: self.packets.iter().skip(k).cloned().collect(),
            audio: self.audio.iter().map(|q| q.iter().filter(|p| audio_time(p.pos) >= from).cloned().collect()).collect(),
        }
    }

    fn video_from(&self, seconds: Option<u32>) -> Vec<Arc<Packet>> {
        let Some(last) = self.packets.back().map(|p| p.pts) else { return Vec::new() };
        let from = seconds.map(|s| last - s as i64 * 10_000_000).unwrap_or(i64::MIN);
        // последний ключевой кадр не позже точки начала
        let mut start = 0;
        for (i, p) in self.packets.iter().enumerate() {
            if p.key && p.pts <= from.max(self.packets[0].pts) {
                start = i;
            }
            if p.pts > from {
                break;
            }
        }
        self.packets.iter().skip(start).cloned().collect()
    }
}

#[cfg(test)]
#[path = "tests/ring.rs"]
mod tests;

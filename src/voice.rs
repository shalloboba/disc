//! litecord voice chat.
//!
//! NOT Discord's voice channels (those now require the DAVE end-to-end encryption protocol,
//! which a small custom client cannot speak). This is a direct voice room between litecord
//! users: one person hosts (UDP port), the others join with "address:port". The host relays
//! audio to everybody. Audio is 16 kHz mono, G.711 mu-law, 20 ms per packet (~64 kbit/s).
//!
//! Network packet: "LC" | sender id (u32 BE) | payload (320 mu-law bytes, or empty = keep-alive)

use std::{
    collections::{HashMap, VecDeque},
    net::{SocketAddr, ToSocketAddrs, UdpSocket},
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::Sample;

pub const RATE: u32 = 16_000;
const FRAME: usize = 320; // 20 ms at 16 kHz
const MAGIC: [u8; 2] = *b"LC";
const HEADER: usize = 6;
const MAX_QUEUE: usize = RATE as usize * 4 / 10; // 400 ms per speaker
const PREFILL: usize = FRAME * 3; // 60 ms jitter buffer before a speaker starts playing
const PEER_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug)]
pub enum Mode {
    Host(u16),
    Join(String),
}

pub struct Shared {
    pub stop: AtomicBool,
    pub done: AtomicBool,
    pub mute: AtomicBool,
    pub deafen: AtomicBool,
    /// microphone level, 0..=1000
    pub level: AtomicU32,
    /// host: connected peers / join: active speakers
    pub peers: AtomicUsize,
    pub status: Mutex<String>,
}

impl Shared {
    fn set_status(&self, s: impl Into<String>) {
        *self.status.lock().unwrap() = s.into();
    }
}

pub struct Voice {
    pub shared: Arc<Shared>,
    handle: Option<JoinHandle<()>>,
}

impl Voice {
    pub fn start(mode: Mode) -> Voice {
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            done: AtomicBool::new(false),
            mute: AtomicBool::new(false),
            deafen: AtomicBool::new(false),
            level: AtomicU32::new(0),
            peers: AtomicUsize::new(0),
            status: Mutex::new("開始中…".into()),
        });
        let sh = shared.clone();
        let handle = thread::spawn(move || {
            if let Err(e) = run(mode, &sh) {
                sh.set_status(format!("エラー: {e}"));
            }
            sh.done.store(true, Ordering::Relaxed);
        });
        Voice { shared, handle: Some(handle) }
    }

    pub fn is_done(&self) -> bool {
        self.shared.done.load(Ordering::Relaxed)
    }

    pub fn status(&self) -> String {
        self.shared.status.lock().unwrap().clone()
    }

    pub fn stop(mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

// ───────────────────────── G.711 mu-law ─────────────────────────

pub fn mulaw_enc(s: i16) -> u8 {
    const BIAS: i32 = 0x84;
    const CLIP: i32 = 32635;
    let mut x = s as i32;
    let sign: u8 = if x < 0 {
        x = -x;
        0x80
    } else {
        0
    };
    if x > CLIP {
        x = CLIP;
    }
    x += BIAS;
    let mut exp: i32 = 7;
    let mut mask: i32 = 0x4000;
    while exp > 0 && (x & mask) == 0 {
        exp -= 1;
        mask >>= 1;
    }
    let mant = ((x >> (exp + 3)) & 0x0F) as u8;
    !(sign | ((exp as u8) << 4) | mant)
}

pub fn mulaw_dec(u: u8) -> i16 {
    let u = !u;
    let sign = u & 0x80;
    let exp = ((u >> 4) & 7) as i32;
    let mant = (u & 0x0F) as i32;
    let mut x = (((mant << 3) + 0x84) << exp) - 0x84;
    if sign != 0 {
        x = -x;
    }
    x as i16
}

// ───────────────────────── mixer (playback side) ─────────────────────────

struct Speaker {
    q: VecDeque<i16>,
    playing: bool,
    last: Instant,
}

#[derive(Default)]
struct Mixer {
    speakers: HashMap<u32, Speaker>,
}

impl Mixer {
    fn push(&mut self, id: u32, samples: impl Iterator<Item = i16>) {
        let sp = self.speakers.entry(id).or_insert_with(|| Speaker {
            q: VecDeque::new(),
            playing: false,
            last: Instant::now(),
        });
        sp.last = Instant::now();
        sp.q.extend(samples);
        while sp.q.len() > MAX_QUEUE {
            sp.q.pop_front();
        }
    }

    /// Next mixed sample at 16 kHz in -1..1.
    fn pop(&mut self) -> f32 {
        let mut sum = 0f32;
        for sp in self.speakers.values_mut() {
            if !sp.playing && sp.q.len() >= PREFILL {
                sp.playing = true;
            }
            if sp.playing {
                match sp.q.pop_front() {
                    Some(s) => sum += s as f32,
                    None => sp.playing = false,
                }
            }
        }
        (sum / 32768.0).clamp(-1.0, 1.0)
    }

    /// Forget speakers that went silent; returns how many are still known.
    fn prune(&mut self) -> usize {
        self.speakers.retain(|_, s| s.last.elapsed() < PEER_TIMEOUT);
        self.speakers.len()
    }
}

// ───────────────────────── audio streams ─────────────────────────

fn build_input<T>(
    dev: &cpal::Device,
    cfg: &cpal::StreamConfig,
    cap: Arc<Mutex<VecDeque<i16>>>,
    sh: Arc<Shared>,
) -> Result<cpal::Stream, String>
where
    T: cpal::SizedSample,
    f32: cpal::FromSample<T>,
{
    let ch = (cfg.channels as usize).max(1);
    let ratio = cfg.sample_rate.0 as f32 / RATE as f32;
    let (mut pos, mut acc, mut n) = (0f32, 0f32, 0f32);
    dev.build_input_stream(
        cfg,
        move |data: &[T], _| {
            let mut out: Vec<i16> = Vec::with_capacity(data.len() / ch / ratio.max(1.0) as usize + 2);
            let mut peak = 0f32;
            for frame in data.chunks(ch) {
                let mut s = 0f32;
                for x in frame {
                    s += (*x).to_sample::<f32>();
                }
                s /= ch as f32;
                peak = peak.max(s.abs());
                // box filter while decimating to 16 kHz
                acc += s;
                n += 1.0;
                pos += 1.0;
                if pos >= ratio {
                    pos -= ratio;
                    out.push(((acc / n).clamp(-1.0, 1.0) * 32767.0) as i16);
                    acc = 0.0;
                    n = 0.0;
                }
            }
            sh.level.store((peak.min(1.0) * 1000.0) as u32, Ordering::Relaxed);
            let mut q = cap.lock().unwrap();
            q.extend(out);
            while q.len() > RATE as usize {
                q.pop_front();
            }
        },
        |e| eprintln!("input stream error: {e}"),
        None,
    )
    .map_err(|e| e.to_string())
}

fn build_output<T>(
    dev: &cpal::Device,
    cfg: &cpal::StreamConfig,
    mix: Arc<Mutex<Mixer>>,
    sh: Arc<Shared>,
) -> Result<cpal::Stream, String>
where
    T: cpal::SizedSample + cpal::FromSample<f32>,
{
    let ch = (cfg.channels as usize).max(1);
    let ratio = RATE as f32 / cfg.sample_rate.0 as f32;
    let (mut prev, mut next, mut phase) = (0f32, 0f32, 0f32);
    dev.build_output_stream(
        cfg,
        move |data: &mut [T], _| {
            let deaf = sh.deafen.load(Ordering::Relaxed);
            let mut m = mix.lock().unwrap();
            for frame in data.chunks_mut(ch) {
                phase += ratio;
                while phase >= 1.0 {
                    prev = next;
                    next = m.pop();
                    phase -= 1.0;
                }
                let v = if deaf { 0.0 } else { prev + (next - prev) * phase };
                let s = T::from_sample(v);
                for o in frame.iter_mut() {
                    *o = s;
                }
            }
        },
        |e| eprintln!("output stream error: {e}"),
        None,
    )
    .map_err(|e| e.to_string())
}

// ───────────────────────── session ─────────────────────────

fn is_would_block(e: &std::io::Error) -> bool {
    matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut)
}

fn packet(id: u32, payload: &[u8]) -> Vec<u8> {
    let mut p = Vec::with_capacity(HEADER + payload.len());
    p.extend_from_slice(&MAGIC);
    p.extend_from_slice(&id.to_be_bytes());
    p.extend_from_slice(payload);
    p
}

fn run(mode: Mode, sh: &Arc<Shared>) -> Result<(), String> {
    let is_host = matches!(mode, Mode::Host(_));
    let sock = match &mode {
        Mode::Host(p) => UdpSocket::bind(("0.0.0.0", *p)),
        Mode::Join(_) => UdpSocket::bind("0.0.0.0:0"),
    }
    .map_err(|e| format!("UDPポートを開けません: {e}"))?;
    sock.set_read_timeout(Some(Duration::from_millis(5))).map_err(|e| e.to_string())?;

    let server: Option<SocketAddr> = match &mode {
        Mode::Join(a) => Some(
            a.trim()
                .to_socket_addrs()
                .map_err(|e| format!("アドレスを解決できません ({a}): {e}"))?
                .next()
                .ok_or("アドレスを解決できません")?,
        ),
        Mode::Host(_) => None,
    };

    let host = cpal::default_host();
    let in_dev = host.default_input_device().ok_or("マイクが見つかりません")?;
    let out_dev = host.default_output_device().ok_or("スピーカーが見つかりません")?;
    let in_cfg = in_dev.default_input_config().map_err(|e| format!("マイク設定: {e}"))?;
    let out_cfg = out_dev.default_output_config().map_err(|e| format!("スピーカー設定: {e}"))?;

    let cap: Arc<Mutex<VecDeque<i16>>> = Arc::new(Mutex::new(VecDeque::new()));
    let mix: Arc<Mutex<Mixer>> = Arc::new(Mutex::new(Mixer::default()));

    let in_stream = match in_cfg.sample_format() {
        cpal::SampleFormat::F32 => build_input::<f32>(&in_dev, &in_cfg.config(), cap.clone(), sh.clone()),
        cpal::SampleFormat::I16 => build_input::<i16>(&in_dev, &in_cfg.config(), cap.clone(), sh.clone()),
        cpal::SampleFormat::U16 => build_input::<u16>(&in_dev, &in_cfg.config(), cap.clone(), sh.clone()),
        other => Err(format!("未対応のマイク形式: {other:?}")),
    }?;
    let out_stream = match out_cfg.sample_format() {
        cpal::SampleFormat::F32 => build_output::<f32>(&out_dev, &out_cfg.config(), mix.clone(), sh.clone()),
        cpal::SampleFormat::I16 => build_output::<i16>(&out_dev, &out_cfg.config(), mix.clone(), sh.clone()),
        cpal::SampleFormat::U16 => build_output::<u16>(&out_dev, &out_cfg.config(), mix.clone(), sh.clone()),
        other => Err(format!("未対応のスピーカー形式: {other:?}")),
    }?;
    in_stream.play().map_err(|e| format!("マイク開始: {e}"))?;
    out_stream.play().map_err(|e| format!("スピーカー開始: {e}"))?;

    let mut idb = [0u8; 4];
    getrandom::getrandom(&mut idb).map_err(|e| e.to_string())?;
    let my_id = u32::from_be_bytes(idb);

    let mut peers: HashMap<SocketAddr, Instant> = HashMap::new(); // host only
    let mut connected = false; // join only
    let mut last_ka = Instant::now() - Duration::from_secs(2);
    let mut last_prune = Instant::now();
    let mut buf = [0u8; 2048];

    match &mode {
        Mode::Host(p) => sh.set_status(format!("ホスト中 (UDP {p}) / 待機中")),
        Mode::Join(a) => sh.set_status(format!("{a} に接続中…")),
    }

    while !sh.stop.load(Ordering::Relaxed) {
        // 1) send captured microphone audio, one 20 ms frame at a time
        loop {
            let chunk: Option<Vec<i16>> = {
                let mut q = cap.lock().unwrap();
                if q.len() >= FRAME {
                    Some(q.drain(..FRAME).collect())
                } else {
                    None
                }
            };
            let Some(chunk) = chunk else { break };
            if sh.mute.load(Ordering::Relaxed) {
                continue;
            }
            let payload: Vec<u8> = chunk.iter().map(|&s| mulaw_enc(s)).collect();
            let pkt = packet(my_id, &payload);
            match server {
                Some(addr) => {
                    let _ = sock.send_to(&pkt, addr);
                }
                None => {
                    for addr in peers.keys() {
                        let _ = sock.send_to(&pkt, addr);
                    }
                }
            }
        }

        // 2) keep-alive once a second (registers us with the host / tells peers the host is alive)
        if last_ka.elapsed() >= Duration::from_secs(1) {
            last_ka = Instant::now();
            let ka = packet(my_id, &[]);
            match server {
                Some(addr) => {
                    let _ = sock.send_to(&ka, addr);
                }
                None => {
                    for addr in peers.keys() {
                        let _ = sock.send_to(&ka, addr);
                    }
                }
            }
        }

        // 3) receive
        match sock.recv_from(&mut buf) {
            Ok((n, from)) => {
                if n >= HEADER && buf[..2] == MAGIC {
                    let id = u32::from_be_bytes([buf[2], buf[3], buf[4], buf[5]]);
                    let accept = match server {
                        Some(addr) => from == addr,
                        None => true,
                    };
                    if accept && id != my_id {
                        let payload = &buf[HEADER..n];
                        if is_host {
                            peers.insert(from, Instant::now());
                            if !payload.is_empty() {
                                for addr in peers.keys().filter(|a| **a != from) {
                                    let _ = sock.send_to(&buf[..n], addr);
                                }
                            }
                        } else if !connected {
                            connected = true;
                        }
                        if !payload.is_empty() && payload.len() <= FRAME * 2 {
                            mix.lock().unwrap().push(id, payload.iter().map(|&b| mulaw_dec(b)));
                        }
                    }
                }
            }
            // Windows reports ICMP "port unreachable" as a read error; just keep going
            Err(e) if is_would_block(&e) => {}
            Err(_) => {}
        }

        // 4) housekeeping
        if last_prune.elapsed() >= Duration::from_secs(1) {
            last_prune = Instant::now();
            let speakers = mix.lock().unwrap().prune();
            if is_host {
                peers.retain(|_, t| t.elapsed() < PEER_TIMEOUT);
                sh.peers.store(peers.len(), Ordering::Relaxed);
                if let Mode::Host(p) = &mode {
                    sh.set_status(format!("ホスト中 (UDP {p}) / {}人接続", peers.len()));
                }
            } else {
                sh.peers.store(speakers, Ordering::Relaxed);
                if let Mode::Join(a) = &mode {
                    if connected {
                        sh.set_status(format!("接続済み ({a})"));
                    } else {
                        sh.set_status(format!("{a} からの応答待ち… (ポート開放/アドレスを確認)"));
                    }
                }
            }
        }
    }

    drop(in_stream);
    drop(out_stream);
    sh.level.store(0, Ordering::Relaxed);
    sh.set_status("停止しました");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mulaw_roundtrip_is_close() {
        for &s in &[0i16, 1, -1, 100, -100, 1000, -1000, 8000, -8000, 20000, -20000, 32000, -32000] {
            let d = mulaw_dec(mulaw_enc(s)) as i32;
            let tol = (s as i32).abs() / 16 + 16; // mu-law keeps ~6 bits of relative precision
            assert!((d - s as i32).abs() <= tol, "{s} -> {d}");
        }
    }

    #[test]
    fn packet_layout() {
        let p = packet(0x01020304, &[9, 8]);
        assert_eq!(&p[..2], b"LC");
        assert_eq!(&p[2..6], &[1, 2, 3, 4]);
        assert_eq!(&p[6..], &[9, 8]);
    }

    #[test]
    fn mixer_waits_for_prefill() {
        let mut m = Mixer::default();
        m.push(1, std::iter::repeat(16384i16).take(FRAME));
        assert_eq!(m.pop(), 0.0); // below the prefill threshold: silent
        m.push(1, std::iter::repeat(16384i16).take(FRAME * 2));
        assert!(m.pop() > 0.4);
    }
}

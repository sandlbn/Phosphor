//! Effect catalog: parameter definitions + the processing for each block.
//!
//! Every block processes stereo in place (`l`, `r` slices, f32, ±1.0 full
//! scale). Knob values come from `Shared` atomics read once per buffer and
//! smoothed per sample. fundsp supplies the filters (modulatable SVFs),
//! reverb, chorus and flanger; blocks whose fundsp graph bakes its
//! parameters in at build time (reverb / chorus / flanger) rebuild the
//! graph and crossfade when a knob moves.

use fundsp::prelude as fp;
use fundsp::prelude::{AudioUnit, Shared};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BlockKind {
    Gain,
    Eq3,
    Filter,
    Compressor,
    Limiter,
    Delay,
    Reverb,
    Chorus,
    Flanger,
    Bitcrusher,
    Overdrive,
    TapeWobble,
    Widener,
    Haas,
    AutoPan,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Core,
    Time,
    Character,
    Stereo,
}

impl Category {
    pub fn label(self) -> &'static str {
        match self {
            Self::Core => "Core",
            Self::Time => "Time",
            Self::Character => "Character",
            Self::Stereo => "Stereo",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ParamDef {
    pub name: &'static str,
    pub min: f32,
    pub max: f32,
    pub default: f32,
    pub unit: &'static str,
    /// Knob travel is logarithmic (frequencies, times).
    pub log: bool,
    /// Discrete parameter with this many steps (e.g. filter mode).
    pub steps: Option<u8>,
}

const fn p(name: &'static str, min: f32, max: f32, default: f32, unit: &'static str) -> ParamDef {
    ParamDef {
        name,
        min,
        max,
        default,
        unit,
        log: false,
        steps: None,
    }
}
const fn plog(
    name: &'static str,
    min: f32,
    max: f32,
    default: f32,
    unit: &'static str,
) -> ParamDef {
    ParamDef {
        name,
        min,
        max,
        default,
        unit,
        log: true,
        steps: None,
    }
}
const fn pstep(name: &'static str, steps: u8, default: f32) -> ParamDef {
    ParamDef {
        name,
        min: 0.0,
        max: (steps - 1) as f32,
        default,
        unit: "",
        log: false,
        steps: Some(steps),
    }
}

impl BlockKind {
    pub const ALL: [BlockKind; 15] = [
        Self::Gain,
        Self::Eq3,
        Self::Filter,
        Self::Compressor,
        Self::Limiter,
        Self::Delay,
        Self::Reverb,
        Self::Chorus,
        Self::Flanger,
        Self::TapeWobble,
        Self::Bitcrusher,
        Self::Overdrive,
        Self::Widener,
        Self::Haas,
        Self::AutoPan,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Gain => "Gain",
            Self::Eq3 => "EQ",
            Self::Filter => "Filter",
            Self::Compressor => "Compressor",
            Self::Limiter => "Limiter",
            Self::Delay => "Delay",
            Self::Reverb => "Reverb",
            Self::Chorus => "Chorus",
            Self::Flanger => "Flanger",
            Self::Bitcrusher => "Bitcrusher",
            Self::Overdrive => "Overdrive",
            Self::TapeWobble => "Tape Wobble",
            Self::Widener => "Widener",
            Self::Haas => "Haas",
            Self::AutoPan => "Auto-Pan",
        }
    }

    pub fn category(self) -> Category {
        use BlockKind::*;
        match self {
            Gain | Eq3 | Filter | Compressor | Limiter => Category::Core,
            Delay | Reverb | Chorus | Flanger | TapeWobble => Category::Time,
            Bitcrusher | Overdrive => Category::Character,
            Widener | Haas | AutoPan => Category::Stereo,
        }
    }

    /// Human-readable label for a discrete param value.
    pub fn step_label(self, param: usize, v: f32) -> Option<&'static str> {
        match (self, param) {
            (Self::Filter, 0) => {
                Some(["Low-pass", "High-pass", "Band-pass"][v.round() as usize % 3])
            }
            (Self::Delay, 3) | (Self::Haas, 1) => Some(["Off", "On"][v.round() as usize % 2]),
            _ => None,
        }
    }

    pub fn params(self) -> &'static [ParamDef] {
        use BlockKind::*;
        match self {
            Gain => {
                const {
                    &[
                        p("Gain", -24.0, 24.0, 0.0, "dB"),
                        p("Pan", -1.0, 1.0, 0.0, ""),
                    ]
                }
            }
            Eq3 => {
                const {
                    &[
                        p("Low", -15.0, 15.0, 0.0, "dB"),
                        p("Mid", -15.0, 15.0, 0.0, "dB"),
                        plog("Mid Freq", 200.0, 5000.0, 1000.0, "Hz"),
                        p("High", -15.0, 15.0, 0.0, "dB"),
                    ]
                }
            }
            Filter => {
                const {
                    &[
                        pstep("Mode", 3, 0.0),
                        plog("Cutoff", 40.0, 18000.0, 4000.0, "Hz"),
                        plog("Resonance", 0.5, 10.0, 0.9, "Q"),
                    ]
                }
            }
            Compressor => {
                const {
                    &[
                        p("Threshold", -40.0, 0.0, -18.0, "dB"),
                        plog("Ratio", 1.0, 20.0, 4.0, ":1"),
                        plog("Attack", 0.1, 100.0, 10.0, "ms"),
                        plog("Release", 10.0, 1000.0, 150.0, "ms"),
                        p("Makeup", 0.0, 24.0, 0.0, "dB"),
                    ]
                }
            }
            Limiter => {
                const {
                    &[
                        p("Ceiling", -12.0, 0.0, -1.0, "dB"),
                        plog("Release", 10.0, 500.0, 80.0, "ms"),
                    ]
                }
            }
            Delay => {
                const {
                    &[
                        plog("Time", 10.0, 2000.0, 375.0, "ms"),
                        p("Feedback", 0.0, 0.95, 0.4, ""),
                        p("Mix", 0.0, 1.0, 0.3, ""),
                        pstep("Ping-pong", 2, 1.0),
                        plog("Tone", 500.0, 18000.0, 6000.0, "Hz"),
                    ]
                }
            }
            Reverb => {
                const {
                    &[
                        p("Size", 1.0, 30.0, 10.0, "m"),
                        plog("Decay", 0.2, 10.0, 2.5, "s"),
                        p("Damping", 0.0, 1.0, 0.5, ""),
                        p("Mix", 0.0, 1.0, 0.25, ""),
                    ]
                }
            }
            Chorus => {
                const {
                    &[
                        plog("Rate", 0.05, 3.0, 0.4, "Hz"),
                        p("Depth", 1.0, 10.0, 4.0, "ms"),
                        p("Mix", 0.0, 1.0, 0.5, ""),
                    ]
                }
            }
            Flanger => {
                const {
                    &[
                        plog("Rate", 0.05, 5.0, 0.25, "Hz"),
                        p("Depth", 0.5, 10.0, 3.0, "ms"),
                        p("Feedback", -0.95, 0.95, 0.5, ""),
                        p("Mix", 0.0, 1.0, 0.5, ""),
                    ]
                }
            }
            Bitcrusher => {
                const {
                    &[
                        p("Bits", 2.0, 16.0, 8.0, "bit"),
                        p("Downsample", 1.0, 32.0, 4.0, "×"),
                        p("Mix", 0.0, 1.0, 1.0, ""),
                    ]
                }
            }
            Overdrive => {
                const {
                    &[
                        p("Drive", 0.0, 40.0, 12.0, "dB"),
                        plog("Tone", 500.0, 12000.0, 6000.0, "Hz"),
                        p("Level", -24.0, 6.0, -6.0, "dB"),
                        p("Mix", 0.0, 1.0, 1.0, ""),
                    ]
                }
            }
            TapeWobble => {
                const {
                    &[
                        plog("Rate", 0.1, 8.0, 0.8, "Hz"),
                        p("Depth", 0.0, 5.0, 1.2, "ms"),
                        p("Flutter", 0.0, 1.0, 0.3, ""),
                        p("Mix", 0.0, 1.0, 1.0, ""),
                    ]
                }
            }
            Widener => const { &[p("Width", 0.0, 2.0, 1.5, "")] },
            Haas => {
                const {
                    &[
                        p("Delay", 0.0, 40.0, 15.0, "ms"),
                        pstep("Delay Left", 2, 0.0),
                        p("Mix", 0.0, 1.0, 1.0, ""),
                    ]
                }
            }
            AutoPan => {
                const {
                    &[
                        plog("Rate", 0.05, 10.0, 1.0, "Hz"),
                        p("Depth", 0.0, 1.0, 0.8, ""),
                    ]
                }
            }
        }
    }
}

// ── Runtime ───────────────────────────────────────────────────────────────

pub trait Effect: Send {
    fn process(&mut self, l: &mut [f32], r: &mut [f32]);
    fn reset(&mut self);
}

/// Build the processor for `kind`, reading live values from `params`.
pub fn build(kind: BlockKind, params: &[Shared], sr: f64) -> Box<dyn Effect> {
    let params = params.to_vec();
    let sr = sr as f32;
    match kind {
        BlockKind::Gain => Box::new(Gain::new(params, sr)),
        BlockKind::Eq3 => Box::new(Eq3::new(params, sr)),
        BlockKind::Filter => Box::new(Filter::new(params, sr)),
        BlockKind::Compressor => Box::new(Compressor::new(params, sr)),
        BlockKind::Limiter => Box::new(Limiter::new(params, sr)),
        BlockKind::Delay => Box::new(PingPong::new(params, sr)),
        BlockKind::Reverb => Box::new(Rebuilt::new(params, sr, 3, make_reverb, false)),
        BlockKind::Chorus => Box::new(Rebuilt::new(params, sr, 2, make_chorus, true)),
        BlockKind::Flanger => Box::new(Rebuilt::new(params, sr, 3, make_flanger, true)),
        BlockKind::Bitcrusher => Box::new(Bitcrusher::new(params)),
        BlockKind::Overdrive => Box::new(Overdrive::new(params, sr)),
        BlockKind::TapeWobble => Box::new(TapeWobble::new(params, sr)),
        BlockKind::Widener => Box::new(Widener::new(params, sr)),
        BlockKind::Haas => Box::new(Haas::new(params, sr)),
        BlockKind::AutoPan => Box::new(AutoPan::new(params, sr)),
    }
}

fn db(v: f32) -> f32 {
    10f32.powf(v / 20.0)
}

/// One-pole parameter smoother (~10 ms).
#[derive(Clone, Copy)]
struct Smooth {
    cur: f32,
    coef: f32,
}

impl Smooth {
    fn new(v: f32, sr: f32) -> Self {
        Self {
            cur: v,
            coef: 1.0 - (-1.0 / (0.01 * sr)).exp(),
        }
    }
    #[inline]
    fn next(&mut self, target: f32) -> f32 {
        self.cur += (target - self.cur) * self.coef;
        self.cur
    }
}

/// Fractional delay line.
struct DelayLine {
    buf: Vec<f32>,
    pos: usize,
}

impl DelayLine {
    fn new(max_samples: usize) -> Self {
        Self {
            buf: vec![0.0; max_samples.max(4)],
            pos: 0,
        }
    }
    #[inline]
    fn write(&mut self, x: f32) {
        self.buf[self.pos] = x;
        self.pos = (self.pos + 1) % self.buf.len();
    }
    /// Read `d` samples behind the last write (linear interpolation).
    #[inline]
    fn read(&self, d: f32) -> f32 {
        let n = self.buf.len();
        let d = d.clamp(1.0, (n - 2) as f32);
        let i = d.floor();
        let frac = d - i;
        let idx = (self.pos + n - i as usize) % n;
        let a = self.buf[idx];
        let b = self.buf[(idx + n - 1) % n];
        a + (b - a) * frac
    }
    fn clear(&mut self) {
        self.buf.iter_mut().for_each(|x| *x = 0.0);
    }
}

fn unit(mut u: Box<dyn AudioUnit>, sr: f32) -> Box<dyn AudioUnit> {
    u.set_sample_rate(sr as f64);
    u.allocate();
    u
}

// ── Gain / pan ───────────────────────────────────────────────────────────

struct Gain {
    p: Vec<Shared>,
    g: Smooth,
    pan: Smooth,
}

impl Gain {
    fn new(p: Vec<Shared>, sr: f32) -> Self {
        let g = Smooth::new(db(p[0].value()), sr);
        let pan = Smooth::new(p[1].value(), sr);
        Self { p, g, pan }
    }
}

impl Effect for Gain {
    fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        let (tg, tp) = (db(self.p[0].value()), self.p[1].value());
        for (a, b) in l.iter_mut().zip(r.iter_mut()) {
            let g = self.g.next(tg);
            let pan = self.pan.next(tp);
            *a *= g * (1.0 - pan).min(1.0);
            *b *= g * (1.0 + pan).min(1.0);
        }
    }
    fn reset(&mut self) {}
}

// ── EQ (fundsp SVF shelves + bell, modulatable) ───────────────────────────

struct Eq3 {
    p: Vec<Shared>,
    /// [low shelf, bell, high shelf] per channel.
    bands: [[Box<dyn AudioUnit>; 3]; 2],
    s: [Smooth; 4],
}

impl Eq3 {
    fn new(p: Vec<Shared>, sr: f32) -> Self {
        let mk = || -> [Box<dyn AudioUnit>; 3] {
            [
                unit(Box::new(fp::lowshelf::<f32>()), sr),
                unit(Box::new(fp::bell::<f32>()), sr),
                unit(Box::new(fp::highshelf::<f32>()), sr),
            ]
        };
        let s = [
            Smooth::new(p[0].value(), sr),
            Smooth::new(p[1].value(), sr),
            Smooth::new(p[2].value(), sr),
            Smooth::new(p[3].value(), sr),
        ];
        Self {
            p,
            bands: [mk(), mk()],
            s,
        }
    }
}

impl Effect for Eq3 {
    fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        let t: [f32; 4] = std::array::from_fn(|i| self.p[i].value());
        let mut out = [0.0f32];
        for (a, b) in l.iter_mut().zip(r.iter_mut()) {
            let lo = db(self.s[0].next(t[0]));
            let mid = db(self.s[1].next(t[1]));
            let mf = self.s[2].next(t[2]);
            let hi = db(self.s[3].next(t[3]));
            for (ch, x) in [a, b].into_iter().enumerate() {
                let bands = &mut self.bands[ch];
                bands[0].tick(&[*x, 200.0, 0.707, lo], &mut out);
                bands[1].tick(&[out[0], mf, 0.9, mid], &mut out);
                bands[2].tick(&[out[0], 5000.0, 0.707, hi], &mut out);
                *x = out[0];
            }
        }
    }
    fn reset(&mut self) {
        self.bands.iter_mut().flatten().for_each(|u| u.reset());
    }
}

// ── Filter (LP / HP / BP) ────────────────────────────────────────────────

struct Filter {
    p: Vec<Shared>,
    lp: [Box<dyn AudioUnit>; 2],
    hp: [Box<dyn AudioUnit>; 2],
    bp: [Box<dyn AudioUnit>; 2],
    cut: Smooth,
    q: Smooth,
}

impl Filter {
    fn new(p: Vec<Shared>, sr: f32) -> Self {
        let cut = Smooth::new(p[1].value(), sr);
        let q = Smooth::new(p[2].value(), sr);
        let lp = || unit(Box::new(fp::lowpass::<f32>()), sr);
        let hp = || unit(Box::new(fp::highpass::<f32>()), sr);
        let bp = || unit(Box::new(fp::bandpass::<f32>()), sr);
        Self {
            p,
            lp: [lp(), lp()],
            hp: [hp(), hp()],
            bp: [bp(), bp()],
            cut,
            q,
        }
    }
}

impl Effect for Filter {
    fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        let mode = self.p[0].value().round() as usize;
        let (tc, tq) = (self.p[1].value(), self.p[2].value());
        let units = match mode {
            1 => &mut self.hp,
            2 => &mut self.bp,
            _ => &mut self.lp,
        };
        let mut out = [0.0f32];
        for (a, b) in l.iter_mut().zip(r.iter_mut()) {
            let c = self.cut.next(tc);
            let q = self.q.next(tq);
            units[0].tick(&[*a, c, q], &mut out);
            *a = out[0];
            units[1].tick(&[*b, c, q], &mut out);
            *b = out[0];
        }
    }
    fn reset(&mut self) {
        for u in self.lp.iter_mut().chain(&mut self.hp).chain(&mut self.bp) {
            u.reset();
        }
    }
}

// ── Compressor (stereo-linked, feed-forward) ─────────────────────────────

struct Compressor {
    p: Vec<Shared>,
    sr: f32,
    env: f32,
    makeup: Smooth,
}

impl Compressor {
    fn new(p: Vec<Shared>, sr: f32) -> Self {
        let makeup = Smooth::new(db(p[4].value()), sr);
        Self {
            p,
            sr,
            env: 0.0,
            makeup,
        }
    }
}

fn time_coef(ms: f32, sr: f32) -> f32 {
    (-1.0 / (ms.max(0.01) * 0.001 * sr)).exp()
}

impl Effect for Compressor {
    fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        let thr = self.p[0].value();
        let ratio = self.p[1].value().max(1.0);
        let att = time_coef(self.p[2].value(), self.sr);
        let rel = time_coef(self.p[3].value(), self.sr);
        let mk = db(self.p[4].value());
        for (a, b) in l.iter_mut().zip(r.iter_mut()) {
            let peak = a.abs().max(b.abs());
            let c = if peak > self.env { att } else { rel };
            self.env = peak + (self.env - peak) * c;
            let lvl = 20.0 * self.env.max(1e-6).log10();
            let over = (lvl - thr).max(0.0);
            let gain = db(-over * (1.0 - 1.0 / ratio)) * self.makeup.next(mk);
            *a *= gain;
            *b *= gain;
        }
    }
    fn reset(&mut self) {
        self.env = 0.0;
    }
}

// ── Limiter (instant attack, smooth release) ─────────────────────────────

struct Limiter {
    p: Vec<Shared>,
    sr: f32,
    gain: f32,
}

impl Limiter {
    fn new(p: Vec<Shared>, sr: f32) -> Self {
        Self { p, sr, gain: 1.0 }
    }
}

impl Effect for Limiter {
    fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        let ceil = db(self.p[0].value());
        let rel = time_coef(self.p[1].value(), self.sr);
        for (a, b) in l.iter_mut().zip(r.iter_mut()) {
            let peak = a.abs().max(b.abs());
            let want = if peak > ceil { ceil / peak } else { 1.0 };
            self.gain = if want < self.gain {
                want
            } else {
                want + (self.gain - want) * rel
            };
            *a *= self.gain;
            *b *= self.gain;
        }
    }
    fn reset(&mut self) {
        self.gain = 1.0;
    }
}

// ── Ping-pong delay ──────────────────────────────────────────────────────

struct PingPong {
    p: Vec<Shared>,
    sr: f32,
    dl: [DelayLine; 2],
    tone: [Box<dyn AudioUnit>; 2],
    time: Smooth,
}

impl PingPong {
    fn new(p: Vec<Shared>, sr: f32) -> Self {
        let max = (2.1 * sr) as usize;
        let time = Smooth::new(p[0].value() * 0.001 * sr, sr);
        let lp = || unit(Box::new(fp::lowpass::<f32>()), sr);
        Self {
            p,
            sr,
            dl: [DelayLine::new(max), DelayLine::new(max)],
            tone: [lp(), lp()],
            time,
        }
    }
}

impl Effect for PingPong {
    fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        let t = self.p[0].value() * 0.001 * self.sr;
        let fb = self.p[1].value();
        let mix = self.p[2].value();
        let ping = self.p[3].value() > 0.5;
        let tone = self.p[4].value();
        let mut o = [0.0f32];
        for (a, b) in l.iter_mut().zip(r.iter_mut()) {
            let d = self.time.next(t);
            let mut wl = self.dl[0].read(d);
            let mut wr = self.dl[1].read(d);
            self.tone[0].tick(&[wl, tone, 0.707], &mut o);
            wl = o[0];
            self.tone[1].tick(&[wr, tone, 0.707], &mut o);
            wr = o[0];
            if ping {
                // Mono input feeds the left line; the lines cross-feed.
                self.dl[0].write((*a + *b) * 0.5 + wr * fb);
                self.dl[1].write(wl * fb);
            } else {
                self.dl[0].write(*a + wl * fb);
                self.dl[1].write(*b + wr * fb);
            }
            // Dry stays at full level; Mix sets the echo level.
            *a += wl * mix;
            *b += wr * mix;
        }
    }
    fn reset(&mut self) {
        self.dl.iter_mut().for_each(DelayLine::clear);
        self.tone.iter_mut().for_each(|u| u.reset());
    }
}

// ── Rebuild-on-change fundsp graphs (reverb / chorus / flanger) ──────────

type Maker = fn(&[f32], f32) -> Box<dyn AudioUnit>;

/// Wraps a 2-in / 2-out fundsp graph whose first `structural` params are
/// baked in at build time. When those change, a new graph is built and
/// crossfaded in while the old one's tail decays. The last param is `Mix`
/// (dry / wet), applied here.
struct Rebuilt {
    p: Vec<Shared>,
    sr: f32,
    structural: usize,
    make: Maker,
    key: Vec<f32>,
    cur: Box<dyn AudioUnit>,
    old: Option<(Box<dyn AudioUnit>, usize)>,
    /// Graph output already contains the dry signal (chorus / flanger).
    includes_dry: bool,
    mix: Smooth,
}

const XFADE: usize = 4096;

impl Rebuilt {
    fn new(p: Vec<Shared>, sr: f32, structural: usize, make: Maker, includes_dry: bool) -> Self {
        let key: Vec<f32> = p[..structural].iter().map(Shared::value).collect();
        let cur = make(&key, sr);
        let mix = Smooth::new(p[p.len() - 1].value(), sr);
        Self {
            p,
            sr,
            structural,
            make,
            key,
            cur,
            old: None,
            includes_dry,
            mix,
        }
    }

    fn maybe_rebuild(&mut self) {
        if self.old.is_some() {
            return; // finish the current crossfade first
        }
        let now: Vec<f32> = self.p[..self.structural]
            .iter()
            .map(Shared::value)
            .collect();
        let changed = now
            .iter()
            .zip(&self.key)
            .any(|(a, b)| (a - b).abs() > 1e-3 * b.abs().max(1e-3));
        if changed {
            let new = (self.make)(&now, self.sr);
            let old = std::mem::replace(&mut self.cur, new);
            self.old = Some((old, XFADE));
            self.key = now;
        }
    }
}

impl Effect for Rebuilt {
    fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        self.maybe_rebuild();
        let tmix = self.p[self.p.len() - 1].value();
        let mut o = [0.0f32; 2];
        let mut oo = [0.0f32; 2];
        for (a, b) in l.iter_mut().zip(r.iter_mut()) {
            let inp = [*a, *b];
            self.cur.tick(&inp, &mut o);
            if let Some((old, left)) = &mut self.old {
                old.tick(&inp, &mut oo);
                let t = *left as f32 / XFADE as f32;
                o[0] = o[0] * (1.0 - t) + oo[0] * t;
                o[1] = o[1] * (1.0 - t) + oo[1] * t;
                *left -= 1;
                if *left == 0 {
                    self.old = None;
                }
            }
            let mix = self.mix.next(tmix);
            if self.includes_dry {
                *a += (o[0] - *a) * mix;
                *b += (o[1] - *b) * mix;
            } else {
                *a = *a * (1.0 - mix * 0.5) + o[0] * mix;
                *b = *b * (1.0 - mix * 0.5) + o[1] * mix;
            }
        }
    }
    fn reset(&mut self) {
        self.cur.reset();
        self.old = None;
    }
}

fn make_reverb(k: &[f32], sr: f32) -> Box<dyn AudioUnit> {
    unit(
        Box::new(fp::reverb_stereo(k[0] as f64, k[1] as f64, k[2] as f64)),
        sr,
    )
}

fn make_chorus(k: &[f32], sr: f32) -> Box<dyn AudioUnit> {
    let (rate, depth) = (k[0], k[1] * 0.001);
    // Different seeds per side give a wide, decorrelated stereo chorus.
    unit(
        Box::new(fp::chorus(11, 0.008, depth, rate) | fp::chorus(29, 0.008, depth, rate * 1.07)),
        sr,
    )
}

fn make_flanger(k: &[f32], sr: f32) -> Box<dyn AudioUnit> {
    let (rate, depth, fb) = (k[0], k[1] * 0.001, k[2]);
    let (lo, hi) = (0.0005f32, 0.0005 + depth);
    let left = fp::flanger(fb, lo, hi, move |t| fp::lerp11(lo, hi, fp::sin_hz(rate, t)));
    let right = fp::flanger(fb, lo, hi, move |t| fp::lerp11(lo, hi, fp::cos_hz(rate, t)));
    unit(Box::new(left | right), sr)
}

// ── Bitcrusher ───────────────────────────────────────────────────────────

struct Bitcrusher {
    p: Vec<Shared>,
    hold: [f32; 2],
    phase: f32,
}

impl Bitcrusher {
    fn new(p: Vec<Shared>) -> Self {
        Self {
            p,
            hold: [0.0; 2],
            phase: 0.0,
        }
    }
}

impl Effect for Bitcrusher {
    fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        let levels = 2f32.powf(self.p[0].value() - 1.0);
        let step = 1.0 / self.p[1].value().max(1.0);
        let mix = self.p[2].value();
        for (a, b) in l.iter_mut().zip(r.iter_mut()) {
            self.phase += step;
            if self.phase >= 1.0 {
                self.phase -= 1.0;
                self.hold = [
                    (*a * levels).round() / levels,
                    (*b * levels).round() / levels,
                ];
            }
            *a += (self.hold[0] - *a) * mix;
            *b += (self.hold[1] - *b) * mix;
        }
    }
    fn reset(&mut self) {
        self.hold = [0.0; 2];
        self.phase = 1.0;
    }
}

// ── Overdrive ────────────────────────────────────────────────────────────

struct Overdrive {
    p: Vec<Shared>,
    tone: [Box<dyn AudioUnit>; 2],
    drive: Smooth,
    level: Smooth,
}

impl Overdrive {
    fn new(p: Vec<Shared>, sr: f32) -> Self {
        let lp = || unit(Box::new(fp::lowpass::<f32>()), sr);
        let drive = Smooth::new(db(p[0].value()), sr);
        let level = Smooth::new(db(p[2].value()), sr);
        Self {
            p,
            tone: [lp(), lp()],
            drive,
            level,
        }
    }
}

impl Effect for Overdrive {
    fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        let (td, tone, tl, mix) = (
            db(self.p[0].value()),
            self.p[1].value(),
            db(self.p[2].value()),
            self.p[3].value(),
        );
        let mut o = [0.0f32];
        for (a, b) in l.iter_mut().zip(r.iter_mut()) {
            let d = self.drive.next(td);
            let lv = self.level.next(tl);
            for (ch, x) in [a, b].into_iter().enumerate() {
                let wet = (*x * d).tanh();
                self.tone[ch].tick(&[wet, tone, 0.707], &mut o);
                *x += (o[0] * lv - *x) * mix;
            }
        }
    }
    fn reset(&mut self) {
        self.tone.iter_mut().for_each(|u| u.reset());
    }
}

// ── Tape wobble (wow + flutter modulated delay) ──────────────────────────

struct TapeWobble {
    p: Vec<Shared>,
    sr: f32,
    dl: [DelayLine; 2],
    phase: f32,
    flutter_phase: f32,
}

impl TapeWobble {
    fn new(p: Vec<Shared>, sr: f32) -> Self {
        let n = (0.02 * sr) as usize;
        Self {
            p,
            sr,
            dl: [DelayLine::new(n), DelayLine::new(n)],
            phase: 0.0,
            flutter_phase: 0.0,
        }
    }
}

impl Effect for TapeWobble {
    fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        let rate = self.p[0].value();
        let depth = self.p[1].value() * 0.001 * self.sr;
        let flutter = self.p[2].value();
        let mix = self.p[3].value();
        let base = depth + 2.0;
        let tau = std::f32::consts::TAU;
        for (a, b) in l.iter_mut().zip(r.iter_mut()) {
            self.phase = (self.phase + rate / self.sr).fract();
            self.flutter_phase = (self.flutter_phase + 13.0 / self.sr).fract();
            let m =
                (self.phase * tau).sin() * 0.8 + (self.flutter_phase * tau).sin() * 0.2 * flutter;
            let d = base + m * depth;
            self.dl[0].write(*a);
            self.dl[1].write(*b);
            let (wl, wr) = (self.dl[0].read(d), self.dl[1].read(d));
            *a += (wl - *a) * mix;
            *b += (wr - *b) * mix;
        }
    }
    fn reset(&mut self) {
        self.dl.iter_mut().for_each(DelayLine::clear);
    }
}

// ── Stereo tools ─────────────────────────────────────────────────────────

struct Widener {
    p: Vec<Shared>,
    w: Smooth,
}

impl Widener {
    fn new(p: Vec<Shared>, sr: f32) -> Self {
        let w = Smooth::new(p[0].value(), sr);
        Self { p, w }
    }
}

impl Effect for Widener {
    fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        let t = self.p[0].value();
        for (a, b) in l.iter_mut().zip(r.iter_mut()) {
            let w = self.w.next(t);
            let m = (*a + *b) * 0.5;
            let s = (*a - *b) * 0.5 * w;
            *a = m + s;
            *b = m - s;
        }
    }
    fn reset(&mut self) {}
}

struct Haas {
    p: Vec<Shared>,
    sr: f32,
    dl: DelayLine,
    d: Smooth,
}

impl Haas {
    fn new(p: Vec<Shared>, sr: f32) -> Self {
        let d = Smooth::new(p[0].value() * 0.001 * sr, sr);
        Self {
            p,
            sr,
            dl: DelayLine::new((0.045 * sr) as usize),
            d,
        }
    }
}

impl Effect for Haas {
    fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        let t = self.p[0].value() * 0.001 * self.sr;
        let delay_left = self.p[1].value() > 0.5;
        let mix = self.p[2].value();
        for (a, b) in l.iter_mut().zip(r.iter_mut()) {
            let d = self.d.next(t);
            let x = if delay_left { &mut *a } else { &mut *b };
            self.dl.write(*x);
            let w = self.dl.read(d.max(1.0));
            *x += (w - *x) * mix;
        }
    }
    fn reset(&mut self) {
        self.dl.clear();
    }
}

struct AutoPan {
    p: Vec<Shared>,
    sr: f32,
    phase: f32,
}

impl AutoPan {
    fn new(p: Vec<Shared>, sr: f32) -> Self {
        Self { p, sr, phase: 0.0 }
    }
}

impl Effect for AutoPan {
    fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        let rate = self.p[0].value();
        let depth = self.p[1].value();
        for (a, b) in l.iter_mut().zip(r.iter_mut()) {
            self.phase = (self.phase + rate / self.sr).fract();
            let pan = (self.phase * std::f32::consts::TAU).sin() * depth;
            *a *= (1.0 - pan).min(1.0);
            *b *= (1.0 + pan).min(1.0);
        }
    }
    fn reset(&mut self) {
        self.phase = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(kind: BlockKind, sr: f64, input: &[f32]) -> (Vec<f32>, Vec<f32>) {
        let params: Vec<Shared> = kind
            .params()
            .iter()
            .map(|d| Shared::new(d.default))
            .collect();
        let mut fx = build(kind, &params, sr);
        let mut l = input.to_vec();
        let mut r = input.to_vec();
        for (cl, cr) in l.chunks_mut(256).zip(r.chunks_mut(256)) {
            fx.process(cl, cr);
        }
        (l, r)
    }

    #[test]
    fn every_block_is_finite_and_bounded() {
        let mut noise = 0x1234_5678u32;
        let input: Vec<f32> = (0..24_000)
            .map(|i| {
                noise ^= noise << 13;
                noise ^= noise >> 17;
                noise ^= noise << 5;
                if i == 0 {
                    1.0
                } else {
                    (noise as f32 / u32::MAX as f32 - 0.5) * 0.8
                }
            })
            .collect();
        for sr in [44_100.0, 48_000.0] {
            for kind in BlockKind::ALL {
                let (l, r) = run(kind, sr, &input);
                for v in l.iter().chain(&r) {
                    assert!(v.is_finite(), "{kind:?} @ {sr} produced {v}");
                    assert!(v.abs() < 20.0, "{kind:?} @ {sr} blew up: {v}");
                }
            }
        }
    }

    #[test]
    fn param_change_is_audible() {
        // Low-pass at 200 Hz must attenuate a 5 kHz tone far more than at 18 kHz.
        let sr = 48_000.0f32;
        let tone: Vec<f32> = (0..9600)
            .map(|i| (i as f32 * 5000.0 * std::f32::consts::TAU / sr).sin() * 0.5)
            .collect();
        let rms = |cut: f32| {
            let params: Vec<Shared> = [0.0, cut, 0.707].iter().map(|&v| Shared::new(v)).collect();
            let mut fx = build(BlockKind::Filter, &params, sr as f64);
            let (mut l, mut r) = (tone.clone(), tone.clone());
            fx.process(&mut l, &mut r);
            (l[4800..].iter().map(|x| x * x).sum::<f32>() / 4800.0).sqrt()
        };
        assert!(rms(200.0) < rms(18_000.0) * 0.1);
    }

    #[test]
    fn param_defaults_in_range() {
        for kind in BlockKind::ALL {
            for d in kind.params() {
                assert!(
                    d.min <= d.default && d.default <= d.max,
                    "{kind:?} {}",
                    d.name
                );
            }
        }
    }
}

//! ⌬ OVERTONE_RIOT — dsp_core.rs
//! ─────────────────────────────────────────────────────────────────────────
//! Rust twin of the JS reference core embedded in overtone_riot.html.
//! Build:   wasm-pack build --target web --release
//! Deploy:  instantiate WebAssembly *inside* the AudioWorklet
//!          (fetch .wasm bytes on the main thread, transfer the ArrayBuffer
//!          through processorOptions, WebAssembly.instantiate in the worklet
//!          constructor — no async fetch is allowed on the audio thread).
//!
//! Param bridge: the UI thread owns a SharedArrayBuffer of NP f32 (normalized
//! 0‥1). Each process() block, the worklet copies it into Wasm linear memory
//! via `params_ptr()` — one memcpy, zero locks, zero latency. Requires
//! COOP/COEP response headers:
//!     Cross-Origin-Opener-Policy:   same-origin
//!     Cross-Origin-Embedder-Policy: require-corp
//! Without them the JS shell falls back to port.postMessage (already wired).
//!
//! Cargo.toml:
//!     [lib] crate-type = ["cdylib"]
//!     [dependencies] wasm-bindgen = "0.2"
//!     [profile.release] opt-level = 3, lto = true
//! ─────────────────────────────────────────────────────────────────────────

use wasm_bindgen::prelude::*;

const TAU: f32 = 6.283_185_3;
const NP: usize = 63;
const NVOICE: usize = 8;
const NTAIL: usize = 2;    // steal fade-pool slots
const NPART: usize = 16;
const FRAME: usize = 2048;
const MAX_FRAMES: usize = 64;

// (min, max, exponential?) — must stay in lockstep with PARAMS in the JS shell
const PDEF: [(f32, f32, bool); NP] = [
    (0.0, 1.0, false), (0.0, 1.0, false), (0.0, 1.0, false),          // sub lvl/morph/drive
    (0.0, 1.0, false), (0.005, 0.4, true),                            // pitch drop / decay
    (0.0, 1.0, false), (0.002, 0.08, true),                           // transient lvl / decay
    (0.0, 1.0, false), (0.0, 1.0, false), (0.0, 1.0, false),          // add lvl/tilt/spread
    (0.0, 1.0, false), (0.0, 1.0, false), (0.0, 1.0, false), (0.0, 1.0, false), // h1‥h16
    (0.0, 1.0, false), (0.0, 1.0, false), (0.0, 1.0, false), (0.0, 1.0, false),
    (0.0, 1.0, false), (0.0, 1.0, false), (0.0, 1.0, false), (0.0, 1.0, false),
    (0.0, 1.0, false), (0.0, 1.0, false), (0.0, 1.0, false), (0.0, 1.0, false),
    (0.0, 1.0, false), (0.0, 1.0, false), (0.0, 1.0, false), (0.0, 1.0, false), // tex lvl/morph/grain/det
    (0.001, 2.0, true), (0.02, 4.0, true), (0.0, 1.0, false), (0.02, 6.0, true), // ADSR
    (0.05, 30.0, true), (0.0, 1.0, false), (0.0, 3.0, false),         // lfo1 rate/depth/shape
    (0.0, 1.0, false),                                                // master
    (0.05, 30.0, true), (0.0, 1.0, false), (0.0, 3.0, false),         // lfo2 rate/depth/shape (38-40)
    // ═ FX RACK — analog (41-49) ═
    (0.0, 1.0, false), (0.05, 8.0, true), (0.0, 0.9, false),          // phaser mix/rate/regen
    (0.0, 1.0, false), (0.1, 5.0, true), (0.0, 1.0, false),           // chorus mix/rate/depth
    (0.0, 1.0, false), (0.5, 12.0, true),                             // trem depth/rate
    (0.0, 1.0, false),                                                // drift
    // ═ hybrid (50-57) ═
    (0.0, 1.0, false), (0.02, 1.2, true), (0.0, 0.95, false), (0.0, 1.0, false), // dly mix/time/fdbk/color
    (0.0, 1.0, false), (0.0, 1.0, false), (0.0, 1.0, false), (0.0, 1.0, false),  // vrb mix/size/decay + bleed
    // ═ digital distortion (58-62) ═
    (0.0, 1.0, false), (0.0, 5.0, false), (0.0, 1.0, false),          // drive/mode/tone
    (0.0, 1.0, false), (0.0, 1.0, false),                             // bias/mix
];

#[inline(always)]
fn map_p(i: usize, x: f32) -> f32 {
    let (mn, mx, exp) = PDEF[i];
    let x = x.clamp(0.0, 1.0);
    if exp { mn * (mx / mn).powf(x) } else { mn + (mx - mn) * x }
}

#[derive(Clone, Copy)]
enum Stage { Idle, Attack, Decay, Sustain, Release, Kill }

#[derive(Clone, Copy)]
struct Voice {
    on: bool, gate: bool, note: u8, f0: f32, vel: f32, age: f64, t: f32,
    ph_s: f32,                 // sub phase
    ph_a: [f32; NPART],        // additive partial phases
    ph_t: f32, ph_t2: f32,     // texture read heads
    g_fade: f32, g_cnt: i32,   // grain crossfade / countdown
    env: f32, stage: Stage,
    pn: f32,                   // previous noise sample (HP differencer)
    tr_samp: i32,              // transient click sample budget — set once at
                               // note-on, only counts DOWN: the burst cannot
                               // stick on or re-arm across fast retriggers
}

/// FX rack state — distortion → analog → hybrid, on the summed voice bus.
struct Fx {
    rng: u32,
    drift: f32,
    ph_ph: f32, ap_x: [f32; 6], ap_y: [f32; 6], ph_fb: f32,
    cho: Box<[f32; 8192]>, cho_w: usize, cho_ph: f32,
    trem_ph: f32,
    dly: Box<[f32; 131072]>, dly_w: usize, dly_lp: f32, dly_smp: f32,
    ap1: Box<[f32; 512]>, ap1_i: usize, ap2: Box<[f32; 1024]>, ap2_i: usize,
    vb: [Box<[f32; 16384]>; 4], vi: [usize; 4], vlp: [f32; 4], vl: [f32; 4],
    dc_x: f32, dc_y: f32, ic_y: f32, cmp_env: f32,
    scr_lp: f32, scr_bp: f32, scr_env: f32,
    crush_hold: f32, crush_cnt: i32, tone_lp: f32,
}

impl Fx {
    fn new() -> Fx {
        Fx {
            rng: 0x51F1_5EED,          // bit-identical stream to fxRand in the JS shell
            drift: 0.0,
            ph_ph: 0.0, ap_x: [0.0; 6], ap_y: [0.0; 6], ph_fb: 0.0,
            cho: Box::new([0.0; 8192]), cho_w: 0, cho_ph: 0.0,
            trem_ph: 0.0,
            dly: Box::new([0.0; 131072]), dly_w: 0, dly_lp: 0.0, dly_smp: 4800.0,
            ap1: Box::new([0.0; 512]), ap1_i: 0, ap2: Box::new([0.0; 1024]), ap2_i: 0,
            vb: [Box::new([0.0; 16384]), Box::new([0.0; 16384]),
                 Box::new([0.0; 16384]), Box::new([0.0; 16384])],
            vi: [0; 4], vlp: [0.0; 4], vl: [1687.0, 2039.0, 2503.0, 3023.0],
            dc_x: 0.0, dc_y: 0.0, ic_y: 0.0, cmp_env: 0.0,
            scr_lp: 0.0, scr_bp: 0.0, scr_env: 0.0,
            crush_hold: 0.0, crush_cnt: 0, tone_lp: 0.0,
        }
    }
}

#[wasm_bindgen]
pub struct RiotCore {
    sr: f32,
    params: [f32; NP],                       // normalized; UI memcpys SAB here
    routes: [(usize, f32, u32); 16], n_routes: usize, // (param, depth, src 0|1)
    voices: [Voice; NVOICE],
    tails: [Voice; NTAIL],                   // steal fade-pool: stolen voices
                                             // render out a <2ms forced kill here
    table: Box<[f32; FRAME * MAX_FRAMES]>, n_frames: usize,
    lfo_ph: f32, sh: f32, sh_ph: f32,
    lfo2_ph: f32, sh2: f32, sh2_ph: f32,
    rng: u32,                                // xorshift — no std RNG on audio thread
    clock: f64,
    fx: Fx,
}

#[wasm_bindgen]
impl RiotCore {
    #[wasm_bindgen(constructor)]
    pub fn new(sample_rate: f32) -> RiotCore {
        let v = Voice { on: false, gate: false, note: 0, f0: 0.0, vel: 1.0, age: 0.0,
            t: 0.0, ph_s: 0.0, ph_a: [0.0; NPART], ph_t: 0.0, ph_t2: 0.0,
            g_fade: 1.0, g_cnt: 0, env: 0.0, stage: Stage::Idle, pn: 0.0, tr_samp: 0 };
        let mut core = RiotCore {
            sr: sample_rate, params: [0.0; NP],
            routes: [(0, 0.0, 0); 16], n_routes: 0,
            voices: [v; NVOICE],
            tails: [v; NTAIL],
            table: Box::new([0.0; FRAME * MAX_FRAMES]), n_frames: 0,
            lfo_ph: 0.0, sh: 0.0, sh_ph: 1.0,
            lfo2_ph: 0.0, sh2: 0.0, sh2_ph: 1.0,
            rng: 0x9E37_79B9, clock: 0.0,
            fx: Fx::new(),
        };
        core.factory_table();
        core
    }

    /// Pointers into linear memory: the worklet builds Float32Array views once
    /// and memcpys SAB→params / reads the rendered block with zero marshaling.
    pub fn params_ptr(&self) -> *const f32 { self.params.as_ptr() }
    pub fn table_ptr(&mut self) -> *mut f32 { self.table.as_mut_ptr() }
    pub fn table_capacity(&self) -> usize { FRAME * MAX_FRAMES }  // shell clamps uploads to this
    pub fn set_table_frames(&mut self, n: usize) { self.n_frames = n.min(MAX_FRAMES); }

    pub fn set_route(&mut self, slot: usize, param: usize, depth: f32, src: u32) {
        if slot < 16 { self.routes[slot] = (param.min(NP - 1), depth, src.min(1)); }
    }
    pub fn set_route_count(&mut self, n: usize) { self.n_routes = n.min(16); }

    pub fn note_on(&mut self, note: u8, vel: f32) {
        let idx = match self.voices.iter().position(|v| !v.on) {
            Some(i) => i,
            None => {
                // steal: prefer a releasing voice (quietest first), oldest last
                let mut best: Option<usize> = None;
                for i in 0..NVOICE {
                    if matches!(self.voices[i].stage, Stage::Release)
                        && best.map_or(true, |b| self.voices[i].env < self.voices[b].env)
                    { best = Some(i); }
                }
                let i = best.unwrap_or_else(|| {
                    let mut oldest = 0;
                    for k in 1..NVOICE { if self.voices[k].age < self.voices[oldest].age { oldest = k; } }
                    oldest
                });
                // Snapshot the dying voice into a fade-pool slot: it keeps
                // rendering with a forced ~1.2ms kill so the slot swap has no
                // step discontinuity. Skip if already inaudible.
                let snap = self.voices[i];
                if snap.env > 1e-3 {
                    let ti = self.tails.iter().position(|t| !t.on).unwrap_or_else(|| {
                        let mut q = 0;               // both busy: overwrite quietest
                        for k in 1..NTAIL { if self.tails[k].env < self.tails[q].env { q = k; } }
                        q
                    });
                    self.tails[ti] = Voice {
                        gate: false, stage: Stage::Kill,
                        tr_samp: 0,                  // noise has no phase to preserve —
                                                     // don't double the click burst
                        ..snap };
                }
                i
            }
        };
        let r1 = self.rand(); let r2 = self.rand();
        // Transient click gets a HARD sample budget, set only here on note
        // start. It can only count DOWN — the process loop cannot re-arm it —
        // so the noise burst physically cannot stick on or leak across rapid
        // retriggers (fast 808 lines). Budget scales with current click decay.
        let tr_budget = ((map_p(6, self.params[6]) * 6.0 * self.sr) as i32).max(1);
        let v = &mut self.voices[idx];
        *v = Voice { on: true, gate: true, note, vel,
            f0: 440.0 * 2f32.powf((note as f32 - 69.0) / 12.0),
            age: self.clock, t: 0.0, ph_s: 0.0, ph_a: [0.0; NPART],
            ph_t: r1, ph_t2: r2,
            g_fade: 1.0, g_cnt: 0, env: 0.0, stage: Stage::Attack, pn: 0.0,
            tr_samp: tr_budget };
    }

    pub fn note_off(&mut self, note: u8) {
        for v in self.voices.iter_mut() {
            if v.on && v.gate && v.note == note { v.gate = false; v.stage = Stage::Release; }
        }
    }

    pub fn panic(&mut self) {
        for v in self.voices.iter_mut().chain(self.tails.iter_mut()) {
            v.on = false; v.env = 0.0; v.stage = Stage::Idle;
        }
    }

    /// Render one AudioWorklet quantum. `out` is the worklet's Float32Array
    /// view over Wasm memory (len = 128). Allocation-free, branch-lean.
    pub fn process(&mut self, out: &mut [f32]) {
        let n = out.len();
        let dt = 1.0 / self.sr;
        self.clock += n as f64 * dt as f64;

        // ── control block: LFOs + mod matrix in normalized space ──
        let rate = map_p(34, self.params[34]);
        let depth = map_p(35, self.params[35]);
        let shape = map_p(36, self.params[36]).round() as u32;
        self.lfo_ph += rate * n as f32 * dt; if self.lfo_ph >= 1.0 { self.lfo_ph -= 1.0; }
        self.sh_ph += rate * n as f32 * dt;
        if self.sh_ph >= 1.0 { self.sh_ph -= 1.0; self.sh = self.rand() * 2.0 - 1.0; }
        let lfo = match shape {
            0 => (TAU * self.lfo_ph).sin(),
            1 => 1.0 - 4.0 * (self.lfo_ph - 0.5).abs(),
            2 => 2.0 * self.lfo_ph - 1.0,
            _ => self.sh,
        };
        let rate2 = map_p(38, self.params[38]);
        let depth2 = map_p(39, self.params[39]);
        let shape2 = map_p(40, self.params[40]).round() as u32;
        self.lfo2_ph += rate2 * n as f32 * dt; if self.lfo2_ph >= 1.0 { self.lfo2_ph -= 1.0; }
        self.sh2_ph += rate2 * n as f32 * dt;
        if self.sh2_ph >= 1.0 { self.sh2_ph -= 1.0; self.sh2 = self.rand() * 2.0 - 1.0; }
        let lfo2 = match shape2 {
            0 => (TAU * self.lfo2_ph).sin(),
            1 => 1.0 - 4.0 * (self.lfo2_ph - 0.5).abs(),
            2 => 2.0 * self.lfo2_ph - 1.0,
            _ => self.sh2,
        };
        let mut q = self.params;
        for k in 0..self.n_routes {
            let (i, d, src) = self.routes[k];
            let m = if src == 1 { lfo2 * depth2 } else { lfo * depth };
            q[i] += m * d;
        }

        // map once per block
        let (sub_lv, morph, drive) = (map_p(0, q[0]), map_p(1, q[1]), map_p(2, q[2]));
        let (p_drop, p_dec) = (map_p(3, q[3]), map_p(4, q[4]));
        let (tr_lv, tr_dec) = (map_p(5, q[5]), map_p(6, q[6]));
        let (add_lv, tilt, spread) = (map_p(7, q[7]), map_p(8, q[8]), map_p(9, q[9]));
        let (tex_lv, text_m, grain, det) = (map_p(26, q[26]), map_p(27, q[27]), map_p(28, q[28]), map_p(29, q[29]));
        let (a_a, a_d, a_s, a_r) = (map_p(30, q[30]), map_p(31, q[31]), map_p(32, q[32]), map_p(33, q[33]));
        let master = map_p(37, q[37]);

        // additive weights: user partials × spectral tilt, constant-power norm
        let t_exp = (tilt - 0.5) * 4.0;
        let mut w = [0f32; NPART];
        let mut w_sum = 1e-9f32;
        let mut p_max = 0usize;                  // highest audible partial (count)
        for p in 0..NPART {
            w[p] = map_p(10 + p, q[10 + p]) * ((p + 1) as f32).powf(t_exp);
            w_sum += w[p] * w[p];
            if w[p] >= 1e-5 { p_max = p + 1; }
        }
        let w_norm = w_sum.sqrt().recip();
        // Load-adaptive partial cap: when the voice pool is stacked, trim the
        // partial count so total sine-calls/sample stays bounded on weak CPUs
        // (Moto G-class). Counts active voices once per block.
        let mut active_v = 0usize;
        for v in self.voices.iter().chain(self.tails.iter()) { if v.on { active_v += 1; } }
        let partial_cap = if active_v <= 3 { 16 }
                     else if active_v <= 5 { 12 }
                     else if active_v <= 6 { 9 } else { 7 };
        let p_top = p_max.min(partial_cap);

        let c_a = 1.0 - (-dt / (a_a * 0.35)).exp();
        let c_d = 1.0 - (-dt / (a_d * 0.35)).exp();
        let c_r = 1.0 - (-dt / (a_r * 0.35)).exp();
        let g = 1.0 + drive * 9.0;
        let g_inv = g.tanh().recip();

        let nf = self.n_frames.max(2);
        let fm = text_m * (nf - 1) as f32;
        let fi = (fm as usize).min(nf - 2);
        let ff = fm - fi as f32;
        let grain_len: i32 = if grain > 0.0 { ((0.25 - grain * 0.23) * self.sr).max(256.0) as i32 } else { 0 };

        out.fill(0.0);

        for vi in 0..(NVOICE + NTAIL) {
            // Voice is Copy: work on a local, write back to the right pool
            let mut v = if vi < NVOICE { self.voices[vi] } else { self.tails[vi - NVOICE] };
            if !v.on { continue; }
            let (f0, vel) = (v.f0, v.vel);
            for s in 0..n {
                // envelope
                match v.stage {
                    Stage::Attack => { v.env += (1.02 - v.env) * c_a;
                        if v.env >= 1.0 { v.env = 1.0; v.stage = Stage::Decay; } }
                    Stage::Decay => { v.env += (a_s - v.env) * c_d;
                        if (v.env - a_s).abs() < 1e-4 { v.stage = Stage::Sustain; } }
                    Stage::Sustain => { v.env += (a_s - v.env) * c_d; } // track live SUSTAIN moves
                    Stage::Release => { v.env -= v.env * c_r;
                        if v.env < 1e-4 { v.on = false; break; } }
                    Stage::Kill => { v.env *= 0.85;      // steal kill: silent in
                                                         // ~57 samples (<1.5ms)
                        if v.env < 1e-4 { v.on = false; break; } }
                    _ => {}
                }

                // ── PILLAR 1 · sub/impact ──
                let f_drop = f0 * (1.0 + p_drop * 6.0 * (-v.t / p_dec).exp());
                v.ph_s += f_drop * dt; if v.ph_s >= 1.0 { v.ph_s -= 1.0; }
                let sn = (TAU * v.ph_s).sin();
                let tri = 0.636_619_77 * sn.asin();            // 2/π·asin → triangle
                let shaped = sn + morph * (tri - sn);
                let mut sub = (shaped * g).tanh() * g_inv;
                if tr_lv > 0.0 && v.tr_samp > 0 {              // HP'd noise click — hard-gated
                    let n0 = self.rand() * 2.0 - 1.0;
                    sub += (n0 - v.pn) * 0.5 * tr_lv * (-v.t / tr_dec).exp();
                    v.pn = n0;
                    v.tr_samp -= 1;                            // can only expire, never re-arm
                }

                // ── PILLAR 2 · additive overtone array ──
                let mut add = 0.0f32;
                for p in 0..p_top {
                    let wp = w[p]; if wp < 1e-5 { continue; }
                    let np = (p + 1) as f32;
                    let fn_ = f0 * np * (1.0 + spread * 0.0009 * np * np);
                    if fn_ > self.sr * 0.48 { continue; }       // alias guard
                    v.ph_a[p] += fn_ * dt; if v.ph_a[p] >= 1.0 { v.ph_a[p] -= 1.0; }
                    add += wp * (TAU * v.ph_a[p]).sin();
                }
                add *= w_norm;

                // ── PILLAR 3 · granular / wavetable ──
                let mut tex = 0.0f32;
                if tex_lv > 0.001 {
                    if grain_len > 0 {
                        v.g_cnt -= 1;
                        if v.g_cnt <= 0 {
                            v.g_cnt = grain_len + (self.rand() * grain_len as f32) as i32;
                            v.g_fade = 0.0;
                            v.ph_t = self.rand();
                        }
                        if v.g_fade < 1.0 { v.g_fade = (v.g_fade + 1.0 / 384.0).min(1.0); }
                    }
                    v.ph_t += f0 * dt; if v.ph_t >= 1.0 { v.ph_t -= 1.0; }
                    v.ph_t2 += f0 * (1.0 + det * 0.012) * dt; if v.ph_t2 >= 1.0 { v.ph_t2 -= 1.0; }
                    let h1 = self.read_table(fi, ff, v.ph_t) * if grain_len > 0 { v.g_fade } else { 1.0 };
                    let h2 = self.read_table(fi, ff, v.ph_t2);
                    tex = (h1 + h2) * 0.5;
                }

                out[s] += (sub * sub_lv + add * add_lv + tex * tex_lv) * v.env * vel * 0.35;
                v.t += dt;
            }
            if vi < NVOICE { self.voices[vi] = v; } else { self.tails[vi - NVOICE] = v; }
        }

        self.process_fx(out, &q);                    // ═ FX RACK (see above) ═
        for s in out.iter_mut() { *s = (*s * master * 1.4).tanh(); }   // master soft clip
    }
}

impl RiotCore {
    #[inline(always)]
    fn fx_rand(&mut self) -> f32 {                   // u32 xorshift — twin of fxRand (JS)
        let mut x = self.fx.rng;
        x ^= x << 13; x ^= x >> 17; x ^= x << 5;
        self.fx.rng = x;
        (x as f32) * (1.0 / 4_294_967_296.0)
    }

    /// ═══ FX RACK · summed voice bus, pre-master ═══
    /// chain: DIGITAL DISTORTION → ANALOG (phaser·chorus·trem) → HYBRID (dly⇄verb)
    /// Sections hard-bypass at mix≈0 so default patches are bit-identical.
    fn process_fx(&mut self, out: &mut [f32], q: &[f32; NP]) {
        let n = out.len();
        let sr = self.sr;
        let dt = 1.0 / sr;
        let sr_k = sr / 48000.0;
        let d_mix = map_p(62, q[62]);
        let p_mix = map_p(41, q[41]); let c_mix = map_p(44, q[44]); let t_dep = map_p(47, q[47]);
        let y_mix = map_p(50, q[50]); let v_mix = map_p(54, q[54]);
        let dist_on = d_mix > 0.001;
        let anlg_on = p_mix > 0.001 || c_mix > 0.001 || t_dep > 0.001;
        let hyb_on  = y_mix > 0.001 || v_mix > 0.001;
        if !dist_on && !anlg_on && !hyb_on { return; }

        // DRIFT — thermal instability: slow random walk nudging every analog LFO
        let dr_amt = map_p(49, q[49]);
        let wander = self.fx_rand() * 2.0 - 1.0;
        self.fx.drift += (wander - self.fx.drift) * 0.004;
        let dr = self.fx.drift * dr_amt;

        // ── distortion block consts ──
        let drive = map_p(58, q[58]); let mode = map_p(59, q[59]).round() as u32;
        let tone = map_p(60, q[60]); let bias = map_p(61, q[61]);
        let big_g = 1.0 + drive * drive * 59.0; let b0 = (bias - 0.5) * 0.6;
        let tone_k = 1.0 - (-TAU * (300.0 * 2f32.powf(tone * 4.0)) / sr).exp();
        let ic_slew = 0.45 / (1.0 + drive * 14.0);   // op-amp slew ceiling/sample
        let cr_q = 2f32.powf(12.0 - drive * 10.0); let cr_n = 1 + (drive * 23.0) as i32;
        let scr_f = (core::f32::consts::PI
            * (0.45f32).min(120.0 * 2f32.powf(tone * 4.5) / sr)).sin();
        let scr_fb = drive * 1.35;

        // ── analog block consts (drift-modulated) ──
        let p_rate = map_p(42, q[42]) * (1.0 + dr * 0.07); let p_fb = map_p(43, q[43]);
        let c_rate = map_p(45, q[45]) * (1.0 + dr * 0.05);
        let c_dep = map_p(46, q[46]) * (1.0 + dr * 0.1);
        let t_rate = map_p(48, q[48]) * (1.0 + dr * 0.04);
        let cho_base = 0.012 * sr; let cho_dep_s = c_dep * 0.009 * sr;

        // ── hybrid block consts ──
        let d_fb = map_p(52, q[52]); let d_col = map_p(53, q[53]);
        let v_size = map_p(55, q[55]); let v_dec = map_p(56, q[56]); let bleed = map_p(57, q[57]);
        let dm2: usize = self.fx.dly.len() - 1;      // 131072-1, power-of-two mask
        let d_tgt = (map_p(51, q[51]) * sr).min((self.fx.dly.len() - 4) as f32);
        self.fx.dly_smp += (d_tgt - self.fx.dly_smp) * 0.03;   // tape-style time glide
        let col_k = 1.0 - (-TAU * (400.0 * 2f32.powf(d_col * 4.6)) / sr).exp();
        let vg = 0.72 + v_dec * 0.26; let v_damp = 0.35;
        let scale = (0.4 + v_size * 1.1) * sr_k;
        const VPRIME: [f32; 4] = [1687.0, 2039.0, 2503.0, 3023.0];
        for k in 0..4 {
            let tgt = (VPRIME[k] * scale).min(16380.0);
            self.fx.vl[k] += (tgt - self.fx.vl[k]) * 0.02;
        }
        let l1 = ((142.0 * sr_k) as usize).min(510);
        let l2 = ((379.0 * sr_k) as usize).min(1022);
        let cm: usize = self.fx.cho.len() - 1;       // 8192-1, power-of-two mask

        let f = &mut self.fx;
        for s in 0..n {
            let mut x = out[s];

            // ═ DIGITAL DISTORTION ═
            if dist_on {
                let mut v = x * big_g + b0;
                let mut y;
                match mode {
                    0 => {         // ▙ CLIP — cubic soft clipper, hard rails
                        y = if v > 1.0 { 0.6667 } else if v < -1.0 { -0.6667 }
                            else { v - v * v * v / 3.0 };
                        y *= 1.5;
                    }
                    1 => {         // ⎍ PUSH A/B — crossover notch + asym rails
                        let xo = 0.02 + bias * 0.05; let av = v.abs();
                        if av < xo { v *= av / xo; }
                        y = if v >= 0.0 { (v * (1.0 + (bias - 0.5) * 0.8)).tanh() }
                            else        { (v * (1.0 - (bias - 0.5) * 0.8)).tanh() };
                    }
                    2 => {         // ▣ IC — op-amp slew limit into rail clip
                        let dv = v - f.ic_y;
                        f.ic_y += dv.clamp(-ic_slew, ic_slew);
                        y = f.ic_y.clamp(-0.9, 0.9) / 0.9;
                    }
                    3 => {         // ◉ COMP — follower-driven gain into tanh
                        let ax = x.abs();
                        f.cmp_env += (ax - f.cmp_env)
                            * if ax > f.cmp_env { 0.012 } else { 0.0009 };
                        y = (v * (1.0 + drive * 7.0)
                            / (1.0 + f.cmp_env * drive * 11.0) * 0.6).tanh();
                    }
                    4 => {         // ⁘ CRUSH — SR hold + bit quantize
                        f.crush_cnt -= 1;
                        if f.crush_cnt <= 0 { f.crush_cnt = cr_n; f.crush_hold = v.tanh(); }
                        y = (f.crush_hold * cr_q).round() / cr_q;
                    }
                    _ => {         // ⚡ SCREAM — biased tanh, resonant feedback howl
                        let w0 = v + f.scr_bp * scr_fb * (1.0 + f.scr_env * 0.5);
                        y = w0.tanh();
                        f.scr_env += (y.abs() - f.scr_env) * 0.004; // envelope rides the howl
                        let hp = y - f.scr_lp - 0.18 * f.scr_bp;
                        f.scr_bp = (f.scr_bp + scr_f * hp).clamp(-3.0, 3.0);
                        f.scr_lp += scr_f * f.scr_bp;               /* Chamberlin semi-implicit fix: update bp phase before lowpass */
                        y *= 0.92;
                    }
                }
                // shared wet path: tone tilt → DC block (bias modes pull DC)
                f.tone_lp += tone_k * (y - f.tone_lp);
                y = f.tone_lp * (1.45 - tone * 0.9) + (y - f.tone_lp) * (0.55 + tone * 0.9);
                let dc_o = y - f.dc_x + 0.995 * f.dc_y; f.dc_x = y; f.dc_y = dc_o;
                x = x * (1.0 - d_mix) + dc_o * d_mix;
            }

            // ═ ANALOG ═
            if anlg_on {
                if p_mix > 0.001 {                   // 6-stage phaser
                    f.ph_ph += p_rate * dt; if f.ph_ph >= 1.0 { f.ph_ph -= 1.0; }
                    let sw = 0.5 + 0.5 * (TAU * f.ph_ph).sin();
                    let wc = core::f32::consts::PI * (220.0 * 2f32.powf(sw * 3.4)) / sr;
                    let a = (1.0 - wc) / (1.0 + wc);
                    let mut t = x + f.ph_fb * p_fb;
                    for k in 0..6 {
                        let yk = a * (t - f.ap_y[k]) + f.ap_x[k];
                        f.ap_x[k] = t; f.ap_y[k] = yk; t = yk;
                    }
                    f.ph_fb = t;
                    x = (x + t * p_mix) / (1.0 + p_mix * 0.35);
                }
                if c_mix > 0.001 {                   // dual-tap chorus
                    f.cho_ph += c_rate * dt; if f.cho_ph >= 1.0 { f.cho_ph -= 1.0; }
                    f.cho[f.cho_w] = x;
                    let d1 = cho_base + cho_dep_s * (0.5 + 0.5 * (TAU * f.cho_ph).sin());
                    let d2 = cho_base * 1.31
                        + cho_dep_s * (0.5 + 0.5 * (TAU * f.cho_ph + 2.094).sin());
                    let mut r = 0.0f32;
                    for dd in [d1, d2] {
                        let rp = f.cho_w as f32 - dd;
                        let rif = rp.floor(); let rf = rp - rif;
                        let ri = (rif as isize as usize) & cm;   // wraps like JS |& mask
                        let r2 = (ri + 1) & cm;
                        r += f.cho[ri] + (f.cho[r2] - f.cho[ri]) * rf;
                    }
                    f.cho_w = (f.cho_w + 1) & cm;
                    x = x * (1.0 - c_mix * 0.5) + r * 0.5 * c_mix;
                }
                if t_dep > 0.001 {                   // sine tremolo
                    f.trem_ph += t_rate * dt; if f.trem_ph >= 1.0 { f.trem_ph -= 1.0; }
                    x *= 1.0 - t_dep * (0.5 + 0.5 * (TAU * f.trem_ph).sin());
                }
                x *= 1.0 + dr * 0.012;               // analog gain wobble
            }

            // ═ HYBRID · delay ⇄ reverb ═
            if hyb_on {
                let rp = f.dly_w as f32 - f.dly_smp;
                let rif = rp.floor(); let rf = rp - rif;
                let ri = (rif as isize as usize) & dm2;
                let d_read = f.dly[ri] + (f.dly[(ri + 1) & dm2] - f.dly[ri]) * rf;
                f.dly_lp += col_k * (d_read - f.dly_lp);   // COLOR: damped repeats
                f.dly[f.dly_w] = x + f.dly_lp * d_fb;
                f.dly_w = (f.dly_w + 1) & dm2;

                let mut wet = 0.0f32;
                if v_mix > 0.001 {
                    // BLEED: repeats diffuse into the reverb alongside the dry feed
                    let mut d = (x + d_read * bleed) * 0.55;
                    let r1 = f.ap1[f.ap1_i]; let t1 = d + 0.5 * r1;
                    f.ap1[f.ap1_i] = t1; d = r1 - 0.5 * t1;
                    f.ap1_i += 1; if f.ap1_i >= l1 { f.ap1_i = 0; }
                    let r2 = f.ap2[f.ap2_i]; let t2 = d + 0.5 * r2;
                    f.ap2[f.ap2_i] = t2; d = r2 - 0.5 * t2;
                    f.ap2_i += 1; if f.ap2_i >= l2 { f.ap2_i = 0; }
                    // FDN-4, Hadamard scatter, per-line damping
                    let mut ln = [0usize; 4];
                    for k in 0..4 {
                        ln[k] = (f.vl[k] as usize).max(32);
                        if f.vi[k] >= ln[k] { f.vi[k] = 0; }
                        let rk = f.vb[k][f.vi[k]];
                        f.vlp[k] += v_damp * (rk - f.vlp[k]);
                    }
                    let m = [
                        (f.vlp[0] + f.vlp[1] + f.vlp[2] + f.vlp[3]) * 0.5,
                        (f.vlp[0] - f.vlp[1] + f.vlp[2] - f.vlp[3]) * 0.5,
                        (f.vlp[0] + f.vlp[1] - f.vlp[2] - f.vlp[3]) * 0.5,
                        (f.vlp[0] - f.vlp[1] - f.vlp[2] + f.vlp[3]) * 0.5,
                    ];
                    for k in 0..4 {
                        f.vb[k][f.vi[k]] = d + vg * m[k];
                        f.vi[k] += 1;
                    }
                    wet = (f.vlp[0] + f.vlp[1] + f.vlp[2] + f.vlp[3]) * 0.35;
                }
                x = x + d_read * y_mix + wet * v_mix;
            }
            out[s] = x;
        }
    }

    #[inline(always)]
    fn read_table(&self, fi: usize, ff: f32, ph: f32) -> f32 {
        let x = ph * FRAME as f32;
        let xi = x as usize & (FRAME - 1);
        let xf = x - x.floor();
        let x2 = (xi + 1) & (FRAME - 1);
        let t = &self.table;
        let a = t[fi * FRAME + xi] + (t[fi * FRAME + x2] - t[fi * FRAME + xi]) * xf;
        let b = t[(fi + 1) * FRAME + xi] + (t[(fi + 1) * FRAME + x2] - t[(fi + 1) * FRAME + xi]) * xf;
        a + (b - a) * ff
    }

    #[inline(always)]
    fn rand(&mut self) -> f32 {                      // xorshift32 → [0,1)
        let mut x = self.rng;
        x ^= x << 13; x ^= x >> 17; x ^= x << 5;
        self.rng = x;
        (x as f32) * (1.0 / 4_294_967_296.0)
    }

    fn factory_table(&mut self) {
        // 6 frames: sine, saw, square, metallic-comb, inharmonic, FM-vox
        self.n_frames = 6;
        for f in 0..6 {
            for i in 0..FRAME {
                let ph = i as f32 / FRAME as f32;
                let v = match f {
                    0 => (TAU * ph).sin(),
                    1 => { let mut a = 0.0; for k in 1..=32 { a += (TAU * ph * k as f32).sin() / k as f32; } a * 0.55 }
                    2 => { let mut a = 0.0; let mut k = 1; while k <= 31 { a += (TAU * ph * k as f32).sin() / k as f32; k += 2; } a * 0.7 }
                    3 => { let mut a = 0.0; for k in 1..=24 { a += (TAU * ph * k as f32).sin() * (k as f32 * 0.9).sin() / k as f32; } a * 0.8 }
                    4 => { let p = [1.0, 2.76, 5.4, 8.93, 13.3]; let mut a = 0.0;
                           for (k, r) in p.iter().enumerate() { a += (TAU * ph * r).sin() / (k as f32 + 1.0); } a * 0.5 }
                    _ => { let fr = [3.0, 5.0, 8.0, 12.0]; let am = [1.0, 0.7, 0.45, 0.3]; let mut a = 0.0;
                           for k in 0..4 { a += am[k] * (TAU * ph * fr[k] + (TAU * ph).sin() * 2.0).sin(); } a * 0.4 }
                };
                self.table[f * FRAME + i] = v;
            }
        }
    }
}

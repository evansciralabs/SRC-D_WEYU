/* riot_worklet.js — hosts the Rust core (dsp_core.rs → wasm-pack → pkg/)
   inside the AudioWorklet. Registered under the same name and message
   protocol as the inline JS reference core in index.html, so the UI is
   engine-agnostic:  ['p',i,v] ['n',note,vel] ['f',note] ['r',routes]
                     ['t',Float32Array,frames] ['panic'] ['rec',0|1]
                     ['hold',samples,note] ['seq',state]
                     →  ['e',name] ['recdata',chunks,rate] ['step',i]      */

import "./worklet_polyfill.js";              /* MUST precede the glue     */
import init, { RiotCore } from "./pkg/dsp_core.js";

const NP = 67;   /* ⚠ index-locked to PDEF in dsp_core.rs and PARAMS in the HTML shell */
const FRAME = 2048;

class RiotProcessor extends AudioWorkletProcessor {
  constructor(options) {
    super();
    const o = options.processorOptions || {};
    this.core = null;
    this.wasm = null;
    this.pView = null;                       /* view into wasm param block */
    this.outView = null;                     /* view into internal output buffer */
    this.sabView = o.sab ? new Float32Array(o.sab) : null;
    this.init = o.init || null;              /* param snapshot at boot     */
    this.pending = [];                       /* messages before wasm ready */
    this.rec = null; this.recN = 0;          /* ⏺ capture buffer           */
    this.holdCnt = 0; this.holdNote = 0;     /* bounce gate                */
    this.seq = null;                         /* ▦ sequencer clock state    */
    this.seqStep = 15; this.seqNext = 0;
    this.seqGate = 0; this.seqNote = -1; this.seqCount = 0;

    init(o.wasmBytes).then((wasm) => {
      this.wasm = wasm;
      this.core = new RiotCore(sampleRate);
      this.refreshView();
      if (this.init) this.pView.set(this.init);
      if (this.sabView) this.pView.set(this.sabView);
      const q = this.pending; this.pending = [];
      for (const m of q) this.handle(m);
      this.port.postMessage(["e", "RUST/WASM"]);
    }).catch(() => this.port.postMessage(["e", "WASM INIT FAILED"]));

    this.port.onmessage = (e) => {
      if (!this.core) { this.pending.push(e.data); return; }
      this.handle(e.data);
    };
  }

  /* wasm memory can grow (e.g. on table upload) — views detach */
  refreshView() {
    this.pView = new Float32Array(this.wasm.memory.buffer, this.core.params_ptr(), NP);
    this.outView = new Float32Array(this.wasm.memory.buffer, this.core.out_ptr(), 128);
  }

  handle(m) {
    switch (m[0]) {
      case "p": if (!this.sabView) this.pView[m[1]] = m[2]; break;
      case "n": this.core.note_on(m[1], m[2]); break;
      case "f": this.core.note_off(m[1]); break;
      case "r":
        m[1].forEach((r, k) => this.core.set_route(k, r.i, r.d, r.s || 0));
        this.core.set_route_count(m[1].length);
        break;
      case "t": {                            /* sample → wavetable upload  */
        const data = m[1], nf = m[2];
        const ptr = this.core.table_ptr();   /* may grow memory: view after */
        /* clamp to the core's buffer — writing data.length floats blind
           would stomp wasm memory if the upload outgrew MAX_FRAMES */
        const cap = this.core.table_capacity ? this.core.table_capacity() : data.length;
        const len = Math.min(data.length, cap);
        const nfW = Math.min(nf, (len / FRAME) | 0);
        if (nfW >= 2) {
          new Float32Array(this.wasm.memory.buffer, ptr, len).set(data.subarray(0, len));
          this.core.set_table_frames(nfW);
        }
        this.refreshView();                  /* Detached view fix: force update immediately post-allocation */
        break;
      }
      case "panic": this.seqNote = -1; this.seqGate = 0; this.core.panic(); break;
      case "rec":                            /* ⏺ live capture on/off       */
        if (m[1]) { this.rec = []; this.recN = 0; }
        else if (this.rec) { const c = this.rec; this.rec = null;
          this.port.postMessage(["recdata", c, sampleRate], c.map(a => a.buffer)); }
        break;
      case "hold": this.holdCnt = m[1]; this.holdNote = m[2]; break;
      case "seq": {                          /* ▦ pattern/transport state   */
        const was = this.seq && this.seq.on;
        this.seq = m[1];
        if (was && !this.seq.on && this.seqNote >= 0) {
          this.core.note_off(this.seqNote); this.seqNote = -1;
        }
        if (!was && this.seq.on) {
          this.seqStep = 15; this.seqNext = 0; this.seqGate = 0; this.seqCount = 0;
        }
        break;
      }
    }
  }

  recPush(out) {
    this.rec.push(new Float32Array(out));
    this.recN += out.length;
    if (this.recN >= sampleRate * 600) {     /* 10-min cap → auto-flush     */
      const c = this.rec; this.rec = null;
      this.port.postMessage(["recdata", c, sampleRate], c.map(a => a.buffer));
    }
  }

  process(inputs, outputs) {
    if (!this.core) return true;             /* silent until wasm is up    */
    if (this.pView.buffer.byteLength === 0) this.refreshView();
    if (this.sabView) this.pView.set(this.sabView);   /* zero-latency sync */

    const out = outputs[0];
    if (out && out[0]) {
      const n = out[0].length;
      /* bounce gate — sample-budget noteOff (parity with inline worklet)   */
      if (this.holdCnt > 0) { this.holdCnt -= n;
        if (this.holdCnt <= 0) this.core.note_off(this.holdNote); }
      /* ▦ sequencer clock — identical math to the inline worklet          */
      if (this.seqGate > 0) { this.seqGate -= n;
        if (this.seqGate <= 0 && this.seqNote >= 0) {
          this.core.note_off(this.seqNote); this.seqNote = -1; } }
      if (this.seq && this.seq.on) {
        this.seqNext -= n;
        while (this.seqNext <= 0) {
          if (this.seq.stopAfter && this.seqCount >= this.seq.stopAfter) {
            this.seq.on = false; break; }
          const sl = this.seq.len || 16;   /* pattern length 1‥16 */
          this.seqStep++; if (this.seqStep >= sl) this.seqStep = 0;
          this.seqCount++;
          const base = sampleRate * 15 / this.seq.bpm;
          const sw = this.seq.swing * 0.6;
          const dur = base * ((this.seqStep & 1) === 0 ? 1 + sw : 1 - sw);
          this.seqNext += dur;
          const st = this.seq.steps[this.seqStep];
          if (st && st.s) {
            if (this.seqNote >= 0) this.core.note_off(this.seqNote);
            this.core.note_on(st.note, st.s === 2 ? 1.0 : 0.6);
            this.seqNote = st.note;
            this.seqGate = Math.max(64, dur * 0.45);
          }
          this.port.postMessage(["step", this.seqStep]);
        }
      }
      
      this.core.process();
      out[0].set(this.outView);
      
      for (let c = 1; c < out.length; c++) out[c].set(out[0]);
      if (this.rec) this.recPush(out[0]);
    }
    return true;
  }
}

registerProcessor("riot-core", RiotProcessor);

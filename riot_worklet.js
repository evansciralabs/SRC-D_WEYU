/* riot_worklet.js — hosts the Rust core (dsp_core.rs → wasm-pack → pkg/)
   inside the AudioWorklet. Registered under the same name and message
   protocol as the inline JS reference core in index.html, so the UI is
   engine-agnostic:  ['p',i,v] ['n',note,vel] ['f',note] ['r',routes]
                     ['t',Float32Array,frames] ['panic']  →  ['e',name]  */

import "./worklet_polyfill.js";              /* MUST precede the glue     */
import init, { RiotCore } from "./pkg/dsp_core.js";

const NP = 63;   /* ⚠ Updated to 63 to lockstep with PDEF in dsp_core.rs and PARAMS in the HTML shell */
const FRAME = 2048;

class RiotProcessor extends AudioWorkletProcessor {
  constructor(options) {
    super();
    const o = options.processorOptions || {};
    this.core = null;
    this.wasm = null;
    this.pView = null;                       /* view into wasm param block */
    this.sabView = o.sab ? new Float32Array(o.sab) : null;
    this.init = o.init || null;              /* param snapshot at boot     */
    this.pending = [];                       /* messages before wasm ready */

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
      case "panic": this.core.panic(); break;
    }
  }

  process(inputs, outputs) {
    if (!this.core) return true;             /* silent until wasm is up    */
    if (this.pView.buffer.byteLength === 0) this.refreshView();
    if (this.sabView) this.pView.set(this.sabView);   /* zero-latency sync */

    const out = outputs[0];
    if (out && out[0]) {
      this.core.process(out[0]);
      for (let c = 1; c < out.length; c++) out[c].set(out[0]);
    }
    return true;
  }
}

registerProcessor("riot-core", RiotProcessor);

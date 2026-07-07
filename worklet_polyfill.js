/* worklet_polyfill.js — AudioWorkletGlobalScope lacks TextDecoder/TextEncoder,
   but the wasm-bindgen glue (pkg/dsp_core.js) references them at module
   evaluation time. riot_worklet.js imports this module FIRST, so these
   minimal shims exist before the glue evaluates. (Static imports
   evaluate dependencies in listed order; dynamic import() is unavailable
   in worklet scopes, which is why the polyfill must be its own module.)  */
if (typeof globalThis.TextDecoder === "undefined") {
  globalThis.TextDecoder = class {
    constructor(){ }
    decode(buf){
      if (!buf) return "";
      const b = buf instanceof Uint8Array ? buf
              : new Uint8Array(buf.buffer || buf, buf.byteOffset || 0, buf.byteLength);
      try {
        return decodeURIComponent(escape(String.fromCharCode.apply(null, b)));
      } catch(_) {
        let s = "";
        for (let i = 0; i < b.length; i++) s += String.fromCharCode(b[i] & 0x7f);
        return s;
      }
    }
  };
}
if (typeof globalThis.TextEncoder === "undefined") {
  globalThis.TextEncoder = class {
    encode(s){
      const encoded = unescape(encodeURIComponent(s));
      const b = new Uint8Array(encoded.length);
      for (let i = 0; i < encoded.length; i++) b[i] = encoded.charCodeAt(i);
      return b;
    }
    encodeInto(s, b){
      const encoded = unescape(encodeURIComponent(s));
      let i = 0;
      for (; i < encoded.length && i < b.length; i++) b[i] = encoded.charCodeAt(i);
      return { read: i, written: i };
    }
  };
}

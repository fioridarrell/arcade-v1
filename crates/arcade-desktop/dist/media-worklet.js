/* Audio worklets for app windows' calls (media.js). A file of its own: the window's policy runs
   scripts from this app only, so a worklet cannot come from a blob.

   arcade-cap: the microphone's samples, in blocks, to the window (it encodes them).
   arcade-play: one player's decoded sound, played from a small buffer: it starts once JITTER
   samples are in, plays silence when it runs dry, and lets go of the oldest when more than
   MOST are waiting (a voice should be late by tens of milliseconds, never seconds). */
class Cap extends AudioWorkletProcessor {
  process(inputs, outputs) {
    const ch = inputs[0] && inputs[0][0];
    if (ch && ch.length) this.port.postMessage(ch.slice(0));
    const out = outputs[0] && outputs[0][0];
    if (out) out.fill(0);
    return true;
  }
}
registerProcessor("arcade-cap", Cap);

class Play extends AudioWorkletProcessor {
  constructor() {
    super();
    this.q = [];          // Float32Arrays waiting
    this.head = 0;        // read position in q[0]
    this.queued = 0;      // samples waiting
    this.playing = false;
    this.JITTER = sampleRate * 0.06;
    this.MOST = sampleRate * 0.3;
    this.port.onmessage = (e) => {
      const b = e.data;
      if (!(b instanceof Float32Array) || !b.length) return;
      this.q.push(b);
      this.queued += b.length;
      while (this.queued > this.MOST && this.q.length > 1) {
        const old = this.q.shift();
        this.queued -= old.length - this.head;
        this.head = 0;
      }
    };
  }
  process(inputs, outputs) {
    const out = outputs[0][0];
    if (!this.playing && this.queued >= this.JITTER) this.playing = true;
    let i = 0;
    while (this.playing && i < out.length && this.q.length) {
      const b = this.q[0], n = Math.min(out.length - i, b.length - this.head);
      out.set(b.subarray(this.head, this.head + n), i);
      i += n; this.head += n; this.queued -= n;
      if (this.head >= b.length) { this.q.shift(); this.head = 0; }
    }
    if (i < out.length) { out.fill(0, i); if (!this.q.length) this.playing = false; }
    for (let c = 1; c < outputs[0].length; c++) outputs[0][c].set(out);
    return true;
  }
}
registerProcessor("arcade-play", Play);

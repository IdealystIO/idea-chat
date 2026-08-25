// The processors that run on the audio rendering thread.
//
// Loaded as a Blob URL rather than a file, so the crate ships a string and an
// app has no asset to serve. Deliberately small and dependency-free: everything
// in here runs on a real-time thread with a 2.67 ms deadline, and the rule is
// that `process` never allocates.
//
// The render quantum is fixed at 128 frames by the spec, on every browser,
// forever. Blocks larger than that are this file's business, not the graph's.

class CaptureProcessor extends AudioWorkletProcessor {
  constructor(options) {
    super();
    const o = (options && options.processorOptions) || {};
    this.channels = Math.max(1, (o.channels | 0) || 1);
    this.blockFrames = Math.max(128, (o.blockFrames | 0) || 256);
    // One buffer for the life of the stream. See `process`.
    this.buf = new Float32Array(this.blockFrames * this.channels);
    this.filled = 0;
  }

  process(inputs) {
    const input = inputs[0];
    // No input connected yet, or the track ended. Staying alive rather than
    // returning false: a capture node that retires cannot be restarted, and a
    // track can go silent for a moment without going away.
    if (!input || input.length === 0 || !input[0]) return true;

    const frames = input[0].length;
    for (let i = 0; i < frames; i++) {
      const base = this.filled * this.channels;
      for (let c = 0; c < this.channels; c++) {
        const ch = input[c] || input[0];
        this.buf[base + c] = ch[i];
      }
      if (++this.filled === this.blockFrames) {
        // A structured clone, NOT a transfer. Transferring would detach the
        // buffer and force a fresh allocation every block — on the audio
        // thread, where an allocation is how you get a dropout that only
        // happens under GC pressure. Copying costs a memcpy of a few hundred
        // floats and lets one buffer last the whole call.
        this.port.postMessage(this.buf);
        this.filled = 0;
      }
    }
    return true;
  }
}

class OutputProcessor extends AudioWorkletProcessor {
  constructor(options) {
    super();
    const o = (options && options.processorOptions) || {};
    this.channels = Math.max(1, (o.channels | 0) || 1);
    // Frames to accumulate before the first sample is played. The cushion only
    // has to cover jitter in the PRODUCER now — the render callback itself
    // cannot be late any more, because it is on this thread.
    this.primeFrames = Math.max(0, o.primeFrames | 0);
    this.queue = [];
    this.head = 0; // sample offset into queue[0]
    this.queued = 0; // frames available
    this.primed = false;
    this.underruns = 0;
    this.sinceReport = 0;
    this.port.onmessage = (e) => {
      const pcm = e.data;
      if (!pcm || pcm.length === undefined) return;
      this.queue.push(pcm);
      this.queued += pcm.length / this.channels;
    };
  }

  process(_inputs, outputs) {
    const out = outputs[0];
    if (!out || out.length === 0) return true;
    const frames = out[0].length;

    if (!this.primed && this.queued >= this.primeFrames) this.primed = true;

    for (let i = 0; i < frames; i++) {
      if (!this.primed || this.queue.length === 0) {
        // Running dry after playing is an underrun; drop back to filling so the
        // cushion is rebuilt rather than stuttering sample by sample.
        if (this.primed) {
          this.underruns++;
          this.primed = false;
        }
        for (let c = 0; c < out.length; c++) out[c][i] = 0;
        continue;
      }
      const chunk = this.queue[0];
      for (let c = 0; c < out.length; c++) {
        out[c][i] = chunk[this.head + (c % this.channels)];
      }
      this.head += this.channels;
      this.queued--;
      if (this.head >= chunk.length) {
        this.queue.shift();
        this.head = 0;
      }
    }

    // Telemetry, ~50 ms apart. The main thread cannot see this queue and an
    // underrun is otherwise indistinguishable from a quiet room — the same
    // reason the transport counts what it could not send.
    this.sinceReport += frames;
    if (this.sinceReport >= 2400) {
      this.sinceReport = 0;
      this.port.postMessage({ queued: this.queued, underruns: this.underruns });
    }
    return true;
  }
}

registerProcessor('audio-io-capture', CaptureProcessor);
registerProcessor('audio-io-output', OutputProcessor);

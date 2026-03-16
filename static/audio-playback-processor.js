/**
 * AudioWorklet processor for voice playback.
 *
 * Receives PCM Float32 chunks at the model's output sample rate and
 * resamples to the AudioContext's native rate using linear interpolation.
 * Outputs a continuous sample stream — no buffer boundaries, no clicks.
 *
 * Main thread controls:
 *   port.postMessage({ type: 'audio', data: Float32Array, rate: number })
 *   port.postMessage({ type: 'clear' })  — flush buffer (e.g. on disconnect)
 */
class AudioPlaybackProcessor extends AudioWorkletProcessor {
  constructor() {
    super();
    // Ring buffer: 2 seconds at native sampleRate (generous for jitter)
    this.ringBuf = new Float32Array(sampleRate * 2);
    this.writePos = 0;
    this.readPos = 0;
    this.srcRate = 24000; // default, updated per chunk
    // Fractional source position for resampling continuity across process() calls
    this.srcFrac = 0;
    this.port.onmessage = (e) => {
      if (e.data.type === 'clear') {
        this.writePos = 0;
        this.readPos = 0;
        this.srcFrac = 0;
        return;
      }
      if (e.data.type === 'audio') {
        this.srcRate = e.data.rate || 24000;
        this._enqueue(e.data.data);
      }
    };
  }

  _enqueue(float32) {
    // Resample from srcRate to sampleRate and write into ring buffer
    const ratio = this.srcRate / sampleRate;
    const outLen = Math.floor(float32.length / ratio);
    const cap = this.ringBuf.length;
    let wp = this.writePos;
    for (let i = 0; i < outLen; i++) {
      const srcIdx = i * ratio;
      const lo = Math.floor(srcIdx);
      const hi = Math.min(lo + 1, float32.length - 1);
      const frac = srcIdx - lo;
      this.ringBuf[wp % cap] = float32[lo] * (1 - frac) + float32[hi] * frac;
      wp++;
    }
    this.writePos = wp;
  }

  process(inputs, outputs) {
    const output = outputs[0];
    if (!output || !output[0]) return true;
    const channel = output[0];
    const cap = this.ringBuf.length;
    const available = this.writePos - this.readPos;
    const toRead = Math.min(channel.length, available);
    let rp = this.readPos;
    for (let i = 0; i < toRead; i++) {
      channel[i] = this.ringBuf[rp % cap];
      rp++;
    }
    // Fill remainder with silence (underrun)
    for (let i = toRead; i < channel.length; i++) {
      channel[i] = 0;
    }
    this.readPos = rp;
    return true;
  }
}

registerProcessor('audio-playback-processor', AudioPlaybackProcessor);

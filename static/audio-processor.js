/**
 * AudioWorklet processor for microphone capture.
 *
 * Accumulates 128-sample WebAudio render quanta into larger chunks
 * (default 4096 samples) before posting to the main thread, reducing
 * message overhead while maintaining low latency.
 *
 * Main thread controls:
 *   port.postMessage({ type: 'mute' })   — pause capture
 *   port.postMessage({ type: 'unmute' }) — resume capture
 *
 * Outbound messages:
 *   { type: 'audio', data: Float32Array }
 */
class AudioCaptureProcessor extends AudioWorkletProcessor {
  constructor(options) {
    super();
    this.bufferSize =
      (options.processorOptions && options.processorOptions.bufferSize) || 4096;
    this.buffer = new Float32Array(this.bufferSize);
    this.writeIndex = 0;
    this.active = true;
    this.port.onmessage = (e) => {
      if (e.data.type === 'mute') this.active = false;
      if (e.data.type === 'unmute') this.active = true;
    };
  }

  process(inputs) {
    const input = inputs[0];
    if (!input || !input[0] || !this.active) return true;
    const channelData = input[0];
    for (let i = 0; i < channelData.length; i++) {
      this.buffer[this.writeIndex++] = channelData[i];
      if (this.writeIndex >= this.bufferSize) {
        this.port.postMessage({ type: 'audio', data: this.buffer.slice() });
        this.writeIndex = 0;
      }
    }
    return true;
  }
}

registerProcessor('audio-capture-processor', AudioCaptureProcessor);

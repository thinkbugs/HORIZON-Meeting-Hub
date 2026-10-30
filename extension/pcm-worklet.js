// Interleaves the captured stereo signal into 20 ms s16le blocks. The receiver
// opens waveOut with exactly this rate and channel count, so the block size is
// the only clock the two sides share: no timestamps, no resampling on arrival.
const FRAMES = 960; // 20 ms at 48 kHz
const CHANNELS = 2;

class PcmCapture extends AudioWorkletProcessor {
  constructor() {
    super();
    this.chunk = new ArrayBuffer(FRAMES * CHANNELS * 2);
    this.view = new DataView(this.chunk);
    this.at = 0;
  }

  process(inputs) {
    const input = inputs[0];
    const left = input && input[0];
    if (!left) return true;
    const right = input.length > 1 ? input[1] : left;
    for (let index = 0; index < left.length; index += 1) {
      this.view.setInt16(this.at * 4, toInt16(left[index]), true);
      this.view.setInt16(this.at * 4 + 2, toInt16(right[index]), true);
      this.at += 1;
      if (this.at === FRAMES) {
        const ready = this.chunk;
        this.chunk = new ArrayBuffer(FRAMES * CHANNELS * 2);
        this.view = new DataView(this.chunk);
        this.at = 0;
        this.port.postMessage(ready, [ready]);
      }
    }
    return true;
  }
}

function toInt16(sample) {
  const clamped = sample < -1 ? -1 : sample > 1 ? 1 : sample;
  return clamped < 0 ? Math.round(clamped * 0x8000) : Math.round(clamped * 0x7fff);
}

registerProcessor('pcm-capture', PcmCapture);

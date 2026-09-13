// 16kHz/16bit/单声道 PCM 采集 worklet
// 把设备采样率线性重采样到 16000，累积到 160ms(2560 样本)再下推 Int16
class PcmProcessor extends AudioWorkletProcessor {
  constructor() {
    super();
    this.ratio = 16000 / sampleRate; // 输出/输入 样本比
    this.frac = 0;
    this.out = []; // 重采样后的 float 样本
    this.FRAME = 2560; // 160ms @ 16k
    this.port.onmessage = (e) => {
      if (e.data && e.data.type === "stop") this.flush();
    };
  }

  process(inputs) {
    const input = inputs[0];
    if (!input || !input[0]) return true;
    const ch = input[0];
    // 线性重采样
    let pos = this.frac;
    while (pos < ch.length) {
      const idx = Math.floor(pos);
      const next = Math.min(idx + 1, ch.length - 1);
      const t = pos - idx;
      const v = ch[idx] + (ch[next] - ch[idx]) * t;
      this.out.push(v);
      pos += 1 / this.ratio;
    }
    this.frac = pos - ch.length;
    // 每 FRAME 样本下推一次
    while (this.out.length >= this.FRAME) {
      this.send(this.out.splice(0, this.FRAME));
    }
    return true;
  }

  flush() {
    if (this.out.length > 0) this.send(this.out.splice(0, this.out.length));
  }

  send(floats) {
    const i16 = new Int16Array(floats.length);
    for (let i = 0; i < floats.length; i++) {
      const s = Math.max(-1, Math.min(1, floats[i]));
      i16[i] = s < 0 ? s * 0x8000 : s * 0x7fff;
    }
    this.port.postMessage(i16.buffer, [i16.buffer]);
  }
}
registerProcessor("pcm-processor", PcmProcessor);

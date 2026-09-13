// 麦克风录音：getUserMedia + AudioWorklet，产出 16kHz/16bit/单声道 PCM。
// stop() 返回完整 PCM 的 ArrayBuffer。
export async function startRecorder() {
  const stream = await navigator.mediaDevices.getUserMedia({
    // 原始麦克风、设备原生采样率（强制 16k 会让 WebView2 给出不匹配的缓冲），
    // worklet 负责重采样到 16k。
    audio: { channelCount: 1, echoCancellation: false, noiseSuppression: false, autoGainControl: false },
  });
  const ctx = new AudioContext();
  await ctx.resume();
  await ctx.audioWorklet.addModule("/pcm-processor.js");
  const src = new MediaStreamAudioSourceNode(ctx, { mediaStream: stream });
  const node = new AudioWorkletNode(ctx, "pcm-processor");
  const mute = ctx.createGain();
  mute.gain.value = 0; // 不回放，避免回声
  const chunks = [];
  node.port.onmessage = (e) => chunks.push(e.data);
  src.connect(node);
  node.connect(mute);
  mute.connect(ctx.destination);

  return {
    stop: () =>
      new Promise((resolve) => {
        node.port.postMessage({ type: "stop" });
        setTimeout(() => {
          try {
            src.disconnect();
            node.disconnect();
            mute.disconnect();
          } catch {}
          stream.getTracks().forEach((t) => t.stop());
          ctx.close();
          const total = chunks.reduce((n, b) => n + b.byteLength, 0);
          const merged = new Uint8Array(total);
          let off = 0;
          for (const b of chunks) {
            merged.set(new Uint8Array(b), off);
            off += b.byteLength;
          }
          resolve(merged.buffer);
        }, 80);
      }),
  };
}

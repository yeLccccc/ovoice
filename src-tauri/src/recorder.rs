// 麦克风录音（cpal / WASAPI）→ 16kHz/16bit/单声道 PCM。
// 供「全局推说话」使用：独立于 WebView 状态，窗口最小化也能录。
// 重采样/单声道/量化与前端 pcm-processor.js 的做法保持一致。
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, Stream, StreamConfig};

/// 目标：百度短语音 ASR 要求 16000Hz。
const OUT_RATE: usize = 16000;

/// 线性重采样器：把任意输入采样率重采样到 OUT_RATE。
/// 用「上一次未消费样本」做 carry，保证跨回调边界也能正确插值。
struct Resampler {
    step: f64,       // 每产出一个输出样本，输入指针前进多少（in/out）
    pos: f64,        // 在逻辑输入流中的读取位置（单位：输入样本）
    carry: Vec<f32>, // 上次遗留、尚未消费的输入样本
}

impl Resampler {
    fn new(in_rate: usize) -> Self {
        Self {
            step: in_rate as f64 / OUT_RATE as f64,
            pos: 0.0,
            carry: Vec::new(),
        }
    }

    /// `input`：单声道 float 样本（[-1,1]）。返回重采样后的 float 样本。
    fn process(&mut self, input: &[f32]) -> Vec<f32> {
        let mut buf = std::mem::take(&mut self.carry);
        buf.extend_from_slice(input);
        if buf.len() < 2 {
            self.carry = buf;
            return Vec::new();
        }
        let mut out = Vec::new();
        // 需要 pos 与 pos+1 两个样本做线性插值，故停在 len-1 之前。
        while self.pos + 1.0 < buf.len() as f64 {
            let i = self.pos.floor() as usize;
            let frac = self.pos - i as f64;
            let a = buf[i];
            let b = buf[i + 1];
            out.push(a + (b - a) * (frac as f32));
            self.pos += self.step;
        }
        let consumed = (self.pos.floor() as usize).min(buf.len());
        self.carry = buf[consumed..].to_vec();
        self.pos -= consumed as f64;
        out
    }
}

/// 把交错的多通道 float 数据下混为单声道。
fn to_mono(data: &[f32], channels: usize) -> Vec<f32> {
    if channels <= 1 {
        return data.to_vec();
    }
    let frames = data.len() / channels;
    let mut out = Vec::with_capacity(frames);
    for f in 0..frames {
        let start = f * channels;
        let sum: f32 = data[start..start + channels].iter().copied().sum();
        out.push(sum / channels as f32);
    }
    out
}

#[inline]
fn f32_to_i16le(s: f32) -> i16 {
    (s.clamp(-1.0, 1.0) * 32767.0).round() as i16
}

fn err_cb(err: cpal::StreamError) {
    eprintln!("[cpal] 录音流错误: {err}");
}

/// 构造一个「下混 → 线性重采样到 16k → i16 LE 入缓冲」的回调。
/// Resampler 状态被闭包拥有、跨回调保留，保证重采样连续。
fn make_callback(
    channels: usize,
    in_rate: usize,
    samples: Arc<Mutex<Vec<u8>>>,
) -> impl FnMut(&[f32], &cpal::InputCallbackInfo) + Send + 'static {
    let mut resampler = Resampler::new(in_rate);
    move |data: &[f32], _| {
        let mono = to_mono(data, channels);
        let resampled = resampler.process(&mono);
        if let Ok(mut g) = samples.lock() {
            for s in resampled {
                g.extend_from_slice(&f32_to_i16le(s).to_le_bytes());
            }
        }
    }
}

/// 麦克风录音器：start() 开始采集并清空缓冲；stop() 停止并返回完整 PCM。
pub struct Recorder {
    samples: Arc<Mutex<Vec<u8>>>,
    stream: Option<Stream>,
}

impl Recorder {
    pub fn new() -> Self {
        Self {
            samples: Arc::new(Mutex::new(Vec::new())),
            stream: None,
        }
    }

    pub fn start(&mut self) -> Result<(), String> {
        // 先停掉可能残留的流。
        self.stream.take();

        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .ok_or_else(|| "找不到麦克风设备".to_string())?;
        let supported = device
            .default_input_config()
            .map_err(|e| format!("无法读取麦克风配置: {e}"))?;
        let sample_format = supported.sample_format();
        let cfg: StreamConfig = supported.into();
        let in_rate = cfg.sample_rate.0 as usize;
        let channels = cfg.channels as usize;
        let samples = self.samples.clone();

        samples.lock().unwrap().clear();

        let stream = match sample_format {
            SampleFormat::F32 => device
                .build_input_stream(&cfg, make_callback(channels, in_rate, samples), err_cb, None),
            SampleFormat::I16 => {
                let mut cb = make_callback(channels, in_rate, samples);
                device.build_input_stream(
                    &cfg,
                    move |data: &[i16], info| {
                        let f: Vec<f32> = data.iter().map(|&s| s as f32 / 32768.0).collect();
                        cb(&f, info);
                    },
                    err_cb,
                    None,
                )
            }
            SampleFormat::U16 => {
                let mut cb = make_callback(channels, in_rate, samples);
                device.build_input_stream(
                    &cfg,
                    move |data: &[u16], info| {
                        let f: Vec<f32> = data.iter().map(|&s| (s as f32 - 32768.0) / 32768.0).collect();
                        cb(&f, info);
                    },
                    err_cb,
                    None,
                )
            }
            other => return Err(format!("不支持的采样格式: {other:?}")),
        }
        .map_err(|e| format!("无法打开麦克风: {e}"))?;

        stream.play().map_err(|e| format!("无法启动录音: {e}"))?;
        self.stream = Some(stream);
        Ok(())
    }

    /// 停止采集并返回累积的 PCM 字节（16k/16bit/单声道，小端）。
    pub fn stop(&mut self) -> Vec<u8> {
        // 丢弃 Stream 即停止采集。
        self.stream.take();
        std::mem::take(&mut *self.samples.lock().unwrap())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resampler_downsamples_48k_to_16k_ratio() {
        // 4800 个输入样本（0.1s @48k）应产出 ~1600 个输出样本（0.1s @16k）。
        let mut r = Resampler::new(48000);
        let input: Vec<f32> = (0..4800).map(|i| (i as f32 / 100.0).sin()).collect();
        // 分两段喂入，验证 carry 边界连续。
        let mut out = r.process(&input[..2400]);
        out.extend(r.process(&input[2400..]));
        assert!(out.len() >= 1590 && out.len() <= 1610, "len={}", out.len());
    }

    #[test]
    fn mono_downmix_averages_channels() {
        // 立体声 [L,R]=[[0.0,1.0],[2.0,2.0]] → 单声道 [0.5, 2.0]
        let m = to_mono(&[0.0, 1.0, 2.0, 2.0], 2);
        assert_eq!(m, vec![0.5, 2.0]);
    }

    #[test]
    fn f32_to_i16_clamps_and_scales() {
        assert_eq!(f32_to_i16le(1.0), 32767);
        assert_eq!(f32_to_i16le(-1.0), -32767);
        assert_eq!(f32_to_i16le(0.0), 0);
        assert_eq!(f32_to_i16le(5.0), 32767); // 超过 1.0 钳到上限
    }
}

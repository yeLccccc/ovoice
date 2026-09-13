// 全局推说话：双击 Ctrl 触发录音，结束→立即 ASR。两种手势由 config.voice_trigger 选择：
//
//   hold（默认，防误触）：
//   1) 点一下 Ctrl（按下→松开）—— 记为第 1 次。
//   2) 350ms 内再按下 Ctrl —— 命中双击，进入「预热」态。
//   3) 持续按住 > hold_gate_ms —— 真正开始录音（之前是沉默的预热窗口）。
//      若在预热期内松开 —— 视为普通双击，取消，不录音。
//   4) 松开 Ctrl —— 结束录音，立即送去百度 ASR。
//
//   toggle：
//   1) 350ms 内连按两次 Ctrl —— 命中双击，立即开始录音（无预热）。
//   2) 录音中再按一次 Ctrl（按下沿）—— 结束录音。松开不停止（与 hold 相反）。
//
// 两种模式都：录音结束后把 PCM 经 mpsc 交给 tokio 任务做异步 ASR；
// ASR 完成 → 恢复/聚焦窗口 → emit "voice-result"，前端按 config.voice_action
// 决定立即发送（send）还是只填入输入框（insert）。
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::mpsc;

use crate::asr::AsrState;
use crate::input;
use crate::recorder::Recorder;

/// 两次按下之间判定为「双击」的最大间隔。
const DOUBLE_TAP: Duration = Duration::from_millis(350);
/// 命中双击后，需持续按住多久才真正开始录音（防误触）——可配置（config.hold_gate_ms）。
/// 读取自配置文件，范围钳到 [50, 10000] 毫秒。
const HOLD_GATE_MIN_MS: u64 = 50;
const HOLD_GATE_MAX_MS: u64 = 10_000;
/// 轮询间隔（Windows 计时器粒度约 15ms，实际会比 10ms 略大，对手势足够）。
const POLL: Duration = Duration::from_millis(10);

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct VoiceState {
    phase: &'static str, // arming | recording | recognizing | idle
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct VoiceResult {
    text: String,
}

/// 录音结束后送给 ASR 任务的请求。
struct RecognizeRequest {
    bytes: Vec<u8>,
}

/// 启动全局推说话：轮询线程 + ASR 任务。在 Tauri setup 里调用一次。
pub fn spawn(app: AppHandle, asr: AsrState) {
    let (tx, mut rx) = mpsc::channel::<RecognizeRequest>(8);

    // tokio 任务：异步识别 → 恢复窗口 → 推送结果。
    let app_task = app.clone();
    let asr_task = async move {
        while let Some(req) = rx.recv().await {
            match asr.recognize(&app_task, req.bytes).await {
                Ok(text) => {
                    // 先把窗口拉回前台，确保前端能即时收到并渲染。
                    if let Some(w) = app_task.get_webview_window("main") {
                        let _ = w.unminimize();
                        let _ = w.show();
                        let _ = w.set_focus();
                    }
                    let _ = app_task.emit("voice-result", VoiceResult { text });
                }
                Err(e) => {
                    let _ = app_task.emit("voice-error", e);
                    let _ = app_task.emit("voice-state", VoiceState { phase: "idle" });
                }
            }
        }
    };
    tauri::async_runtime::spawn(asr_task);

    // std 轮询线程：独占 Recorder，跑手势状态机。
    std::thread::Builder::new()
        .name("ovoice-voice".into())
        .spawn(move || run_loop(app, tx, Recorder::new()))
        .expect("spawn ovoice-voice thread");
}

fn run_loop(app: AppHandle, tx: mpsc::Sender<RecognizeRequest>, mut recorder: Recorder) {
    eprintln!("[voice] 全局推说话已就绪：双击 Ctrl 触发（hold=按住 / toggle=再按一次停，见设置）。");
    let mut prev_down = false;
    let mut last_press: Option<Instant> = None; // 上一次「按下」时刻（待配对的第 1 次）
    let mut armed_since: Option<Instant> = None; // 命中双击后的预热起始时刻
    let mut armed_hold = Duration::from_millis(1000); // 本次预热需持续的时长（来自配置）
    let mut recording = false;

    loop {
        let down = input::ctrl_down();
        let now = Instant::now();

        if down && !prev_down {
            // —— 按下沿 ——
            let cfg = crate::config::load(&app);
            let toggle = cfg.voice_trigger.as_str() == "toggle";
            if recording {
                // toggle 模式：录音中再次按下即停止。
                // （hold 模式按住期间持续 down，不会产生新的按下沿，不会走到这里。）
                if toggle {
                    recording = false;
                    let bytes = recorder.stop();
                    let _ = app.emit("voice-state", VoiceState { phase: "recognizing" });
                    // PCM 交给 ASR 任务；识别结果由该任务负责恢复窗口并下发。
                    let _ = tx.blocking_send(RecognizeRequest { bytes });
                }
            } else if let Some(t) = last_press {
                if now.duration_since(t) <= DOUBLE_TAP {
                    // 命中双击。
                    last_press = None;
                    if toggle {
                        // toggle：立即开录，无预热。
                        match recorder.start() {
                            Ok(()) => {
                                recording = true;
                                let _ = app.emit("voice-state", VoiceState { phase: "recording" });
                            }
                            Err(e) => {
                                eprintln!("[voice] 无法开始录音: {e}");
                                let _ = app.emit("voice-error", e);
                                let _ = app.emit("voice-state", VoiceState { phase: "idle" });
                            }
                        }
                    } else {
                        // hold：进入预热。此刻读一次配置，确定本次需按住的时长。
                        armed_hold = Duration::from_millis(
                            cfg.hold_gate_ms.clamp(HOLD_GATE_MIN_MS, HOLD_GATE_MAX_MS),
                        );
                        armed_since = Some(now);
                        let _ = app.emit("voice-state", VoiceState { phase: "arming" });
                    }
                } else {
                    // 超时，当作新一轮的第 1 次。
                    last_press = Some(now);
                }
            } else {
                last_press = Some(now);
            }
        } else if !down && prev_down {
            // —— 松开沿 ——
            let cfg = crate::config::load(&app);
            let toggle = cfg.voice_trigger.as_str() == "toggle";
            if recording && !toggle {
                // hold 模式：松开结束录音。toggle 模式靠「再按一次」停，松开不停。
                recording = false;
                let bytes = recorder.stop();
                let _ = app.emit("voice-state", VoiceState { phase: "recognizing" });
                // PCM 交给 ASR 任务；识别结果由该任务负责恢复窗口并下发。
                let _ = tx.blocking_send(RecognizeRequest { bytes });
            } else if armed_since.is_some() {
                // 预热期（<hold_gate）松开 → 当作普通双击，取消（仅 hold 模式会进入预热）。
                armed_since = None;
                last_press = None;
                let _ = app.emit("voice-state", VoiceState { phase: "idle" });
            }
            // 其余情况（单次按下后松开）：保留 last_press，给快速再按一次留双击窗口。
        } else if !recording {
            // —— 无沿变化：检查预热是否满 1s ——
            if let Some(since) = armed_since {
                if now.duration_since(since) >= armed_hold {
                    armed_since = None;
                    match recorder.start() {
                        Ok(()) => {
                            recording = true;
                            let _ = app.emit("voice-state", VoiceState { phase: "recording" });
                        }
                        Err(e) => {
                            eprintln!("[voice] 无法开始录音: {e}");
                            let _ = app.emit("voice-error", e);
                            let _ = app.emit("voice-state", VoiceState { phase: "idle" });
                        }
                    }
                }
            }
        }

        prev_down = down;
        std::thread::sleep(POLL);
    }
}

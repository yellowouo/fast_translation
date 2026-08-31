use anyhow::Result;
use std::sync::{Arc, Mutex};
use windows::core::HSTRING;
use windows::Foundation::TypedEventHandler;
use windows::Media::Core::MediaSource;
use windows::Media::Playback::MediaPlayer;
use windows::Media::SpeechSynthesis::{SpeechSynthesizer, VoiceInformation};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

/// Windows Modern WinRT-based TTS Engine (Windows.Media.SpeechSynthesis)
#[derive(Clone)]
pub struct WindowsTts {
    current_player: Arc<Mutex<Option<MediaPlayer>>>,
}

impl WindowsTts {
    pub fn new() -> Self {
        Self {
            current_player: Arc::new(Mutex::new(None)),
        }
    }

    /// 异步朗读文本（采用 Windows 10/11 原生 WinRT 现代语音引擎）
    pub fn speak_async(&self, text: &str) -> Result<()> {
        let text = text.trim().to_string();
        if text.is_empty() {
            return Ok(());
        }

        let player_ref = self.current_player.clone();

        std::thread::spawn(move || {
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            }

            // 1. 如果上一个语音还在播放，先停止
            {
                let mut guard = player_ref.lock().unwrap();
                if let Some(prev_player) = guard.take() {
                    let _ = prev_player.Pause();
                }
            }

            if let Err(e) = Self::play_speech_internal(&text, player_ref) {
                eprintln!("[WinRT TTS] Speech synthesis failed: {e}");
            }
        });

        Ok(())
    }

    fn play_speech_internal(text: &str, player_ref: Arc<Mutex<Option<MediaPlayer>>>) -> Result<()> {
        let synth = SpeechSynthesizer::new()?;

        // 根据文本语言自动匹配最佳的系统现代语音包（中文 / 英文）
        let is_chinese = text.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c));
        if let Ok(voices) = SpeechSynthesizer::AllVoices() {
            let mut matched_voice: Option<VoiceInformation> = None;
            for voice in voices {
                if let Ok(lang) = voice.Language() {
                    let lang_str = lang.to_string();
                    if is_chinese {
                        if lang_str.starts_with("zh") {
                            matched_voice = Some(voice);
                            break;
                        }
                    } else {
                        if lang_str.starts_with("en") {
                            matched_voice = Some(voice);
                            break;
                        }
                    }
                }
            }
            if let Some(voice) = matched_voice {
                let _ = synth.SetVoice(&voice);
            }
        }

        // 生成音频流
        let h_text = HSTRING::from(text);
        let stream = synth.SynthesizeTextToStreamAsync(&h_text)?.get()?;
        let content_type = stream.ContentType()?;

        // 通过 MediaPlayer 播放音频流
        let source = MediaSource::CreateFromStream(&stream, &content_type)?;
        let player = MediaPlayer::new()?;
        player.SetSource(&source)?;

        {
            let mut guard = player_ref.lock().unwrap();
            *guard = Some(player.clone());
        }

        player.Play()?;

        // 等待播放完成，保持流存活
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let token = player.MediaEnded(&TypedEventHandler::new(move |_, _| {
            let _ = done_tx.send(());
            Ok(())
        }))?;

        // 最多等待 30 秒或播放完毕
        let _ = done_rx.recv_timeout(std::time::Duration::from_secs(30));
        let _ = player.RemoveMediaEnded(token);

        Ok(())
    }
}


use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// 百度翻译引擎模式
#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[serde(rename_all = "snake_case")]
pub enum TranslateMode {
    /// 通用文本翻译（基于百度通用翻译 API，毫秒级响应，适合快速查词查句）
    #[default]
    General,
    /// 百度大模型文本翻译（基于文心大模型 API，结合语境意译，句式更自然）
    Llm,
}

impl std::fmt::Display for TranslateMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::General => write!(f, "通用文本翻译"),
            Self::Llm => write!(f, "大模型文本翻译"),
        }
    }
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct BaiduConfig {
    pub appid: String,
    pub secret_key: String,
    #[serde(default = "default_from")]
    pub from: String,
    #[serde(default = "default_to")]
    pub to: String,
    #[serde(default)]
    pub mode: TranslateMode,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct AppConfig {
    #[serde(default)]
    pub baidu: BaiduConfig,
    #[serde(default)]
    pub app: AppSettings,
    #[serde(default)]
    pub hotkey: HotkeyConfig,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct HotkeyConfig {
    #[serde(default = "default_translate_hotkey")]
    pub translate: String,
    #[serde(default = "default_toggle_hotkey")]
    pub toggle: String,
    #[serde(default = "default_hotkeys_enabled")]
    pub enabled: bool,
    #[serde(default = "default_drag_copy_fallback")]
    pub drag_copy_fallback: bool,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct AppSettings {
    #[serde(default = "default_cache_size")]
    pub cache_size: usize,
    #[serde(default = "default_auto_translate")]
    pub auto_translate: bool,
}

fn default_from() -> String { "auto".into() }
fn default_to() -> String { "zh".into() }
fn default_cache_size() -> usize { 512 }
fn default_auto_translate() -> bool { true }
fn default_translate_hotkey() -> String { "Alt+Q".into() }
fn default_toggle_hotkey() -> String { "Alt+W".into() }
fn default_hotkeys_enabled() -> bool { true }
fn default_drag_copy_fallback() -> bool { true }

impl Default for BaiduConfig {
    fn default() -> Self {
        Self {
            appid: String::new(),
            secret_key: String::new(),
            from: default_from(),
            to: default_to(),
            mode: TranslateMode::default(),
        }
    }
}

impl Default for HotkeyConfig {
    fn default() -> Self {
        Self {
            translate: default_translate_hotkey(),
            toggle: default_toggle_hotkey(),
            enabled: default_hotkeys_enabled(),
            drag_copy_fallback: default_drag_copy_fallback(),
        }
    }
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            cache_size: default_cache_size(),
            auto_translate: default_auto_translate(),
        }
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            baidu: BaiduConfig::default(),
            app: AppSettings::default(),
            hotkey: HotkeyConfig::default(),
        }
    }
}

impl AppConfig {
    pub fn load() -> Result<Self> {
        let config_path = Self::config_path();
        println!("[Config] Config path: {}", config_path.display());

        if config_path.exists() {
            let content = std::fs::read_to_string(&config_path)
                .context("Failed to read config file")?;
            let config: AppConfig =
                toml::from_str(&content).context("Failed to parse config file")?;
            println!("[Config] baidu.appid = '{}'", config.baidu.appid);
            Ok(config)
        } else {
            let config = AppConfig::default();
            let content = toml::to_string_pretty(&config)?;
            if let Some(parent) = config_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&config_path, &content)?;
            println!("[Config] Created default config at: {}", config_path.display());
            Ok(config)
        }
    }

    pub fn save(&self) -> Result<()> {
        let config_path = Self::config_path();
        let content = toml::to_string_pretty(self)?;
        if let Some(parent) = config_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&config_path, &content)?;
        println!("[Config] Saved configuration to: {}", config_path.display());
        Ok(())
    }

    pub fn config_path() -> PathBuf {
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                let p = dir.join("config.toml");
                if p.exists() {
                    return p;
                }
            }
        }
        PathBuf::from("config.toml")
    }
}

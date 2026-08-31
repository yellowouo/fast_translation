use anyhow::{Context, Result};
use lru::LruCache;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::num::NonZeroUsize;

use crate::config::{BaiduConfig, TranslateMode};

/// 百度翻译客户端（支持“通用文本翻译”与“大模型文本翻译”双引擎，并集成 LRU 高速缓存）
pub struct BaiduTranslator {
    config: BaiduConfig,
    client: Client,
    /// 缓存 Key 由 (翻译模式, 文本) 组成，避免切换模式后误读旧缓存
    cache: LruCache<(TranslateMode, String), String>,
}

/// 百度通用翻译响应结构体
#[derive(Deserialize)]
struct BaiduResponse {
    #[serde(rename = "from")]
    _from: Option<String>,
    #[serde(rename = "to")]
    _to: Option<String>,
    #[serde(rename = "trans_result")]
    trans_result: Option<Vec<TransResult>>,
    #[serde(rename = "error_code")]
    error_code: Option<serde_json::Value>,
    #[serde(rename = "error_msg")]
    error_msg: Option<String>,
}

/// 百度大模型文本翻译 (aiTextTranslate) 请求体
#[derive(Serialize)]
struct BaiduLlmRequest<'a> {
    q: &'a str,
    from: &'a str,
    to: &'a str,
    appid: &'a str,
    salt: &'a str,
    sign: &'a str,
}

/// 百度大模型文本翻译 (aiTextTranslate) 响应结构体（兼容多种外层包裹字段）
#[derive(Deserialize)]
struct BaiduLlmResponse {
    #[serde(rename = "trans_result")]
    trans_result: Option<Vec<TransResult>>,
    #[serde(rename = "result")]
    result: Option<Vec<TransResult>>,
    #[serde(rename = "data")]
    data: Option<BaiduLlmData>,
    #[serde(rename = "error_code")]
    error_code: Option<serde_json::Value>,
    #[serde(rename = "error_msg")]
    error_msg: Option<String>,
}

#[derive(Deserialize)]
struct BaiduLlmData {
    #[serde(rename = "trans_result")]
    trans_result: Option<Vec<TransResult>>,
    #[serde(rename = "result")]
    result: Option<Vec<TransResult>>,
}

#[derive(Deserialize)]
struct TransResult {
    #[allow(dead_code)]
    src: Option<String>,
    dst: String,
}

impl BaiduTranslator {
    /// 初始化百度翻译客户端
    pub fn new(config: &crate::config::AppConfig) -> Result<Self> {
        if config.baidu.appid.is_empty() || config.baidu.secret_key.is_empty() {
            anyhow::bail!(
                "百度翻译 API 凭证未配置！\n请编辑配置文件: {}",
                crate::config::AppConfig::config_path().display()
            );
        }

        let cache_size = NonZeroUsize::new(config.app.cache_size)
            .unwrap_or(NonZeroUsize::new(512).unwrap());
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .context("创建 HTTP 客户端失败")?;

        Ok(Self {
            config: config.baidu.clone(),
            client,
            cache: LruCache::new(cache_size),
        })
    }

    /// 获取当前生效的翻译模式
    #[allow(dead_code)]
    pub fn get_mode(&self) -> TranslateMode {
        self.config.mode
    }

    /// 动态切换翻译模式（通用文本翻译 / 大模型文本翻译）
    pub fn set_mode(&mut self, mode: TranslateMode) {
        if self.config.mode != mode {
            println!("[Translator] 切换翻译模式: {} -> {}", self.config.mode, mode);
            self.config.mode = mode;
        }
    }

    /// 执行文本翻译（自动根据当前模式分发并读取/写入缓存）
    pub async fn translate(&mut self, text: &str) -> Result<String> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Ok(String::new());
        }

        let mode = self.config.mode;
        let cache_key = (mode, trimmed.to_lowercase());

        if let Some(cached) = self.cache.get(&cache_key) {
            println!("[Translator] 命中 LRU 缓存 [{}]", mode);
            return Ok(cached.clone());
        }

        let result = match mode {
            TranslateMode::General => self.call_general_api(trimmed).await?,
            TranslateMode::Llm => self.call_llm_api(trimmed).await?,
        };

        self.cache.put(cache_key, result.clone());
        Ok(result)
    }

    /// 调用百度“通用文本翻译 API”
    ///
    /// 接口: `https://fanyi-api.baidu.com/api/trans/vip/translate`
    /// 鉴权: `sign = md5(appid + q + salt + secret_key)`
    async fn call_general_api(&self, text: &str) -> Result<String> {
        let salt: u32 = rand::random();
        let sign_str = format!(
            "{}{}{}{}",
            self.config.appid, text, salt, self.config.secret_key
        );
        let sign = format!("{:x}", md5::compute(sign_str.as_bytes()));

        let url = "https://fanyi-api.baidu.com/api/trans/vip/translate";

        let response = self
            .client
            .post(url)
            .form(&[
                ("q", text),
                ("from", &self.config.from),
                ("to", &self.config.to),
                ("appid", &self.config.appid),
                ("salt", &salt.to_string()),
                ("sign", &sign),
            ])
            .send()
            .await
            .context("百度通用翻译 API 请求失败")?;

        let body = response.text().await.context("读取百度通用翻译响应失败")?;

        let resp: BaiduResponse =
            serde_json::from_str(&body).context("解析百度通用翻译响应失败")?;

        if let Some(code_val) = resp.error_code {
            let code_str = match code_val {
                serde_json::Value::String(s) => s,
                serde_json::Value::Number(n) => n.to_string(),
                v => v.to_string(),
            };
            if code_str != "52000" && code_str != "0" {
                let msg = resp.error_msg.unwrap_or_default();
                anyhow::bail!("百度通用翻译错误 [{code_str}]: {msg}");
            }
        }

        let results = resp.trans_result.context("百度通用翻译未返回结果")?;
        let translated: String = results.iter().map(|r| r.dst.as_str()).collect::<Vec<_>>().join("\n");

        if translated.is_empty() {
            anyhow::bail!("百度通用翻译返回结果为空");
        }

        Ok(translated)
    }

    /// 调用百度“大模型文本翻译 API” (文心一言/AI 大模型赋能)
    ///
    /// 接口: `https://fanyi-api.baidu.com/ait/api/aiTextTranslate`
    /// 格式: `application/json`
    /// 鉴权: `sign = md5(appid + q + salt + secret_key)`
    async fn call_llm_api(&self, text: &str) -> Result<String> {
        let salt: u32 = rand::random();
        let salt_str = salt.to_string();
        let sign_str = format!(
            "{}{}{}{}",
            self.config.appid, text, salt_str, self.config.secret_key
        );
        let sign = format!("{:x}", md5::compute(sign_str.as_bytes()));

        let url = "https://fanyi-api.baidu.com/ait/api/aiTextTranslate";

        let payload = BaiduLlmRequest {
            q: text,
            from: &self.config.from,
            to: &self.config.to,
            appid: &self.config.appid,
            salt: &salt_str,
            sign: &sign,
        };

        let response = self
            .client
            .post(url)
            .header("Content-Type", "application/json")
            .json(&payload)
            .send()
            .await
            .context("百度大模型翻译 API 请求失败")?;

        let body = response.text().await.context("读取百度大模型翻译响应失败")?;

        let resp: BaiduLlmResponse =
            serde_json::from_str(&body).context("解析百度大模型翻译响应失败")?;

        if let Some(code_val) = resp.error_code {
            let code_str = match code_val {
                serde_json::Value::String(s) => s,
                serde_json::Value::Number(n) => n.to_string(),
                v => v.to_string(),
            };
            if code_str != "52000" && code_str != "0" {
                let msg = resp.error_msg.unwrap_or_default();
                anyhow::bail!("百度大模型翻译错误 [{code_str}]: {msg}");
            }
        }

        // 提取翻译结果，优先从 trans_result / result / data.trans_result 中提取
        let results_opt = resp
            .trans_result
            .or(resp.result)
            .or_else(|| resp.data.and_then(|d| d.trans_result.or(d.result)));

        let results = results_opt.context("百度大模型翻译未返回结果内容")?;
        let translated: String = results.iter().map(|r| r.dst.as_str()).collect::<Vec<_>>().join("\n");

        if translated.is_empty() {
            anyhow::bail!("百度大模型翻译返回结果为空");
        }

        Ok(translated)
    }
}

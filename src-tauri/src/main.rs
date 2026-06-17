use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{fs, path::{Path, PathBuf}, sync::Mutex, thread, time::Duration};
use tauri::{AppHandle, Manager, State};
use uuid::Uuid;

const POLL_INTERVAL_SECS: u64 = 3;
const POLL_MAX_ATTEMPTS: usize = 120;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProviderConfig {
    id: String,
    name: String,
    base_url: String,
    api_key: String,
    model: String,
    adapter: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AppSettings {
    providers: Vec<ProviderConfig>,
    active_provider_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HistoryItem {
    id: String,
    prompt: String,
    size: String,
    provider_name: String,
    adapter: String,
    reference_paths: Vec<String>,
    status: String,
    #[serde(default)]
    task_id: Option<String>,
    #[serde(default)]
    stored_path: String,
    #[serde(default)]
    error: Option<String>,
    created_at: String,
    #[serde(default)]
    finished_at: String,
    #[serde(default)]
    duration_seconds: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AppData {
    settings: AppSettings,
    history: Vec<HistoryItem>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ImageInput {
    file_name: String,
    mime: String,
    bytes: Vec<u8>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ReferenceImage {
    id: String,
    name: String,
    mime: String,
    size_bytes: u64,
    stored_path: String,
    created_at: String,
}

struct AppStore {
    data: Mutex<AppData>,
    state_path: PathBuf,
    data_dir: PathBuf,
}

impl AppStore {
    fn load(app: &tauri::App) -> Result<Self, String> {
        let data_dir = app
            .path()
            .app_data_dir()
            .map_err(|e| format!("读取 app data 目录失败：{e}"))?;
        fs::create_dir_all(&data_dir).map_err(|e| format!("创建 app data 目录失败：{e}"))?;
        let state_path = data_dir.join("state.json");
        let mut data = if state_path.exists() {
            let text = fs::read_to_string(&state_path).map_err(|e| format!("读取状态失败：{e}"))?;
            serde_json::from_str(&text).unwrap_or_else(|_| default_data())
        } else {
            default_data()
        };
        data.settings = normalize_settings(data.settings);
        Ok(Self {
            data: Mutex::new(data),
            state_path,
            data_dir,
        })
    }

    fn save(&self, data: &AppData) -> Result<(), String> {
        let text = serde_json::to_string_pretty(data).map_err(|e| format!("序列化状态失败：{e}"))?;
        fs::write(&self.state_path, format!("{text}\n")).map_err(|e| format!("保存状态失败：{e}"))
    }

    fn references_dir(&self) -> PathBuf {
        self.data_dir.join("references")
    }

    fn results_dir(&self) -> PathBuf {
        self.data_dir.join("results")
    }

    fn downloads_dir(&self) -> PathBuf {
        self.data_dir.join("downloads")
    }
}

fn default_data() -> AppData {
    AppData {
        settings: AppSettings {
            providers: vec![ProviderConfig {
                id: "default".to_string(),
                name: "GeekAI Proxy".to_string(),
                base_url: "https://geekai.co/api/v1".to_string(),
                api_key: String::new(),
                model: "gpt-image-2".to_string(),
                adapter: "async_generations".to_string(),
            }],
            active_provider_id: "default".to_string(),
        },
        history: vec![],
    }
}

#[tauri::command]
fn get_app_state(store: State<'_, AppStore>) -> Result<AppData, String> {
    Ok(store.data.lock().map_err(|_| "状态锁异常".to_string())?.clone())
}

#[tauri::command]
fn update_settings(store: State<'_, AppStore>, settings: AppSettings) -> Result<AppData, String> {
    let mut data = store.data.lock().map_err(|_| "状态锁异常".to_string())?;
    data.settings = normalize_settings(settings);
    store.save(&data)?;
    Ok(data.clone())
}

#[tauri::command]
fn save_reference_image(store: State<'_, AppStore>, input: ImageInput) -> Result<ReferenceImage, String> {
    if input.bytes.is_empty() {
        return Err("图片为空".to_string());
    }
    let dir = store.references_dir();
    fs::create_dir_all(&dir).map_err(|e| format!("创建参考图目录失败：{e}"))?;
    let ext = extension_for(&input.file_name, &input.mime);
    let id = format!("ref_{}", Uuid::new_v4().simple());
    let path = dir.join(format!("{id}.{ext}"));
    fs::write(&path, &input.bytes).map_err(|e| format!("保存参考图失败：{e}"))?;
    Ok(ReferenceImage {
        id,
        name: input.file_name,
        mime: input.mime,
        size_bytes: input.bytes.len() as u64,
        stored_path: path.to_string_lossy().to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
    })
}

#[tauri::command]
fn generate_image(
    app: AppHandle,
    store: State<'_, AppStore>,
    prompt: String,
    size: String,
    reference_paths: Vec<String>,
) -> Result<HistoryItem, String> {
    let prompt = prompt.trim().to_string();
    if prompt.is_empty() {
        return Err("提示词为空".to_string());
    }
    println!("[generate_image] request size={} refs={}", size, reference_paths.len());
    let provider = active_provider(&store)?;
    validate_provider(&provider)?;
    println!(
        "[generate_image] provider={} adapter={} model={} size={} refs={}",
        provider.name,
        provider.adapter,
        provider.model,
        size,
        reference_paths.len()
    );
    let item = HistoryItem {
        id: format!("img_{}", Uuid::new_v4().simple()),
        prompt: prompt.clone(),
        size,
        provider_name: provider.name.clone(),
        adapter: provider.adapter.clone(),
        reference_paths: reference_paths.clone(),
        status: "pending".to_string(),
        task_id: None,
        stored_path: String::new(),
        error: None,
        created_at: now_rfc3339(),
        finished_at: String::new(),
        duration_seconds: None,
    };
    let mut data = store.data.lock().map_err(|_| "状态锁异常".to_string())?;
    data.history.insert(0, item.clone());
    data.history.truncate(80);
    store.save(&data)?;
    println!("[generate_image] queued id={}", item.id);
    spawn_backend_submit(
        app,
        item.id.clone(),
        provider,
        prompt,
        item.size.clone(),
        reference_paths,
    );
    Ok(item)
}

#[tauri::command]
fn query_image_task(store: State<'_, AppStore>, history_id: String) -> Result<HistoryItem, String> {
    query_image_task_inner(&store, &history_id)
}

fn query_image_task_inner(store: &AppStore, history_id: &str) -> Result<HistoryItem, String> {
    let (provider, item) = {
        let data = store.data.lock().map_err(|_| "状态锁异常".to_string())?;
        let item = data.history.iter().find(|item| item.id == history_id)
            .cloned()
            .ok_or_else(|| "历史记录不存在".to_string())?;
        let provider = data.settings.providers.iter()
            .find(|provider| provider.name == item.provider_name)
            .or_else(|| data.settings.providers.first())
            .cloned()
            .ok_or_else(|| "未配置 Provider".to_string())?;
        (provider, item)
    };
    if item.status != "pending" {
        return Ok(item);
    }
    let task_id = match item.task_id.clone() {
        Some(task_id) => task_id,
        None => {
            return fail_history(store, history_id, "历史记录缺少 task id".to_string());
        }
    };
    let url = format!("{}/images/{}", provider.base_url.trim().trim_end_matches('/'), task_id);
    let response = http_get(&url, &provider.api_key)?;
    let payload = parse_json(&response, &url)?;
    let status = extract_status(&payload);
    if matches!(status.as_deref(), Some("failed" | "error" | "canceled" | "cancelled")) {
        let message = payload.get("error")
            .or_else(|| payload.get("message"))
            .and_then(|v| v.as_str())
            .unwrap_or("生成失败")
            .to_string();
        return fail_history(store, history_id, message);
    }
    if !matches!(status.as_deref(), Some("completed" | "succeeded" | "succeed" | "success" | "finished" | "done")) {
        return Ok(item);
    }
    let bytes = extract_image_bytes(&payload)?;
    let completed = store_result(&store, item, bytes)?;
    update_history(&store, &history_id, |item| {
        *item = completed.clone();
    })
}

#[tauri::command]
fn download_result(app: AppHandle, store: State<'_, AppStore>, history_id: String) -> Result<String, String> {
    let item = store.data.lock().map_err(|_| "状态锁异常".to_string())?
        .history.iter()
        .find(|item| item.id == history_id)
        .cloned()
        .ok_or_else(|| "历史记录不存在".to_string())?;
    if item.status != "completed" || item.stored_path.trim().is_empty() {
        return Err("图片尚未生成完成".to_string());
    }
    let dir = app.path().download_dir().unwrap_or_else(|_| store.downloads_dir());
    fs::create_dir_all(&dir).map_err(|e| format!("创建下载目录失败：{e}"))?;
    let ext = Path::new(&item.stored_path)
        .extension()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("png");
    let dest = unique_download_path(&dir, &item.id, ext);
    fs::copy(&item.stored_path, &dest).map_err(|e| format!("保存图片失败：{e}"))?;
    Ok(dest.to_string_lossy().to_string())
}

#[tauri::command]
fn clear_history(store: State<'_, AppStore>) -> Result<AppData, String> {
    let mut data = store.data.lock().map_err(|_| "状态锁异常".to_string())?;
    data.history.clear();
    store.save(&data)?;
    Ok(data.clone())
}

struct SubmitResult {
    task_id: Option<String>,
    bytes: Option<Vec<u8>>,
}

fn submit_generation(
    provider: &ProviderConfig,
    prompt: &str,
    size: &str,
    reference_paths: &[String],
) -> Result<SubmitResult, String> {
    match provider.adapter.as_str() {
        "openai_edits" if !reference_paths.is_empty() => submit_openai_edits(provider, prompt, size, reference_paths),
        "sync_generations" => submit_json_generation(provider, prompt, size, reference_paths, false),
        _ => submit_json_generation(provider, prompt, size, reference_paths, true),
    }
}

fn submit_json_generation(
    provider: &ProviderConfig,
    prompt: &str,
    size: &str,
    reference_paths: &[String],
    async_mode: bool,
) -> Result<SubmitResult, String> {
    let mut body = serde_json::json!({
        "model": provider.model.trim(),
        "prompt": prompt,
        "n": 1,
        "size": size,
        "response_format": "url"
    });
    if async_mode {
        body["async"] = serde_json::Value::Bool(true);
    }
    let refs = read_refs_base64(reference_paths)?;
    if refs.len() == 1 {
        body["image"] = serde_json::Value::String(refs[0].clone());
    } else if !refs.is_empty() {
        body["images"] = serde_json::Value::Array(refs.into_iter().map(serde_json::Value::String).collect());
    }
    let url = format!("{}/images/generations", provider.base_url.trim().trim_end_matches('/'));
    let text = http_post_json(&url, &provider.api_key, &body)?;
    let payload = parse_json(&text, &url)?;
    if async_mode {
        let task_id = extract_task_id(&payload).ok_or_else(|| {
            format!("异步提交成功但响应中没有 task id：{}", truncate(&payload.to_string(), 300))
        })?;
        Ok(SubmitResult { task_id: Some(task_id), bytes: None })
    } else {
        Ok(SubmitResult { task_id: None, bytes: Some(extract_image_bytes(&payload)?) })
    }
}

fn submit_openai_edits(
    provider: &ProviderConfig,
    prompt: &str,
    size: &str,
    reference_paths: &[String],
) -> Result<SubmitResult, String> {
    let url = format!("{}/images/edits", provider.base_url.trim().trim_end_matches('/'));
    let mut form = reqwest::blocking::multipart::Form::new()
        .text("model", provider.model.trim().to_string())
        .text("prompt", prompt.to_string())
        .text("size", size.to_string())
        .text("n", "1")
        .text("response_format", "b64_json");
    for path in reference_paths {
        let bytes = fs::read(path).map_err(|e| format!("读取参考图失败 {path}：{e}"))?;
        let file_name = Path::new(path).file_name().and_then(|s| s.to_str()).unwrap_or("image.png").to_string();
        let part = reqwest::blocking::multipart::Part::bytes(bytes).file_name(file_name);
        form = form.part("image[]", part);
    }
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .connect_timeout(std::time::Duration::from_secs(60))
        .build()
        .map_err(|e| format!("创建 HTTP 客户端失败：{e}"))?;
    let response = client.post(&url)
        .bearer_auth(provider.api_key.trim())
        .multipart(form)
        .send()
        .map_err(|e| format!("图片生成请求失败：{e}"))?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().unwrap_or_default();
        return Err(format!("图片生成失败 HTTP {status}：{}", truncate(&body, 500)));
    }
    let text = response.text().map_err(|e| format!("读取响应失败：{e}"))?;
    let payload = parse_json(&text, &url)?;
    Ok(SubmitResult { task_id: extract_task_id(&payload), bytes: Some(extract_image_bytes(&payload)?) })
}

fn http_post_json(url: &str, api_key: &str, body: &serde_json::Value) -> Result<String, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .connect_timeout(std::time::Duration::from_secs(60))
        .build()
        .map_err(|e| format!("创建 HTTP 客户端失败：{e}"))?;
    let response = client.post(url)
        .bearer_auth(api_key.trim())
        .header("content-type", "application/json")
        .body(body.to_string())
        .send()
        .map_err(|e| format!("图片生成请求失败：{e}"))?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().unwrap_or_default();
        return Err(format!("图片生成失败 HTTP {status}：{}", truncate(&body, 500)));
    }
    response.text().map_err(|e| format!("读取响应失败：{e}"))
}

fn http_get(url: &str, api_key: &str) -> Result<String, String> {
    let response = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .connect_timeout(std::time::Duration::from_secs(20))
        .build()
        .map_err(|e| format!("创建 HTTP 客户端失败：{e}"))?
        .get(url)
        .bearer_auth(api_key.trim())
        .send()
        .map_err(|e| format!("查询任务失败：{e}"))?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().unwrap_or_default();
        return Err(format!("查询任务失败 HTTP {status}：{}", truncate(&body, 500)));
    }
    response.text().map_err(|e| format!("读取响应失败：{e}"))
}

fn extract_image_bytes(payload: &serde_json::Value) -> Result<Vec<u8>, String> {
    let first = payload.get("data")
        .and_then(|d| d.as_array())
        .and_then(|arr| arr.first())
        .or_else(|| payload.get("data"))
        .unwrap_or(payload);
    if let Some(b64) = first.get("b64_json").and_then(|v| v.as_str()) {
        return BASE64.decode(b64.as_bytes()).map_err(|e| format!("base64 解码失败：{e}"));
    }
    let image_url = first.get("url")
        .or_else(|| payload.get("url"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("响应中未找到图片数据：{}", truncate(&payload.to_string(), 300)))?;
    let bytes = reqwest::blocking::get(image_url)
        .map_err(|e| format!("下载图片失败：{e}"))?
        .bytes()
        .map_err(|e| format!("读取图片失败：{e}"))?
        .to_vec();
    Ok(bytes)
}

fn store_result(store: &AppStore, mut item: HistoryItem, bytes: Vec<u8>) -> Result<HistoryItem, String> {
    let dir = store.results_dir();
    fs::create_dir_all(&dir).map_err(|e| format!("创建结果目录失败：{e}"))?;
    let path = dir.join(format!("{}.png", item.id));
    fs::write(&path, bytes).map_err(|e| format!("写入结果图失败：{e}"))?;
    item.status = "completed".to_string();
    item.stored_path = path.to_string_lossy().to_string();
    item.error = None;
    item.finished_at = now_rfc3339();
    item.duration_seconds = Some(elapsed_seconds(&item.created_at, &item.finished_at));
    Ok(item)
}

fn now_rfc3339() -> String {
    Utc::now().to_rfc3339()
}

fn elapsed_seconds(started_at: &str, finished_at: &str) -> u64 {
    let started = DateTime::parse_from_rfc3339(started_at).map(|value| value.with_timezone(&Utc));
    let finished = DateTime::parse_from_rfc3339(finished_at).map(|value| value.with_timezone(&Utc));
    match (started, finished) {
        (Ok(started), Ok(finished)) => finished
            .signed_duration_since(started)
            .to_std()
            .map(|duration| duration.as_secs())
            .unwrap_or(0),
        _ => 0,
    }
}

fn fail_history(store: &AppStore, history_id: &str, message: String) -> Result<HistoryItem, String> {
    update_history(store, history_id, |item| {
        item.status = "failed".to_string();
        item.error = Some(message);
        item.finished_at = now_rfc3339();
        item.duration_seconds = Some(elapsed_seconds(&item.created_at, &item.finished_at));
    })
}

fn complete_history_with_bytes(
    store: &AppStore,
    history_id: &str,
    task_id: Option<String>,
    bytes: Vec<u8>,
) -> Result<HistoryItem, String> {
    let mut item = {
        let data = store.data.lock().map_err(|_| "状态锁异常".to_string())?;
        data.history
            .iter()
            .find(|item| item.id == history_id)
            .cloned()
            .ok_or_else(|| "历史记录不存在".to_string())?
    };
    if task_id.is_some() {
        item.task_id = task_id;
    }
    let completed = store_result(store, item, bytes)?;
    update_history(store, history_id, |item| {
        *item = completed.clone();
    })
}

fn update_history<F>(store: &AppStore, id: &str, f: F) -> Result<HistoryItem, String>
where
    F: FnOnce(&mut HistoryItem),
{
    let mut data = store.data.lock().map_err(|_| "状态锁异常".to_string())?;
    let mut updated = None;
    if let Some(item) = data.history.iter_mut().find(|item| item.id == id) {
        f(item);
        updated = Some(item.clone());
    }
    store.save(&data)?;
    updated.ok_or_else(|| "历史记录不存在".to_string())
}

fn normalize_settings(mut settings: AppSettings) -> AppSettings {
    if settings.providers.is_empty() {
        return default_data().settings;
    }
    for (index, provider) in settings.providers.iter_mut().enumerate() {
        if provider.id.trim().is_empty() {
            provider.id = if index == 0 { "default".to_string() } else { format!("provider-{index}") };
        }
        if provider.name.trim().is_empty() {
            provider.name = format!("Provider {}", index + 1);
        }
        if provider.model.trim().is_empty() {
            provider.model = "gpt-image-2".to_string();
        }
        if provider.id == "default"
            && provider.name == "GeekAI Proxy"
            && provider.base_url == "https://api.example.com/v1"
            && provider.model == "gpt-image-1"
            && provider.api_key.trim().is_empty()
        {
            provider.base_url = "https://geekai.co/api/v1".to_string();
            provider.model = "gpt-image-2".to_string();
        }
        if provider.adapter.trim().is_empty() {
            provider.adapter = "async_generations".to_string();
        }
    }
    if !settings.providers.iter().any(|p| p.id == settings.active_provider_id) {
        settings.active_provider_id = settings.providers[0].id.clone();
    }
    settings
}

fn active_provider(store: &State<'_, AppStore>) -> Result<ProviderConfig, String> {
    let data = store.data.lock().map_err(|_| "状态锁异常".to_string())?;
    data.settings.providers.iter()
        .find(|provider| provider.id == data.settings.active_provider_id)
        .or_else(|| data.settings.providers.first())
        .cloned()
        .ok_or_else(|| "未配置 Provider".to_string())
}

fn validate_provider(provider: &ProviderConfig) -> Result<(), String> {
    if provider.base_url.trim().is_empty() { return Err("Base URL 为空".to_string()); }
    if provider.api_key.trim().is_empty() { return Err("API Key 为空".to_string()); }
    if provider.model.trim().is_empty() { return Err("模型名称为空".to_string()); }
    Ok(())
}

fn read_refs_base64(paths: &[String]) -> Result<Vec<String>, String> {
    paths.iter()
        .map(|path| {
            let bytes = fs::read(path).map_err(|e| format!("读取参考图失败 {path}：{e}"))?;
            Ok(BASE64.encode(bytes))
        })
        .collect()
}

fn parse_json(text: &str, url: &str) -> Result<serde_json::Value, String> {
    serde_json::from_str(text).map_err(|e| format!("{url} 返回非 JSON：{e}；{}", truncate(text, 300)))
}

fn extract_task_id(v: &serde_json::Value) -> Option<String> {
    for key in ["task_id", "taskId", "id"] {
        if let Some(s) = v.get(key).and_then(|x| x.as_str()).filter(|s| !s.is_empty()) {
            return Some(s.to_string());
        }
    }
    if let Some(d) = v.get("data") {
        for key in ["task_id", "taskId", "id"] {
            if let Some(s) = d.get(key).and_then(|x| x.as_str()).filter(|s| !s.is_empty()) {
                return Some(s.to_string());
            }
        }
        if let Some(first) = d.as_array().and_then(|arr| arr.first()) {
            for key in ["task_id", "taskId", "id"] {
                if let Some(s) = first.get(key).and_then(|x| x.as_str()).filter(|s| !s.is_empty()) {
                    return Some(s.to_string());
                }
            }
        }
    }
    None
}

fn extract_status(v: &serde_json::Value) -> Option<String> {
    v.get("status")
        .or_else(|| v.get("task_status"))
        .or_else(|| v.get("data").and_then(|d| d.get("status")))
        .or_else(|| v.get("data").and_then(|d| d.get("task_status")))
        .and_then(|v| v.as_str())
        .map(|s| s.to_lowercase())
}

fn extension_for(file_name: &str, mime: &str) -> &'static str {
    let lower = file_name.to_ascii_lowercase();
    if lower.ends_with(".jpg") || lower.ends_with(".jpeg") || mime.contains("jpeg") { "jpg" }
    else if lower.ends_with(".webp") || mime.contains("webp") { "webp" }
    else { "png" }
}

fn unique_download_path(dir: &Path, history_id: &str, ext: &str) -> PathBuf {
    let safe_id = history_id
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' ) { ch } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .trim_matches('_')
        .to_string();
    let stem = if safe_id.is_empty() {
        "image-lab".to_string()
    } else {
        format!("image-lab-{}", safe_id)
    };
    let mut candidate = dir.join(format!("{stem}.{ext}"));
    if !candidate.exists() {
        return candidate;
    }
    for index in 1.. {
        candidate = dir.join(format!("{stem}-{index}.{ext}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    unreachable!("path generation loop should always return a unique file path");
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max { s.to_string() }
    else { format!("{}...", s.chars().take(max).collect::<String>()) }
}

fn spawn_backend_poll(app: AppHandle, history_id: String) {
    thread::spawn(move || {
        for attempt in 0..POLL_MAX_ATTEMPTS {
            thread::sleep(Duration::from_secs(POLL_INTERVAL_SECS));
            let store = app.state::<AppStore>();
            match query_image_task_inner(&store, &history_id) {
                Ok(item) if item.status != "pending" => break,
                Ok(_) => {}
                Err(error) if error.contains("缺少 task id") || error.contains("历史记录不存在") => {
                    let _ = fail_history(&store, &history_id, error);
                    break;
                }
                Err(error) if attempt + 1 >= POLL_MAX_ATTEMPTS => {
                    let _ = fail_history(&store, &history_id, format!("轮询超时：{error}"));
                    break;
                }
                Err(_) => {}
            }
        }
    });
}

fn spawn_backend_submit(
    app: AppHandle,
    history_id: String,
    provider: ProviderConfig,
    prompt: String,
    size: String,
    reference_paths: Vec<String>,
) {
    thread::spawn(move || {
        println!(
            "[generate_image/bg_submit] start id={} provider={} adapter={}",
            history_id,
            provider.name,
            provider.adapter
        );
        let store = app.state::<AppStore>();
        match submit_generation(&provider, &prompt, &size, &reference_paths) {
            Ok(submit) => {
                if let Some(bytes) = submit.bytes {
                    match complete_history_with_bytes(&store, &history_id, submit.task_id, bytes) {
                        Ok(item) => {
                            println!(
                                "[generate_image/bg_submit] completed id={} path={}",
                                item.id, item.stored_path
                            );
                        }
                        Err(error) => {
                            eprintln!("[generate_image/bg_submit] complete failed id={} error={}", history_id, error);
                        }
                    }
                    return;
                }
                let task_id = match submit.task_id {
                    Some(task_id) => task_id,
                    None => {
                        let _ = fail_history(&store, &history_id, "异步提交成功但响应中没有 task id".to_string());
                        return;
                    }
                };
                match update_history(&store, &history_id, |item| {
                    item.task_id = Some(task_id.clone());
                    item.error = None;
                }) {
                    Ok(item) => {
                        println!(
                            "[generate_image/bg_submit] pending id={} task_id={}",
                            item.id,
                            item.task_id.as_deref().unwrap_or("")
                        );
                        spawn_backend_poll(app, history_id);
                    }
                    Err(error) => {
                        eprintln!("[generate_image/bg_submit] update failed id={} error={}", history_id, error);
                    }
                }
            }
            Err(error) => {
                let _ = fail_history(&store, &history_id, error);
            }
        }
    });
}

fn spawn_pending_pollers(app: AppHandle) {
    let ids = {
        let store = app.state::<AppStore>();
        let ids = match store.data.lock() {
            Ok(data) => data.history.iter()
                .filter(|item| item.status == "pending" && item.task_id.is_some())
                .map(|item| item.id.clone())
                .collect::<Vec<_>>(),
            Err(_) => Vec::new(),
        };
        ids
    };
    for id in ids {
        spawn_backend_poll(app.clone(), id);
    }
}

fn main() {
    tauri::Builder::default()
        .setup(|app| {
            let store = AppStore::load(app).map_err(|e| Box::<dyn std::error::Error>::from(e))?;
            app.manage(store);
            spawn_pending_pollers(app.handle().clone());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_app_state,
            update_settings,
            save_reference_image,
            generate_image,
            query_image_task,
            download_result,
            clear_history,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Image Lab");
}

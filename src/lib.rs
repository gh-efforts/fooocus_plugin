use std::{fs, io};
use std::fs::FileTimes;
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;
use async_openai::config::OpenAIConfig;
use async_openai::types::{ChatCompletionRequestSystemMessageArgs, ChatCompletionRequestUserMessageArgs, CreateChatCompletionRequestArgs};

use nacos_sdk::api::constants;
use nacos_sdk::api::naming::{NamingServiceBuilder, ServiceInstance};
use nacos_sdk::api::naming::NamingService;
use nacos_sdk::api::props::ClientProps;
use named_lock::{NamedLock, NamedLockGuard};
use pyo3::exceptions::PyTypeError;
use pyo3::prelude::*;
use rand::Rng;
use serde::Deserialize;
use ureq::Agent;

static CONFIG: OnceLock<Config> = OnceLock::new();

#[derive(Deserialize)]
struct ServiceConfig {
    server_addr: String,
    username: String,
    password: String,
    namespace: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum TranslatorConfig {
    LibreTranslate {
        api: String
    },
    OpenAi {
        keys: Vec<String>
    }
}

#[derive(Deserialize)]
struct ModelsConfig {
    model_memory_path: PathBuf,
    model_path: PathBuf,
    checkpoints_memory_limit: u64,
    loras_memory_limit: u64,
}

#[derive(Deserialize)]
struct Config {
    nacos_service_config: ServiceConfig,
    translator_config: TranslatorConfig,
    models_config: ModelsConfig,
}

fn find_lan_addr() -> std::io::Result<IpAddr> {
    let socket = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    socket.connect("8.8.8.8:53")?;
    let addr = socket.local_addr()?;
    Ok(addr.ip())
}

#[pyfunction]
fn init(config_path: &str) {
    let level = std::env::var("FOOOCUS_PLUGIN_LOG")
        .map(|s| tracing::Level::from_str(&s).unwrap())
        .unwrap_or(tracing::Level::INFO);

    tracing_subscriber::fmt()
        .with_max_level(level)
        .init();

    CONFIG.get_or_init(|| {
        let f = || {
            let c = std::fs::read_to_string(config_path)?;
            toml::from_str(&c).map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
        };
        f().unwrap()
    });
}

fn create_naming_service() -> nacos_sdk::api::error::Result<impl NamingService> {
    let config = CONFIG
        .get()
        .ok_or_else(|| nacos_sdk::api::error::Error::ErrResult(String::from("fooocus plugin is not initialized")))?;

    let config = &config.nacos_service_config;

    NamingServiceBuilder::new(
        ClientProps::new()
            .server_addr(&config.server_addr)
            .auth_username(&config.username)
            .auth_password(&config.password)
            .namespace(&config.namespace)
    )
        .enable_auth_plugin_http()
        .build()
}

static NAMING_SERVICE: OnceLock<Box<dyn NamingService + Sync + Send + 'static>> = OnceLock::new();
static DEREGISTER_SERVICE: Mutex<Option<Box<dyn Fn() -> nacos_sdk::api::error::Result<()> + Sync + Send + 'static>>> = Mutex::new(None);

#[pyfunction]
fn service_register(
    instance_port: u16,
    instance_name: String,
    metadata_json: &str,
) -> PyResult<()> {
    let instance_ip = find_lan_addr().map_err(|e| PyTypeError::new_err(e.to_string()))?;

    let service = NAMING_SERVICE.get_or_init(|| {
        let s = create_naming_service().unwrap();
        Box::new(s)
    });

    let instance = ServiceInstance {
        ip: instance_ip.to_string(),
        port: instance_port as i32,
        metadata: serde_json::from_str(metadata_json).map_err(|e| PyTypeError::new_err(e.to_string()))?,
        ..Default::default()
    };

    let res = {
        let instance = instance.clone();
        let instance_name = instance_name.clone();

        service.register_instance(
            instance_name,
            Some(constants::DEFAULT_GROUP.to_string()),
            instance,
        ).map_err(|e| PyTypeError::new_err(e.to_string()))
    };

    if res.is_ok() {
        let deregister_fn = move || {
            service.deregister_instance(
                instance_name.clone(),
                Some(constants::DEFAULT_GROUP.to_string()),
                instance.clone(),
            )
        };

        *DEREGISTER_SERVICE.lock().unwrap() = Some(Box::new(deregister_fn));
    };
    res
}

#[pyfunction]
fn service_deregister() -> PyResult<()> {
    match DEREGISTER_SERVICE.lock().unwrap().as_ref() {
        None => Err(PyTypeError::new_err("service has not been registered")),
        Some(v) => v().map_err(|e| PyTypeError::new_err(e.to_string()))
    }
}

trait Translate {
    fn text_translate(&self, input_text: &str, dst_language: &str) -> PyResult<String>;
}

struct LibreTranslate {
    client: Agent,
    api: String
}

impl Translate for LibreTranslate {
    fn text_translate(&self, input_text: &str, dst_language: &str) -> PyResult<String> {
        let resp = self.client.post(&self.api)
            .send_json(ureq::json!({
                    "q": input_text,
                    "source": "auto",
                    "target": dst_language
                })).map_err(|e| PyTypeError::new_err(e.to_string()))?;

        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Message {
            translated_text: String,
        }

        let msg: Message = resp.into_json()?;
        Ok(msg.translated_text)
    }
}

struct OpenAI {
    clients: Vec<(String, async_openai::Client<OpenAIConfig>)>,
    tokio_rt: tokio::runtime::Runtime
}

impl Translate for OpenAI {
    fn text_translate(&self, input_text: &str, dst_language: &str) -> PyResult<String> {
        let request = CreateChatCompletionRequestArgs::default()
            .model("gpt-3.5-turbo")
            .messages([
                ChatCompletionRequestSystemMessageArgs::default()
                    .content(format!("Translate all text into {}", dst_language))
                    .build()
                    .map_err(|e| PyTypeError::new_err(e.to_string()))?
                    .into(),
                ChatCompletionRequestUserMessageArgs::default()
                    .content(input_text)
                    .build()
                    .map_err(|e| PyTypeError::new_err(e.to_string()))?
                    .into(),
            ])
            .build()
            .map_err(|e| PyTypeError::new_err(e.to_string()))?;

        let clients = self.clients.as_slice();
        let i = rand::thread_rng().gen_range(0..clients.len());
        let (suffix, client) = &clients[i];

        let mut response = self.tokio_rt
            .block_on(client.chat().create(request))
            .map_err(|e|  PyTypeError::new_err(format!("openai[{}] error: {}", suffix, e.to_string())))?;
        let choice = response.choices.pop().ok_or_else(||  PyTypeError::new_err("choices is empty"))?;
        let content = choice.message.content.ok_or_else(||  PyTypeError::new_err("content is empty"))?;

        Ok(content)
    }
}

#[pyfunction]
fn text_translate(
    input_text: &str,
    dst_language: &str,
) -> PyResult<String> {
    static TRANSLATOR: OnceLock<Box<dyn Translate + Sync + Send + 'static>> = OnceLock::new();

    let config = &CONFIG.get().ok_or_else(|| PyTypeError::new_err("fooocus plugin is not initialized"))?.translator_config;

    let translator = TRANSLATOR.get_or_init(|| {
        match config {
            TranslatorConfig::LibreTranslate { api } => {
                let translator = LibreTranslate {
                    client: ureq::Agent::new(),
                    api: api.clone()
                };
                Box::new(translator)
            }
            TranslatorConfig::OpenAi { keys} => {
                let client = reqwest::Client::new();
                let mut clients = Vec::with_capacity(keys.len());

                for x in keys {
                    let suffix = x[x.len() - 4..].to_string();
                    let open_ai_config = OpenAIConfig::new().with_api_key(x);
                    let open_ai_client = async_openai::Client::with_config(open_ai_config).with_http_client(client.clone());
                    clients.push((suffix, open_ai_client));
                }

                let tokio_rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
                Box::new(OpenAI { clients, tokio_rt})
            }
        }
    });

    translator.text_translate(input_text, dst_language)
}

struct Item {
    model_path: PathBuf,
    access_time: SystemTime,
    file_size: u64,
}

fn search_models(path: &Path) -> io::Result<(Vec<Item>, u64)> {
    let dir = std::fs::read_dir(path)?;
    let mut items = Vec::with_capacity(0);
    let mut total_model_size = 0;

    for res in dir {
        let entry = res?;

        if entry.file_type()?.is_file() {
            let md = entry.metadata()?;
            let access_time = md.accessed()?;
            let file_size = md.len();

            let item = Item {
                model_path: entry.path(),
                access_time,
                file_size,
            };

            items.push(item);
            total_model_size += file_size;
        }
    }
    Ok((items, total_model_size))
}

fn lock(#[allow(unused)] path: &Path) -> PyResult<NamedLockGuard> {
    static LOCK: OnceLock<NamedLock> = OnceLock::new();

    let lock = LOCK.get_or_init(|| {
        #[cfg(unix)]
        {
            let path = path.join("fooocus_lock");
            NamedLock::with_path(path).unwrap()
        }

        #[cfg(windows)]
        NamedLock::create("fooocus_lock").unwrap()
    });
    lock.lock().map_err(|e| PyTypeError::new_err(e.to_string()))
}

fn load_model(
    disk_path: &Path,
    memory_disk_path: &Path,
    model_parent_path: &Path,
    model_name: &str,
    memory_limit: u64,
) -> PyResult<bool> {
    println!("load model {}", model_name);
    let model_memory_parent_path = memory_disk_path.join(model_parent_path);
    let model_disk_parent_path = disk_path.join(model_parent_path);

    let model_disk_path = model_disk_parent_path.join(model_name);
    let model_memory_path = model_memory_parent_path.join(model_name);

    let mut update = false;

    // todo reading deleted files may occur
    if !model_memory_path.exists() {
        let _guard = lock(memory_disk_path)?;

        if !model_memory_path.exists() {
            let disk_model = std::fs::File::open(&model_disk_path)?;
            let disk_model_metadata = disk_model.metadata()?;

            if disk_model_metadata.len() > memory_limit {
                return Err(PyTypeError::new_err(format!("{} size > memory limit", model_name)));
            }

            update = true;
            let (mut models, models_size) = search_models(&model_memory_parent_path)?;

            if models_size + disk_model_metadata.len() > memory_limit {
                models.sort_unstable_by_key(|v| v.access_time);
                let mut remove_size = 0;

                while models_size + disk_model_metadata.len() - remove_size > memory_limit {
                    let item = models.pop().unwrap();
                    std::fs::remove_file(&item.model_path)?;
                    remove_size += item.file_size;
                }
            }

            println!("copy {} to {}", model_disk_path.to_str().unwrap(), model_memory_path.to_str().unwrap());
            std::fs::copy(&model_disk_path, &model_memory_path)?;
        }
    }

    let f = std::fs::File::options().write(true).open(&model_memory_path)?;
    let ft = FileTimes::new()
        .set_accessed(SystemTime::now());
    f.set_times(ft)?;
    Ok(update)
}

#[pyfunction]
fn load_checkpoint_model(model_name: &str) -> PyResult<bool> {
    if model_name.is_empty() || model_name == "None" {
        return Ok(false)
    }

    let config = &CONFIG.get().ok_or_else(|| PyTypeError::new_err("fooocus plugin is not initialized"))?.models_config;
    let memory_limit = config.checkpoints_memory_limit * 1024 * 1024 * 1024;
    load_model(&config.model_path, &config.model_memory_path, Path::new("checkpoints"), model_name, memory_limit)
}

#[pyfunction]
fn load_lora_models(models: Vec<(&str, f32)>) -> PyResult<bool> {
    let mut update = false;
    let config = &CONFIG.get().ok_or_else(|| PyTypeError::new_err("fooocus plugin is not initialized"))?.models_config;

    let total_size: u64 = models.iter()
        .map(|(model_name, _)| config.model_path.join(Path::new("loras")).join(model_name).metadata().unwrap().len())
        .sum();

    let memory_limit = config.loras_memory_limit * 1024 * 1024 * 1024;

    if total_size > memory_limit {
        return Err(PyTypeError::new_err("lora model size > memory limit"));
    }

    for (model_name, _) in &models {
        update |= load_model(&config.model_path, &config.model_memory_path, Path::new("loras"), model_name, memory_limit)?;
    }
    Ok(update)
}

fn copy_dir_all(src: impl AsRef<Path>, dst: impl AsRef<Path>) -> io::Result<()> {
    fs::create_dir_all(&dst)?;
    let src = src.as_ref();

    for entry in fs::read_dir(src).map_err(|e| std::io::Error::new(io::ErrorKind::Other, format!("failed to open {}, error: {:?}", src.to_str().unwrap(), e)))? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let dst = dst.as_ref().join(entry.file_name());

        if ty.is_dir() {
            copy_dir_all(entry.path(), dst)?;
        } else if !dst.exists() {
            fs::copy(entry.path(), dst).map_err(|e| std::io::Error::new(io::ErrorKind::Other, format!("copy file {} error: {:?}", entry.path().to_str().unwrap(), e)))?;
        }
    }
    Ok(())
}

fn copy_models(
    memory_path: &Path,
    disk_path: &Path,
    model_parent_path: &Path
) -> io::Result<()> {
    let from = disk_path.join(model_parent_path);
    let to = memory_path.join(model_parent_path);

    println!("copy {} to {}", from.to_str().unwrap(), to.to_str().unwrap());
    copy_dir_all(from, to)
}

fn create_dic(memory_path: &Path, model_parent_path: &Path) -> io::Result<()> {
    let to = memory_path.join(model_parent_path);
    fs::create_dir_all(&to)
}

#[pyfunction]
fn load_model_caches() -> PyResult<()> {
    let config = &CONFIG.get().ok_or_else(|| PyTypeError::new_err("fooocus plugin is not initialized"))?.models_config;
    fs::create_dir_all(&config.model_memory_path)?;

    create_dic(&config.model_memory_path, Path::new("checkpoints"))?;
    create_dic(&config.model_memory_path, Path::new("loras"))?;

    load_checkpoint_model("juggernautXL_version6Rundiffusion.safetensors")?;
    load_lora_models(vec![("sd_xl_offset_example-lora_1.0.safetensors", 0.0)])?;

    let _guard = lock(&config.model_memory_path)?;

    copy_models(&config.model_memory_path, &config.model_path, Path::new("embeddings"))?;
    copy_models(&config.model_memory_path, &config.model_path, Path::new("vae_approx"))?;
    copy_models(&config.model_memory_path, &config.model_path, Path::new("upscale_models"))?;
    copy_models(&config.model_memory_path, &config.model_path, Path::new("inpaint"))?;
    copy_models(&config.model_memory_path, &config.model_path, Path::new("controlnet"))?;
    copy_models(&config.model_memory_path, &config.model_path, Path::new("clip_vision"))?;
    copy_models(&config.model_memory_path, &config.model_path, Path::new("prompt_expansion/fooocus_expansion"))?;

    Ok(())
}

/// A Python module implemented in Rust.
#[pymodule]
fn fooocus_plugin(_py: Python, m: &PyModule) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(init, m)?)?;
    m.add_function(wrap_pyfunction!(service_register, m)?)?;
    m.add_function(wrap_pyfunction!(service_deregister, m)?)?;
    m.add_function(wrap_pyfunction!(text_translate, m)?)?;
    m.add_function(wrap_pyfunction!(load_checkpoint_model, m)?)?;
    m.add_function(wrap_pyfunction!(load_lora_models, m)?)?;
    m.add_function(wrap_pyfunction!(load_model_caches, m)?)?;

    Ok(())
}


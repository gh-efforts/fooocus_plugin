use std::fs::FileTimes;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

use nacos_sdk::api::constants;
use nacos_sdk::api::naming::{NamingServiceBuilder, ServiceInstance};
use nacos_sdk::api::naming::NamingService;
use nacos_sdk::api::props::ClientProps;
use named_lock::NamedLock;
use pyo3::exceptions::PyTypeError;
use pyo3::prelude::*;
use serde::Deserialize;
use ureq::Agent;

static CONFIG: OnceLock<Config> = OnceLock::new();
static NAMING_SERVICE: OnceLock<Box<dyn NamingService + Sync + Send + 'static>> = OnceLock::new();
static DEREGISTER_SERVICE: Mutex<Option<Box<dyn Fn() -> nacos_sdk::api::error::Result<()> + Sync + Send + 'static>>> = Mutex::new(None);
static CLIENT: OnceLock<Agent> = OnceLock::new();

#[derive(Deserialize)]
struct ServiceConfig {
    server_addr: String,
    username: String,
    password: String,
    namespace: String,
}

#[derive(Deserialize)]
struct TranslatorConfig {
    api: String,
}

#[derive(Deserialize)]
struct ModelsConfig {
    memory_limit: u64,
    memory_disk_path: PathBuf,
    base_model: PathBuf,
    lora_model: PathBuf
}

#[derive(Deserialize)]
struct Config {
    nacos_service_config: ServiceConfig,
    translator_config: TranslatorConfig,
    models_config: ModelsConfig
}

fn find_lan_addr() -> std::io::Result<IpAddr> {
    let socket = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    socket.connect("8.8.8.8:53")?;
    let addr = socket.local_addr()?;
    Ok(addr.ip())
}

fn init_inner(config_path: &str) {
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

#[pyfunction]
fn init(config_path: &str) {
    init_inner(config_path);
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

fn service_register_inner(
    instance_addr: SocketAddr,
    instance_name: String,
    metadata_json: &str,
) -> PyResult<()> {
    let service = NAMING_SERVICE.get_or_init(|| {
        let s = create_naming_service().unwrap();
        Box::new(s)
    });

    let instance = ServiceInstance {
        ip: instance_addr.ip().to_string(),
        port: instance_addr.port() as i32,
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
fn service_register(
    instance_port: u16,
    instance_name: String,
    metadata_json: &str,
) -> PyResult<()> {
    let instance_ip = find_lan_addr().map_err(|e| PyTypeError::new_err(e.to_string()))?;

    service_register_inner(
        SocketAddr::new(instance_ip, instance_port),
        instance_name,
        metadata_json,
    )
}

fn service_deregister_inner() -> PyResult<()> {
    match DEREGISTER_SERVICE.lock().unwrap().as_ref() {
        None => Err(PyTypeError::new_err("service has not been registered")),
        Some(v) => v().map_err(|e| PyTypeError::new_err(e.to_string()))
    }
}

#[pyfunction]
fn service_deregister() -> PyResult<()> {
    service_deregister_inner()
}

fn text_translate_inner(
    input_text: &str,
    dst_language: &str,
) -> PyResult<String> {
    let c = CLIENT.get_or_init(|| {
        ureq::Agent::new()
    });

    let config = &CONFIG.get().ok_or_else(|| PyTypeError::new_err("fooocus plugin is not initialized"))?.translator_config;

    let resp = c.post(&config.api)
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

#[pyfunction]
fn text_translate(
    input_text: &str,
    dst_language: &str,
) -> PyResult<String> {
    text_translate_inner(input_text, dst_language)
}

struct Item {
    model_path: PathBuf,
    access_time: SystemTime,
    file_size: u64
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
                file_size
            };

            items.push(item);
            total_model_size += file_size;
        }
    }
    Ok((items, total_model_size))
}

fn load_model_inner(model_type: &str, model_name: &str) -> PyResult<()> {
    static LOCK: OnceLock<NamedLock> = OnceLock::new();

    let config = &CONFIG.get().ok_or_else(|| PyTypeError::new_err("fooocus plugin is not initialized"))?.models_config;
    let memory_limit = config.memory_limit * 1024 * 1024 * 1024;

    let disk_models_dic_path = match model_type {
        "base" => &config.base_model,
        "lora" => &config.lora_model,
        _ => return Err(PyTypeError::new_err("unsupported model type"))
    };

    let memory_models_dic_path = config.memory_disk_path.join(disk_models_dic_path.file_name().unwrap().to_str().unwrap());

    let disk_model_path = disk_models_dic_path.join(model_name);
    let memory_model_path = memory_models_dic_path.join(model_name);

    if !memory_model_path.exists() {
        let lock = LOCK.get_or_init(|| {
            #[cfg(unix)]
            {
                let path = config.memory_disk_path.join("fooocus_lock");
                NamedLock::with_path(path).unwrap()
            }

            #[cfg(windows)]
            NamedLock::create("fooocus_lock").unwrap()
        });

        let _guard = lock.lock().map_err(|e| PyTypeError::new_err(e.to_string()))?;

        let disk_model = std::fs::File::open(&disk_model_path)?;
        let disk_model_metadata = disk_model.metadata()?;

        if disk_model_metadata.len() > memory_limit {
            return Err(PyTypeError::new_err(format!("{} size > memory limit", disk_model_path.file_name().unwrap().to_str().unwrap())));
        }

        let (mut models, models_size) = search_models(&memory_models_dic_path)?;

        if models_size + disk_model_metadata.len() > memory_limit {
            models.sort_unstable_by_key(|v| v.access_time);
            let mut remove_size = 0;

            while models_size + disk_model_metadata.len() - remove_size > memory_limit {
                let item= models.pop().unwrap();
                std::fs::remove_file(&item.model_path)?;
                remove_size += item.file_size;
            }
        }
        std::fs::copy(&disk_model_path, &memory_model_path)?;
    }

    let f = std::fs::File::options().write(true).open(&memory_model_path)?;
    let ft = FileTimes::new()
        .set_accessed(SystemTime::now());
    f.set_times(ft)?;
    Ok(())
}

#[pyfunction]
fn load_model(model_type: &str, model_name: &str) -> PyResult<()> {
    load_model_inner(model_type, model_name)
}

/// A Python module implemented in Rust.
#[pymodule]
fn fooocus_plugin(_py: Python, m: &PyModule) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(init, m)?)?;
    m.add_function(wrap_pyfunction!(service_register, m)?)?;
    m.add_function(wrap_pyfunction!(service_deregister, m)?)?;
    m.add_function(wrap_pyfunction!(text_translate, m)?)?;
    m.add_function(wrap_pyfunction!(load_model, m)?)?;

    Ok(())
}


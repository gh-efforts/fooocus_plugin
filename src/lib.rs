use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Mutex, OnceLock};

use nacos_sdk::api::constants;
use nacos_sdk::api::naming::{NamingServiceBuilder, ServiceInstance};
use nacos_sdk::api::naming::NamingService;
use nacos_sdk::api::props::ClientProps;
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
    server_addr: SocketAddr,
    username: String,
    password: String,
    namespace: String,
}

#[derive(Deserialize)]
struct TranslatorConfig {
    api: String,
}

#[derive(Deserialize)]
struct Config {
    nacos_service_config: ServiceConfig,
    translator_config: TranslatorConfig,
}

fn find_lan_addr() -> std::io::Result<IpAddr> {
    let socket = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    socket.connect("8.8.8.8")?;
    let addr = socket.local_addr()?;
    Ok(addr.ip())
}

fn init_inner(nacos_config_path: &str) {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    CONFIG.get_or_init(|| {
        let f = || {
            let c = std::fs::read_to_string(nacos_config_path)?;
            toml::from_str(&c).map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
        };
        f().unwrap()
    });
}

#[pyfunction]
fn init(nacos_config_path: &str) {
    init_inner(nacos_config_path);
}

fn create_naming_service() -> nacos_sdk::api::error::Result<impl NamingService> {
    let config = CONFIG
        .get()
        .ok_or_else(|| nacos_sdk::api::error::Error::ErrResult(String::from("fooocus plugin is not initialized")))?;

    let config = &config.nacos_service_config;

    NamingServiceBuilder::new(
        ClientProps::new()
            .server_addr(&config.server_addr.to_string())
            .auth_username(&config.username)
            .auth_password(&config.password)
            .namespace(&config.namespace)
    )
        .enable_auth_plugin_http()
        .build()
}

pub fn service_register_inner(
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
pub fn service_register(
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

pub fn service_deregister_inner() -> PyResult<()> {
    match DEREGISTER_SERVICE.lock().unwrap().as_ref() {
        None => Err(PyTypeError::new_err("service has not been registered")),
        Some(v) => v().map_err(|e| PyTypeError::new_err(e.to_string()))
    }
}

#[pyfunction]
pub fn service_deregister() -> PyResult<()> {
    service_deregister_inner()
}

pub fn text_translate_inner(
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
pub fn text_translate(
    input_text: &str,
    dst_language: &str,
) -> PyResult<String> {
    text_translate_inner(input_text, dst_language)
}

/// A Python module implemented in Rust.
#[pymodule]
fn fooocus_plugin(_py: Python, m: &PyModule) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(init, m)?)?;
    m.add_function(wrap_pyfunction!(service_register, m)?)?;
    m.add_function(wrap_pyfunction!(service_deregister, m)?)?;
    m.add_function(wrap_pyfunction!(text_translate, m)?)?;

    Ok(())
}


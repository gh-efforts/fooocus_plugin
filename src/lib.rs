use std::net::{IpAddr, SocketAddr};
use std::sync::OnceLock;

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
) -> PyResult<()> {
    let service = NAMING_SERVICE.get_or_init(|| {
        let s = create_naming_service().unwrap();
        Box::new(s)
    });

    let instance = ServiceInstance {
        ip: instance_addr.ip().to_string(),
        port: instance_addr.port() as i32,
        ..Default::default()
    };

    service.register_instance(
        instance_name,
        Some(constants::DEFAULT_GROUP.to_string()),
        instance,
    ).map_err(|e| PyTypeError::new_err(e.to_string()))
}

#[pyfunction]
pub fn service_register(
    instance_ip: IpAddr,
    instance_port: u16,
    instance_name: String,
) -> PyResult<()> {
    service_register_inner(SocketAddr::new(instance_ip, instance_port), instance_name)
}

pub fn service_deregister_inner(
    instance_addr: SocketAddr,
    instance_name: String,
) -> PyResult<()> {
    let service = NAMING_SERVICE.get_or_init(|| {
        let s = create_naming_service().unwrap();
        Box::new(s)
    });

    let instance = ServiceInstance {
        ip: instance_addr.ip().to_string(),
        port: instance_addr.port() as i32,
        ..Default::default()
    };

    service.deregister_instance(
        instance_name,
        Some(constants::DEFAULT_GROUP.to_string()),
        instance
    ).map_err(|e| PyTypeError::new_err(e.to_string()))
}

#[pyfunction]
pub fn service_deregister(
    instance_ip: IpAddr,
    instance_port: u16,
    instance_name: String,
) -> PyResult<()> {
    service_deregister_inner(SocketAddr::new(instance_ip, instance_port), instance_name)
}

pub fn text_translate_inner(
    other_language_text: &str
) -> PyResult<String> {
    let c = CLIENT.get_or_init(|| {
        ureq::Agent::new()
    });

    let config = &CONFIG.get().ok_or_else(|| PyTypeError::new_err("fooocus plugin is not initialized"))?.translator_config;

    let resp = c.post(&config.api)
        .send_json([
            ("q", other_language_text),
            ("source", "auto"),
            ("target", "en")
        ]).map_err(|e| PyTypeError::new_err(e.to_string()))?;

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
    other_language_text: &str
) -> PyResult<String> {
    text_translate_inner(other_language_text)
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


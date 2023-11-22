use std::net::{IpAddr, SocketAddr};
use std::sync::OnceLock;

use nacos_sdk::api::constants;
use nacos_sdk::api::naming::{NamingServiceBuilder, ServiceInstance};
use nacos_sdk::api::naming::NamingService;
use nacos_sdk::api::props::ClientProps;
use pyo3::prelude::*;
use serde::Deserialize;

static NACOS_SERVICE_CONFIG: OnceLock<ServiceConfig> = OnceLock::new();
static NAMING_SERVICE: OnceLock<Box<dyn NamingService + Sync + Send + 'static>> = OnceLock::new();

#[derive(Debug, Clone, Deserialize)]
struct ServiceConfig {
    server_addr: SocketAddr,
    username: String,
    password: String,
    namespace: String,
}

fn init_inner(nacos_config_path: &str) {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    NACOS_SERVICE_CONFIG.get_or_init(|| {
        let f = || {
            let c = std::fs::read_to_string(nacos_config_path)?;
            toml::from_str(&c).map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
        };
        f().expect("failed to read nacos service config")
    });
}

#[pyfunction]
fn init(nacos_config_path: &str) {
    init_inner(nacos_config_path);
}

pub fn service_register_inner(
    instance_addr: SocketAddr,
    instance_name: String,
) {
    NAMING_SERVICE.get_or_init(|| {
        let f = || {
            let config = NACOS_SERVICE_CONFIG
                .get()
                .ok_or_else(|| nacos_sdk::api::error::Error::ErrResult(String::from("fooocus plugin is not initialized")))?;

            let naming_service = NamingServiceBuilder::new(
                ClientProps::new()
                    .server_addr(&config.server_addr.to_string())
                    .auth_username(&config.username)
                    .auth_password(&config.password)
                    .namespace(&config.namespace)
            )
                .enable_auth_plugin_http()
                .build()?;

            // example naming register instances
            let service_instance1 = ServiceInstance {
                ip: instance_addr.ip().to_string(),
                port: instance_addr.port() as i32,
                ..Default::default()
            };

            naming_service.register_instance(
                instance_name.clone(),
                Some(constants::DEFAULT_GROUP.to_string()),
                service_instance1,
            )?;

            nacos_sdk::api::error::Result::Ok(Box::new(naming_service))
        };
        f().expect(&format!("failed to register {}", instance_name))
    });
}

#[pyfunction]
pub fn service_register(
    instance_ip: IpAddr,
    instance_port: u16,
    instance_name: String,
) {
    service_register_inner(SocketAddr::new(instance_ip, instance_port), instance_name);
}

/// A Python module implemented in Rust.
#[pymodule]
fn fooocus_plugin(_py: Python, m: &PyModule) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(init, m)?)?;
    m.add_function(wrap_pyfunction!(service_register, m)?)?;
    Ok(())
}


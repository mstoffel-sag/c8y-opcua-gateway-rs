//! Everything this gateway says on localhost: the thin-edge MQTT broker and the Cumulocity proxy.
//!
//! Both are thin-edge endpoints under thin-edge's auth model, which is why they live in one crate.
//! The gateway holds no cloud credentials: readings go out over MQTT and thin-edge's mapper and
//! bridge carry them to the cloud, while device types come back in over the proxy, which injects
//! the device's JWT for us.
#![forbid(unsafe_code)]

pub mod mqtt;
pub mod proxy;
pub mod topic;

pub use mqtt::{IncomingMessage, MqttConfig, TedgeMqtt};
pub use proxy::{C8yProxy, ProxyConfig, ProxyError};
pub use topic::EntityTopicId;

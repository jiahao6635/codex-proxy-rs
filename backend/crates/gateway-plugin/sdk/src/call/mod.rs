//! 按业务职责组织的跨进程调用数据，不依赖网关领域类型。

pub mod auth;
pub mod frontend_authentication;
pub mod host;
pub mod management;
pub mod middleware;
pub mod observation;
pub mod policy;
pub mod provider;

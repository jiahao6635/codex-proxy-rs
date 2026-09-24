//! 受管 HTTP 资源；调用授权属于上层，Host 负责出站、容量、期限和流回收。

mod connector;
mod http;
mod network;

pub use http::{HttpBody, HttpClient, HttpError, HttpErrorKind, HttpRequest, HttpResponse};
pub use network::{DnsResolver, NetworkPolicy};

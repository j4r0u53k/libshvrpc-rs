pub mod framerw;
pub mod client;
pub mod metamethod;
pub mod rpc;
pub mod rpctype;
pub mod rpcframe;
pub mod rpcmessage;
pub mod serialrw;
pub mod streamrw;
pub mod util;

pub use rpcmessage::{RpcMessage, RpcMessageMetaTags};
pub use shvproto::{self, RpcValue};

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = std::result::Result<T, Error>;


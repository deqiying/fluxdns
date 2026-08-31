//! 可热替换的资源解析与不可变索引。

mod hosts;

pub use hosts::{
    CanonicalDomain, HostsIndex, HostsLimits, HostsLookup, HostsParseError, HostsRecord,
};

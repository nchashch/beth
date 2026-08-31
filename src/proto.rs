//! Generated protobuf/gRPC bindings for the CUSF BIP300/301 enforcer's `ValidatorService`.
//!
//! See `build.rs` for how these are compiled from the enforcer's `.proto` sources.

pub mod cusf {
    pub mod common {
        pub mod v1 {
            tonic::include_proto!("cusf.common.v1");
        }
    }
    pub mod mainchain {
        pub mod v1 {
            tonic::include_proto!("cusf.mainchain.v1");
        }
    }
}

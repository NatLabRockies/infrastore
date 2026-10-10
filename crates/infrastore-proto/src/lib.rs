//! Generated protobuf types and conversions to/from `infrastore-core`.

// tonic-build marks its async_trait methods `#[must_use]`, which clippy 1.99 flags as
// `double_must_use`; the code is generated, so the lint is silenced here.
#[allow(clippy::double_must_use)]
pub mod pb {
    tonic::include_proto!("infrastore.v1");
}

pub mod convert;

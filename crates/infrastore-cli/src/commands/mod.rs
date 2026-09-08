//! CLI command handlers.

pub mod add;
pub mod admin;
pub mod assoc;
pub mod diff;
pub mod discover;
pub mod export;
pub mod grid;
pub mod manage;
pub mod plot;
pub mod show;

/// What a build without the `parquet` cargo feature says when asked for it.
///
/// Shared by `add --parquet` and `-f parquet export` rather than written twice:
/// the two used to drift, and one of them stopped saying anything at all. Only
/// a lean build has anywhere to say it.
#[cfg(not(feature = "parquet"))]
pub fn without_parquet() -> String {
    "this infrastore was built without Parquet support; rebuild with \
     `cargo install infrastore-cli --features parquet`"
        .to_string()
}

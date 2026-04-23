//! Built-in search providers

#[cfg(feature = "serpapi")]
pub mod serpapi;

#[cfg(feature = "serpapi")]
pub use serpapi::SerpApiProvider;

//! Backend-neutral clipboard types.
//!
//! These types model clipboard offers, generation tracking, MIME validation,
//! and capture-size policy without coupling to any specific display server
//! protocol.

mod capture;
mod content;
mod feedback;
mod mime;
mod protocol;

pub use capture::*;
pub use content::*;
pub use feedback::*;
pub use mime::*;
pub use protocol::*;

#[cfg(test)]
mod tests;

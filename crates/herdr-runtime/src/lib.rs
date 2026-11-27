//! Backend-agnostic agent runtime abstraction.
//!
//! Implements RFC `docs/hc-migration/00-rfc.md` §2: the [`AgentRuntime`]
//! trait is the single seam every pool component (CLI, router daemon,
//! delivery/rescue/nudge workers, registry sync) will talk to, so the same
//! code can run on the Herdr TUI socket API today and on hc/herdr-core
//! later. This crate lands the trait **dark** — no call sites are ported
//! yet; [`HerdrRuntime`] wraps [`herdr_protocol::HerdrClient`] as impl #1.
//!
//! Sync only: std threads + channels, matching the workspace rule. All
//! methods return pool-domain types ([`RuntimeAgent`], [`SendOutcome`],
//! [`RuntimeEvent`], ...) — never backend wire types.

// No `unwrap()`/`expect()` in library code; integration tests live in
// `tests/` (separate crates, unaffected by this lint).
#![deny(clippy::unwrap_used, clippy::expect_used)]

pub mod error;
pub mod herdr;
pub mod types;

pub use error::RuntimeError;
pub use herdr::HerdrRuntime;
pub use types::{
    AgentRuntime, DeferReason, EnvLookup, EventStream, RuntimeAgent, RuntimeCaps, RuntimeEvent,
    RuntimeStatus, SelfIdentity, SendFailureKind, SendOutcome, WaitEnd, WaitOutcome, WaitRequest,
};

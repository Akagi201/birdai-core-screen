//! Checkpoint-driven in-memory venue state.
//!
//! See [`manager`] for the boundary between a checkpoint's raw objects and typed venue state, the
//! three hard problems (new pools, dynamic-field churn, package upgrades), and what changes when
//! the objects come from inside a validator rather than from the checkpoint stream.
//!
//! # Example
//!
//! ```ignore
//! let layouts = birdai_resolve::rpc_layout_registry(RPC);
//! let manager = StateManager::new();
//!
//! let mut service = IngestionService::new(args, config, Some("birdai"), &registry)?;
//! let mut rx = service.subscribe_bounded(8);
//! let _service = service.run(start..).await?;
//!
//! while let Some(envelope) = rx.recv().await {
//!     let report = manager.apply_checkpoint(&layouts, &envelope.checkpoint).await?;
//!     tracing::info!(venues = report.venues_updated, "applied a checkpoint");
//! }
//! ```

pub mod error;
pub mod manager;

pub use error::StateError;
pub use manager::{ManagerStats, StateManager, UpdateReport, VenueSlot, group_children};

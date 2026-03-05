//! Epoch-based safe memory reclamation.
//!
//! Implements FASTER's custom epoch framework for coordinating concurrent
//! access to shared data structures without locks. Provides drain-list
//! semantics and checkpoint integration.

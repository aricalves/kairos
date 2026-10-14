//! The `kairos` command tree over `kairos-client` (KAIROS-T-0037, per
//! KAIROS-A-0015): one module per noun, all thin veneers — flag plumbing
//! and rendering only, no business logic.

pub mod admin;
pub mod boards;
pub mod columns;
pub mod dispatch;
pub mod entities;
pub mod grants;
pub mod keys;
pub mod members;
pub mod meta;
pub mod orgs;
pub mod pages;
pub mod repos;
pub mod search;
pub mod service_accounts;
pub mod streams;
pub mod team_pages;
pub mod teams;

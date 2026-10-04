//! rbb — a fast, scalable forum engine inspired by MyBB.
//!
//! The library holds the whole application; the `rbb` binary is a thin command-line front end.

pub mod admin;
pub mod app;
pub mod assets;
pub mod audit;
pub mod auth;
pub mod automod;
pub mod badges;
pub mod cache;
pub mod config;
pub mod ctx;
pub mod debugbar;
pub mod doctor;
pub mod domain;
pub mod error;
pub mod fuzzing;
pub mod i18n;
pub mod import;
pub mod infra;
pub mod install;
pub mod mail;
pub mod models;
pub mod notify;
pub mod ops;
pub mod pagecache;
pub mod parser;
pub mod passkeys;
pub mod perms;
pub mod pgp;
pub mod plugins;
pub mod posting;
pub mod privacy;
pub mod render;
pub mod routes;
pub mod seed;
pub mod server;
pub mod settings;
pub mod system;
pub mod tasks;
pub mod templates;
pub mod usecase;
pub mod util;
